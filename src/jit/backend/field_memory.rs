use cranelift_codegen::ir::{
    types, AliasRegionData, AliasRegionSet, Function, GlobalValueData, Inst, InstructionData,
    MemFlags, Opcode, Value,
};

use super::{BudgetAllocator, BudgetVec, CompileError, Reservation};
use crate::jit::work::Expansion;

const FIRST_FIELD: u32 = 0x4c550000;
const EXIT_REGION: u32 = FIRST_FIELD + 512;
const DESCRIPTION: &str = "Luna scratch field or exit";

fn invalid() -> CompileError {
    CompileError::Compilation("field memory translation".into())
}

#[derive(Clone, Copy)]
struct Roots {
    slots: Value,
    exit: Value,
    registers: usize,
}

fn classify(function: &Function, inst: Inst, roots: Roots) -> Result<Option<u32>, CompileError> {
    let data = &function.dfg.insts[inst];
    let opcode = data.opcode();
    if !opcode.can_load() && !opcode.can_store() || opcode.is_call() {
        return Ok(None);
    }
    let (pointer, offset, ty) = match *data {
        InstructionData::Load {
            opcode: Opcode::Load,
            arg,
            offset,
            ..
        } => (
            arg,
            i32::from(offset),
            function.dfg.value_type(function.dfg.first_result(inst)),
        ),
        InstructionData::Store {
            opcode: Opcode::Store,
            args,
            offset,
            ..
        } => (args[1], i32::from(offset), function.dfg.value_type(args[0])),
        _ => return Err(invalid()),
    };
    let pointer = function.dfg.resolve_aliases(pointer);
    if pointer == roots.slots {
        if !matches!(ty, types::I64 | types::F64)
            || offset < 0
            || offset % 8 != 0
            || offset as usize / 16 >= roots.registers
        {
            return Err(invalid());
        }
        Ok(Some(FIRST_FIELD + offset as u32 / 8))
    } else if pointer == roots.exit {
        if opcode != Opcode::Store
            || !matches!(
                (offset, ty),
                (0, types::I64) | (8, types::I32) | (12, types::I32)
            )
        {
            return Err(invalid());
        }
        Ok(Some(EXIT_REGION))
    } else {
        Err(invalid())
    }
}

fn verify(
    source: &Function,
    candidate: &mut Function,
    roots: Roots,
    allocator: BudgetAllocator,
) -> Result<(), CompileError> {
    if candidate.dfg.num_insts() != source.dfg.num_insts() {
        return Err(invalid());
    }
    let mut expected_regions = AliasRegionSet::new();
    let mut records = BudgetVec::<(Inst, MemFlags), _>::new_in(allocator);
    for index in 0..source.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let region = classify(source, inst, roots)?.map(|user_id| {
            expected_regions.insert(AliasRegionData {
                user_id,
                description: DESCRIPTION.into(),
            })
        });
        match (
            source.dfg.insts[inst].memflags(),
            candidate.dfg.insts[inst].memflags(),
        ) {
            (Some(old), Some(new)) => {
                let expected = source.dfg.mem_flags[old].with_alias_region(region);
                if !candidate.dfg.mem_flags.is_valid(new)
                    || candidate.dfg.mem_flags[new] != expected
                {
                    return Err(invalid());
                }
                records
                    .try_reserve(1)
                    .map_err(|_| CompileError::ResourceLimit("field memory records"))?;
                records.push((inst, new));
                *candidate.dfg.insts[inst]
                    .memflags_mut()
                    .ok_or_else(invalid)? = old;
            }
            (None, None) if region.is_none() => {}
            _ => return Err(invalid()),
        }
    }
    if candidate.dfg.alias_regions != expected_regions {
        return Err(invalid());
    }
    let flags = std::mem::replace(&mut candidate.dfg.mem_flags, source.dfg.mem_flags.clone());
    let regions = std::mem::replace(
        &mut candidate.dfg.alias_regions,
        source.dfg.alias_regions.clone(),
    );
    let preserved = *source == *candidate;
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
    slots: Value,
    exit: Value,
    registers: usize,
    allocator: BudgetAllocator,
    expansion: Expansion,
) -> Result<Reservation, CompileError> {
    expansion.verify_actual(function.dfg.num_insts(), function.layout.blocks().count())?;
    let roots = Roots {
        slots: function.dfg.resolve_aliases(slots),
        exit: function.dfg.resolve_aliases(exit),
        registers,
    };
    if roots.slots == roots.exit
        || registers > 256
        || function.dfg.alias_regions.len() != 0
        || function
            .global_values
            .values()
            .any(|v| matches!(v, GlobalValueData::Load { .. }))
    {
        return Err(invalid());
    }
    if function.dfg.mem_flags.is_valid(MemFlags::from_bits(32)) {
        return Err(CompileError::ResourceLimit("field memory source flags"));
    }
    let mut accesses = 0usize;
    for index in 0..function.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        if let Some(flags) = function.dfg.insts[inst].memflags() {
            if !function.dfg.mem_flags.is_valid(flags)
                || function.dfg.mem_flags[flags].alias_region().is_some()
            {
                return Err(invalid());
            }
        }
        accesses += usize::from(classify(function, inst, roots)?.is_some());
    }
    let bytes = accesses
        .checked_mul(384)
        .and_then(|n| n.checked_add(16 * 1024))
        .ok_or(CompileError::ResourceLimit("field memory table size"))?;
    let charge = Reservation::new(allocator.0.clone(), bytes)
        .map_err(|_| CompileError::ResourceLimit("field memory tables"))?;
    let mut candidate = function.clone();
    for index in 0..function.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let Some(user_id) = classify(function, inst, roots)? else {
            continue;
        };
        let region = candidate.dfg.alias_regions.insert(AliasRegionData {
            user_id,
            description: DESCRIPTION.into(),
        });
        let old = function.dfg.insts[inst].memflags().ok_or_else(invalid)?;
        let flags = candidate
            .dfg
            .mem_flags
            .insert(function.dfg.mem_flags[old].with_alias_region(Some(region)))
            .map_err(|_| CompileError::ResourceLimit("field memory flags"))?;
        *candidate.dfg.insts[inst]
            .memflags_mut()
            .ok_or_else(invalid)? = flags;
    }
    verify(function, &mut candidate, roots, allocator)?;
    *function = candidate;
    Ok(charge)
}

mod tests;
