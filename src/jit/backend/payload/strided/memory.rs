use super::*;
use cranelift_codegen::ir::{AliasRegionData, AliasRegionSet, GlobalValueData, MemFlags};

const HEADER: u32 = 0x4c554e48;
const OTHER: u32 = 0x4c554e56;
const TABLE_BYTES: usize = 16 * 1024;

#[cfg(test)]
mod tests;

fn regions() -> AliasRegionSet {
    let mut regions = AliasRegionSet::new();
    for (user_id, description) in [
        (HEADER, "strided frame metadata"),
        (OTHER, "strided other memory"),
    ] {
        regions.insert(AliasRegionData {
            user_id,
            description: description.into(),
        });
    }
    regions
}

fn metadata(
    function: &Function,
    slots: IrValue,
    reads: &[Read],
    allocator: BudgetAllocator,
) -> Result<BudgetVec<bool, BudgetAllocator>, JitError> {
    let mut metadata = BudgetVec::new_in(allocator);
    metadata
        .try_reserve_exact(function.dfg.num_insts())
        .map_err(|_| JitError::ResourceLimit("strided metadata records"))?;
    metadata.resize(function.dfg.num_insts(), false);
    for &read in reads {
        super::verify(function, slots, read)?;
        let mut at = Some(read.inst);
        for _ in 0..=read.extra() {
            let inst = at.ok_or_else(invalid)?;
            if function.dfg.insts[inst].opcode().can_load() {
                metadata[inst.as_u32() as usize] = true;
            }
            at = function.layout.prev_inst(inst);
        }
    }
    Ok(metadata)
}

fn verify(
    source: &Function,
    candidate: &mut Function,
    slots: IrValue,
    reads: &[Read],
    allocator: BudgetAllocator,
) -> Result<(), JitError> {
    if candidate.dfg.num_insts() != source.dfg.num_insts()
        || candidate.dfg.alias_regions != regions()
    {
        return Err(invalid());
    }
    let metadata = metadata(source, slots, reads, allocator.clone())?;
    let mut restored = BudgetVec::new_in(allocator);
    restored
        .try_reserve_exact(source.dfg.num_insts())
        .map_err(|_| JitError::ResourceLimit("strided memory verification records"))?;
    for index in 0..source.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let before = &source.dfg.insts[inst];
        match (before.memflags(), candidate.dfg.insts[inst].memflags()) {
            (Some(old), Some(new)) => {
                let expected = if before.opcode().can_load() || before.opcode().can_store() {
                    let region = candidate
                        .dfg
                        .alias_regions
                        .get(if metadata[index] { HEADER } else { OTHER })
                        .ok_or_else(invalid)?;
                    source.dfg.mem_flags[old].with_alias_region(Some(region))
                } else {
                    source.dfg.mem_flags[old]
                };
                if !candidate.dfg.mem_flags.is_valid(new)
                    || candidate.dfg.mem_flags[new] != expected
                {
                    return Err(invalid());
                }
                restored.push((inst, new));
            }
            (None, None) => {}
            _ => return Err(invalid()),
        }
    }
    for &(inst, _) in &restored {
        *candidate.dfg.insts[inst]
            .memflags_mut()
            .ok_or_else(invalid)? = source.dfg.insts[inst].memflags().ok_or_else(invalid)?;
    }
    let flags = std::mem::replace(&mut candidate.dfg.mem_flags, source.dfg.mem_flags.clone());
    let regions = std::mem::replace(
        &mut candidate.dfg.alias_regions,
        source.dfg.alias_regions.clone(),
    );
    let preserved = *source == *candidate;
    candidate.dfg.mem_flags = flags;
    candidate.dfg.alias_regions = regions;
    for (inst, flags) in restored {
        *candidate.dfg.insts[inst]
            .memflags_mut()
            .ok_or_else(invalid)? = flags;
    }
    if preserved {
        Ok(())
    } else {
        Err(invalid())
    }
}

pub(super) fn lower(
    function: &mut Function,
    slots: IrValue,
    reads: &[Read],
    allocator: BudgetAllocator,
) -> Result<Reservation, JitError> {
    if function.dfg.alias_regions.len() != 0
        || function.dfg.mem_flags.is_valid(MemFlags::from_bits(32))
        || function
            .global_values
            .values()
            .any(|value| matches!(value, GlobalValueData::Load { .. }))
    {
        return Err(invalid());
    }
    let metadata = metadata(function, slots, reads, allocator.clone())?;
    let charge = Reservation::new(allocator.0.clone(), TABLE_BYTES)
        .map_err(|_| JitError::ResourceLimit("strided memory tables"))?;
    let mut candidate = function.clone();
    candidate.dfg.alias_regions = regions();
    for index in 0..function.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let before = &function.dfg.insts[inst];
        let Some(old) = before.memflags() else {
            continue;
        };
        if !function.dfg.mem_flags.is_valid(old)
            || function.dfg.mem_flags[old].alias_region().is_some()
        {
            return Err(invalid());
        }
        if before.opcode().can_load() || before.opcode().can_store() {
            let region = candidate
                .dfg
                .alias_regions
                .get(if metadata[index] { HEADER } else { OTHER })
                .ok_or_else(invalid)?;
            let flags = candidate
                .dfg
                .mem_flags
                .insert(function.dfg.mem_flags[old].with_alias_region(Some(region)))
                .map_err(|_| JitError::ResourceLimit("strided memory flags"))?;
            *candidate.dfg.insts[inst]
                .memflags_mut()
                .ok_or_else(invalid)? = flags;
        }
    }
    drop(metadata);
    verify(function, &mut candidate, slots, reads, allocator)?;
    *function = candidate;
    Ok(charge)
}
