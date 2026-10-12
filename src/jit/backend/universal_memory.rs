use cranelift_codegen::ir::{
    AliasRegionData, Function, GlobalValueData, Inst, MemFlags, MemFlagsData,
};

use super::{BudgetAllocator, BudgetVec, JitError, Reservation};
use crate::jit::work::Expansion;

const REGION_ID: u32 = 0x4c554e41;
const DESCRIPTION: &str = "all Luna native memory";
const MAX_FLAGS: usize = 32;
const TABLE_BYTES: usize = 16 * 1024;

fn invalid() -> JitError {
    JitError::Compilation("universal memory translation".into())
}

fn verify(
    source: &Function,
    candidate: &mut Function,
    allocator: BudgetAllocator,
) -> Result<(), JitError> {
    let region = candidate
        .dfg
        .alias_regions
        .get(REGION_ID)
        .ok_or_else(invalid)?;
    if candidate.dfg.alias_regions.len() != 1
        || candidate.dfg.alias_regions[region].description != DESCRIPTION
        || candidate.dfg.num_insts() != source.dfg.num_insts()
    {
        return Err(invalid());
    }
    let mut records = BudgetVec::<(Inst, MemFlags), _>::new_in(allocator);
    for index in 0..source.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let before = &source.dfg.insts[inst];
        match (before.memflags(), candidate.dfg.insts[inst].memflags()) {
            (Some(old), Some(new)) => {
                let expected = if before.opcode().can_load() || before.opcode().can_store() {
                    source.dfg.mem_flags[old].with_alias_region(Some(region))
                } else {
                    source.dfg.mem_flags[old]
                };
                if !candidate.dfg.mem_flags.is_valid(new)
                    || candidate.dfg.mem_flags[new] != expected
                {
                    return Err(invalid());
                }
                records.try_reserve(1).map_err(|_| {
                    JitError::ResourceLimit("universal memory verification records")
                })?;
                records.push((inst, new));
                *candidate.dfg.insts[inst]
                    .memflags_mut()
                    .ok_or_else(invalid)? = old;
            }
            (None, None) => {}
            _ => return Err(invalid()),
        }
    }
    let flags = std::mem::replace(&mut candidate.dfg.mem_flags, source.dfg.mem_flags.clone());
    let regions = std::mem::replace(
        &mut candidate.dfg.alias_regions,
        source.dfg.alias_regions.clone(),
    );
    let preserved = *candidate == *source;
    candidate.dfg.mem_flags = flags;
    candidate.dfg.alias_regions = regions;
    for (inst, flags) in records {
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
    allocator: BudgetAllocator,
    expansion: Expansion,
) -> Result<Reservation, JitError> {
    expansion.verify_actual(function.dfg.num_insts(), function.layout.blocks().count())?;
    if function.dfg.alias_regions.len() != 0 {
        return Err(invalid());
    }
    if function
        .dfg
        .mem_flags
        .is_valid(MemFlags::from_bits(MAX_FLAGS as u32))
    {
        return Err(JitError::ResourceLimit("universal memory source flags"));
    }
    if function
        .global_values
        .values()
        .any(|value| matches!(value, GlobalValueData::Load { .. }))
    {
        return Err(invalid());
    }
    for index in 0..function.dfg.num_insts() {
        let inst = &function.dfg.insts[Inst::from_u32(index as u32)];
        let Some(flags) = inst.memflags() else {
            if inst.opcode().can_load() && !inst.opcode().is_call() {
                return Err(invalid());
            }
            continue;
        };
        if !function.dfg.mem_flags.is_valid(flags) {
            return Err(invalid());
        }
        let flags = function.dfg.mem_flags[flags];
        if flags.alias_region().is_some() {
            return Err(invalid());
        }
    }
    let charge = Reservation::new(allocator.0.clone(), TABLE_BYTES)
        .map_err(|_| JitError::ResourceLimit("universal memory tables"))?;
    let mut candidate = function.clone();
    let region = candidate.dfg.alias_regions.insert(AliasRegionData {
        user_id: REGION_ID,
        description: DESCRIPTION.into(),
    });
    for index in 0..function.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let before = &function.dfg.insts[inst];
        if !before.opcode().can_load() && !before.opcode().can_store() {
            continue;
        }
        if let Some(flags) = before.memflags() {
            let data = function.dfg.mem_flags[flags].with_alias_region(Some(region));
            let new = candidate
                .dfg
                .mem_flags
                .insert(data)
                .map_err(|_| JitError::ResourceLimit("universal memory flags"))?;
            *candidate.dfg.insts[inst]
                .memflags_mut()
                .ok_or_else(invalid)? = new;
        }
    }
    verify(function, &mut candidate, allocator)?;
    *function = candidate;
    Ok(charge)
}

mod tests;
