use super::*;
use cranelift_codegen::ir::{SigRef, ValueDef};

#[derive(Clone, Copy)]
struct Site {
    table: Inst,
    target: Inst,
    call: Inst,
    index: usize,
    signature: SigRef,
}

pub(super) type Targets = [FuncRef; 12];

fn target_index(offset: i32) -> Option<usize> {
    (0..12).find(|&index| offset == target_offset(index))
}

fn target_offset(index: usize) -> i32 {
    match index {
        0..10 => {
            (std::mem::offset_of!(Functions, helpers)
                + index * std::mem::size_of::<abi::HelperEntry>()) as i32
        }
        10 => std::mem::offset_of!(Functions, write) as i32,
        11 => std::mem::offset_of!(Functions, terminal) as i32,
        _ => unreachable!(),
    }
}

fn definition(f: &Function, value: IrValue) -> Result<Inst, JitError> {
    match f.dfg.value_def(value) {
        ValueDef::Result(inst, 0) if f.dfg.inst_results(inst) == [value] => Ok(inst),
        _ => Err(invalid()),
    }
}

fn load(f: &Function, inst: Inst) -> Result<(IrValue, i32), JitError> {
    match f.dfg.insts[inst] {
        InstructionData::Load {
            opcode: Opcode::Load,
            arg,
            flags,
            offset,
        } if f.dfg.mem_flags[flags] == MemFlagsData::new()
            && f.dfg.inst_results(inst).len() == 1
            && f.dfg.value_type(f.dfg.first_result(inst)) == types::I64 =>
        {
            Ok((arg, i32::from(offset)))
        }
        _ => Err(invalid()),
    }
}

fn signature(f: &Function, reference: SigRef, index: usize) -> Result<(), JitError> {
    let signature = &f.dfg.signatures[reference];
    let parameters: &[cranelift_codegen::ir::Type] = if index < 10 {
        &[
            types::I64,
            types::I64,
            types::I32,
            types::I32,
            types::I32,
            types::I32,
        ]
    } else {
        &[types::I64, types::I64, types::I32, types::I64, types::I64]
    };
    if signature.call_conv != f.signature.call_conv
        || signature.params.len() != parameters.len()
        || !signature
            .params
            .iter()
            .zip(parameters)
            .all(|(actual, &ty)| *actual == AbiParam::new(ty))
        || if index < 10 {
            signature.returns != [AbiParam::new(types::I32)]
        } else {
            !signature.returns.is_empty()
        }
    {
        return Err(invalid());
    }
    Ok(())
}

fn site(f: &Function, host: IrValue, call: Inst, targets: &Targets) -> Result<Site, JitError> {
    let InstructionData::CallIndirect {
        opcode: Opcode::CallIndirect,
        sig_ref,
        ..
    } = f.dfg.insts[call]
    else {
        return Err(invalid());
    };
    let args = f.dfg.inst_args(call);
    let (&callee, args) = args.split_first().ok_or_else(invalid)?;
    if args.first() != Some(&host) {
        return Err(invalid());
    }
    let target = definition(f, callee)?;
    let (table_value, offset) = load(f, target)?;
    let index = target_index(offset).ok_or_else(invalid)?;
    let table = definition(f, table_value)?;
    if load(f, table)? != (host, std::mem::offset_of!(Bridge, functions) as i32)
        || f.layout.inst_block(table) != f.layout.inst_block(call)
        || f.layout.inst_block(target) != f.layout.inst_block(call)
        || f.layout.next_inst(table) != Some(target)
        || f.dfg.signatures[sig_ref] != f.dfg.signatures[f.dfg.ext_funcs[targets[index]].signature]
    {
        return Err(invalid());
    }
    signature(f, sig_ref, index)?;
    if args.len() != f.dfg.signatures[sig_ref].params.len() {
        return Err(invalid());
    }
    let mut next = f.layout.next_inst(target);
    while next != Some(call) {
        let inst = next.ok_or_else(invalid)?;
        if f.dfg.insts[inst].opcode() != Opcode::Iconst {
            return Err(invalid());
        }
        next = f.layout.next_inst(inst);
    }
    Ok(Site {
        table,
        target,
        call,
        index,
        signature: sig_ref,
    })
}

fn verify(
    source: &Function,
    candidate: &Function,
    sites: &[Site],
    targets: &Targets,
) -> Result<(), JitError> {
    if source.signature != candidate.signature
        || source.params != candidate.params
        || source.sized_stack_slots != candidate.sized_stack_slots
        || source.dfg.signatures != candidate.dfg.signatures
        || source.dfg.ext_funcs != candidate.dfg.ext_funcs
        || !source.layout.blocks().eq(candidate.layout.blocks())
    {
        return Err(invalid());
    }
    let mut current = 0;
    for block in source.layout.blocks() {
        if source.dfg.block_params(block) != candidate.dfg.block_params(block) {
            return Err(invalid());
        }
        let mut actual = candidate.layout.block_insts(block);
        for original in source.layout.block_insts(block) {
            let replacement = sites.get(current);
            if replacement.is_some_and(|s| original == s.table || original == s.target) {
                continue;
            }
            if actual.next() != Some(original)
                || source.dfg.inst_results(original) != candidate.dfg.inst_results(original)
            {
                return Err(invalid());
            }
            if let Some(s) = replacement.filter(|s| original == s.call) {
                if !matches!(candidate.dfg.insts[original], InstructionData::Call {opcode: Opcode::Call, func_ref, ..} if func_ref == targets[s.index])
                    || candidate.dfg.inst_args(original) != &source.dfg.inst_args(original)[1..]
                    || candidate.dfg.ext_funcs[targets[s.index]].signature != s.signature
                        && candidate.dfg.signatures
                            [candidate.dfg.ext_funcs[targets[s.index]].signature]
                            != source.dfg.signatures[s.signature]
                {
                    return Err(invalid());
                }
                current += 1;
            } else {
                if source.dfg.insts[original] != candidate.dfg.insts[original]
                    || source.dfg.inst_args(original) != candidate.dfg.inst_args(original)
                {
                    return Err(invalid());
                }
                let left = source.dfg.insts[original]
                    .branch_destination(&source.dfg.jump_tables, &source.dfg.exception_tables);
                let right = candidate.dfg.insts[original].branch_destination(
                    &candidate.dfg.jump_tables,
                    &candidate.dfg.exception_tables,
                );
                if left.len() != right.len()
                    || left.iter().zip(right).any(|(left, right)| {
                        left.block(&source.dfg.value_lists)
                            != right.block(&candidate.dfg.value_lists)
                            || !left
                                .args(&source.dfg.value_lists)
                                .eq(right.args(&candidate.dfg.value_lists))
                    })
                {
                    return Err(invalid());
                }
                if let InstructionData::Load { flags, .. } | InstructionData::Store { flags, .. } =
                    source.dfg.insts[original]
                {
                    if source.dfg.mem_flags[flags] != candidate.dfg.mem_flags[flags] {
                        return Err(invalid());
                    }
                }
            }
            for &arg in candidate.dfg.inst_args(original) {
                if source.dfg.resolve_aliases(arg) != candidate.dfg.resolve_aliases(arg)
                    || source.dfg.value_type(arg) != candidate.dfg.value_type(arg)
                {
                    return Err(invalid());
                }
            }
        }
        if actual.next().is_some() {
            return Err(invalid());
        }
    }
    if current != sites.len()
        || size(candidate) != (size(source).0 - 2 * sites.len(), size(source).1)
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn lower(
    f: &mut Function,
    host: IrValue,
    targets: &Targets,
    allocator: BudgetAllocator,
) -> Result<usize, JitError> {
    let calls = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::CallIndirect)
        .count();
    if calls == 0 {
        return Ok(0);
    }
    let mut sites = BudgetVec::new_in(allocator);
    sites
        .try_reserve_exact(calls)
        .map_err(|_| JitError::ResourceLimit("payload direct calls"))?;
    for block in f.layout.blocks() {
        for inst in f.layout.block_insts(block) {
            if f.dfg.insts[inst].opcode() == Opcode::CallIndirect {
                sites.push(site(f, host, inst, targets)?);
            }
        }
    }
    let mut uses = BudgetVec::new_in(sites.allocator().clone());
    uses.try_reserve_exact(f.dfg.num_values())
        .map_err(|_| JitError::ResourceLimit("payload direct uses"))?;
    uses.resize(f.dfg.num_values(), None);
    for s in &sites {
        for (inst, user) in [(s.table, s.target), (s.target, s.call)] {
            let index = f.dfg.first_result(inst).as_u32() as usize;
            if uses[index].replace((user, 0usize)).is_some() {
                return Err(invalid());
            }
        }
    }
    for block in f.layout.blocks() {
        for inst in f.layout.block_insts(block) {
            for &arg in f.dfg.inst_args(inst) {
                let value = f.dfg.resolve_aliases(arg);
                if let Some((expected, count)) = &mut uses[value.as_u32() as usize] {
                    if inst != *expected || arg != value {
                        return Err(invalid());
                    }
                    *count += 1;
                }
            }
            for branch in
                f.dfg.insts[inst].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                for arg in branch.args(&f.dfg.value_lists) {
                    if let BlockArg::Value(value) = arg {
                        if uses[f.dfg.resolve_aliases(value).as_u32() as usize].is_some() {
                            return Err(invalid());
                        }
                    }
                }
            }
        }
    }
    if uses.iter().flatten().any(|(_, count)| *count != 1) {
        return Err(invalid());
    }
    drop(uses);
    let mut candidate = f.clone();
    for s in &sites {
        let args: [IrValue; 7] = {
            let mut args = [host; 7];
            let values = f.dfg.inst_args(s.call);
            if values.len() > args.len() {
                return Err(invalid());
            }
            args[..values.len()].copy_from_slice(values);
            args
        };
        let length = f.dfg.inst_args(s.call).len();
        candidate
            .replace(s.call)
            .call(targets[s.index], &args[1..length]);
        candidate.layout.remove_inst(s.table);
        candidate.layout.remove_inst(s.target);
    }
    verify(f, &candidate, &sites, targets)?;
    *f = candidate;
    Ok(calls)
}

#[cfg(test)]
#[path = "direct_tests.rs"]
mod tests;
