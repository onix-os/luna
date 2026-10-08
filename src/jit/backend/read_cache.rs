use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{
        types, BlockCall, Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode, Value,
    },
};

use super::super::JitError;

const MAX_WORDS: usize = 32;
const MAX_VALUES: usize = 8192;

fn invalid() -> JitError {
    JitError::Compilation("invalid scalar read cache".into())
}

fn word(
    function: &Function,
    inst: Inst,
    slots: Value,
    words: usize,
) -> Result<Option<usize>, JitError> {
    let (address, offset, ty, flags) = match function.dfg.insts[inst] {
        InstructionData::Load {
            opcode: Opcode::Load,
            arg,
            offset,
            flags,
        } => (
            arg,
            i32::from(offset),
            function.dfg.value_type(function.dfg.first_result(inst)),
            flags,
        ),
        InstructionData::Store {
            opcode: Opcode::Store,
            args,
            offset,
            flags,
        } => (
            args[1],
            i32::from(offset),
            function.dfg.value_type(args[0]),
            flags,
        ),
        _ => return Ok(None),
    };
    if function.dfg.resolve_aliases(address) != function.dfg.resolve_aliases(slots) {
        return Ok(None);
    }
    if ty != types::I64
        || offset < 0
        || offset % 8 != 0
        || offset as usize / 8 >= words
        || function.dfg.mem_flags[flags] != MemFlagsData::new()
    {
        return Err(invalid());
    }
    Ok(Some(offset as usize / 8))
}

#[derive(Clone, Copy, Default)]
struct Live {
    uses: u32,
    defines: u32,
    incoming: u32,
    outgoing: u32,
}

fn append(
    call: &mut BlockCall,
    values: &[Option<Value>],
    states: &[Live],
    lists: &mut cranelift_codegen::ir::instructions::ValueListPool,
) -> Result<(), JitError> {
    let mask = states[call.block(lists).as_u32() as usize].incoming;
    for (index, value) in values.iter().enumerate() {
        if mask & (1 << index) != 0 {
            call.append_argument(value.ok_or_else(invalid)?, lists);
        }
    }
    Ok(())
}

pub(super) fn promote(
    function: &mut Function,
    slots: Value,
    exit: Value,
    registers: usize,
    allocator: super::super::resources::BudgetAllocator,
) -> Result<bool, JitError> {
    use allocator_api2::vec::Vec;
    let words = registers.checked_mul(2).ok_or_else(invalid)?;
    if words == 0 || words > MAX_WORDS {
        return Ok(false);
    }
    let blocks = function.dfg.num_blocks();
    let mut edges = 0usize;
    let mut calls = 0usize;
    let mut reads = [0u32; MAX_WORDS];
    let mut writes = [0u32; MAX_WORDS];
    for block in function.layout.blocks() {
        for inst in function.layout.block_insts(block) {
            let data = function.dfg.insts[inst];
            if let Some(index) = word(function, inst, slots, words)?.filter(|index| index % 2 == 1)
            {
                if data.opcode() == Opcode::Load {
                    reads[index] = reads[index].saturating_add(1);
                } else {
                    writes[index] = writes[index].saturating_add(1);
                }
            }
            if data.opcode().can_store()
                && word(function, inst, slots, words)?.is_none()
                && !matches!(data, InstructionData::Store { args, .. } if function.dfg.resolve_aliases(args[1]) == function.dfg.resolve_aliases(exit))
            {
                return Ok(false);
            }
            if data.opcode().is_call() {
                if data.opcode().is_terminator() {
                    return Ok(false);
                }
                calls += 1;
            }
            match data {
                InstructionData::Jump { .. } => edges += 1,
                InstructionData::Brif { .. } => edges += 2,
                InstructionData::BranchTable { table, .. } => {
                    edges += function.dfg.jump_tables[table].all_branches().len()
                }
                _ if data.opcode().is_branch() => return Ok(false),
                _ => {}
            }
        }
    }
    if words
        .checked_mul(blocks + edges + calls + 1)
        .ok_or_else(invalid)?
        > MAX_VALUES
    {
        return Ok(false);
    }
    let mut selected = 0u32;
    for _ in 0..2 {
        let next = (0..words)
            .filter(|index| writes[*index] > 1 && reads[*index] > 0 && selected & (1 << index) == 0)
            .max_by_key(|index| (writes[*index], reads[*index]));
        if let Some(index) = next {
            selected |= 1 << index;
        }
    }
    if selected == 0 {
        return Ok(false);
    }
    let refused = || JitError::ResourceLimit("scalar read-cache liveness");
    let mut states = Vec::new_in(allocator.clone());
    states.try_reserve_exact(blocks).map_err(|_| refused())?;
    states.resize(blocks, Live::default());
    let mut reloads = Vec::new_in(allocator);
    reloads.try_reserve_exact(calls).map_err(|_| refused())?;
    for block in function.layout.blocks() {
        let state = &mut states[block.as_u32() as usize];
        for inst in function.layout.block_insts(block) {
            if let Some(index) =
                word(function, inst, slots, words)?.filter(|index| selected & (1 << index) != 0)
            {
                if function.dfg.insts[inst].opcode() == Opcode::Load {
                    state.uses |= (1 << index) & !state.defines;
                } else {
                    state.defines |= 1 << index;
                }
            }
            if function.dfg.insts[inst].opcode().is_call() {
                state.defines = u32::MAX;
            }
        }
        state.incoming = state.uses;
    }
    let mut remaining = MAX_VALUES + 1;
    loop {
        remaining = remaining.checked_sub(1).ok_or_else(invalid)?;
        let mut changed = false;
        let mut next = function.layout.last_block();
        while let Some(block) = next {
            next = function.layout.prev_block(block);
            let last = function.layout.last_inst(block).ok_or_else(invalid)?;
            let outgoing = function.dfg.insts[last]
                .branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables)
                .iter()
                .fold(0, |mask, call| {
                    mask | states[call.block(&function.dfg.value_lists).as_u32() as usize].incoming
                });
            let state = &mut states[block.as_u32() as usize];
            let incoming = state.uses | (outgoing & !state.defines);
            changed |= incoming != state.incoming;
            state.incoming = incoming;
            state.outgoing = outgoing;
        }
        if !changed {
            break;
        }
    }
    for block in function.layout.blocks() {
        let mut live = states[block.as_u32() as usize].outgoing;
        let mut next = function.layout.last_inst(block);
        while let Some(inst) = next {
            next = function.layout.prev_inst(inst);
            if function.dfg.insts[inst].opcode().is_call() {
                reloads.push((inst, live));
                live = 0;
            }
            if let Some(index) =
                word(function, inst, slots, words)?.filter(|index| selected & (1 << index) != 0)
            {
                if function.dfg.insts[inst].opcode() == Opcode::Load {
                    live |= 1 << index;
                } else {
                    live &= !(1 << index);
                }
            }
        }
        if live != states[block.as_u32() as usize].incoming {
            return Err(invalid());
        }
    }
    reloads.sort_unstable_by_key(|(inst, _)| inst.as_u32());
    let entry = function.layout.entry_block().ok_or_else(invalid)?;
    let first = function.layout.first_inst(entry).ok_or_else(invalid)?;
    let mut initial = [None; MAX_WORDS];
    {
        let mut cursor = FuncCursor::new(function);
        cursor.goto_inst(first);
        for (index, value) in initial[..words].iter_mut().enumerate() {
            if states[entry.as_u32() as usize].incoming & (1 << index) != 0 {
                *value = Some(cursor.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    slots,
                    (index * 8) as i32,
                ));
            }
        }
    }
    let mut next = function.layout.next_block(entry);
    while let Some(block) = next {
        next = function.layout.next_block(block);
        for _ in 0..states[block.as_u32() as usize].incoming.count_ones() {
            function.dfg.append_block_param(block, types::I64);
        }
    }
    let mut next = Some(entry);
    while let Some(block) = next {
        next = function.layout.next_block(block);
        let mut current = if block == entry {
            initial
        } else {
            [None; MAX_WORDS]
        };
        if block != entry {
            let mask = states[block.as_u32() as usize].incoming;
            let params = function.dfg.block_params(block);
            let mut params = params[params.len() - mask.count_ones() as usize..].iter();
            for (index, value) in current[..words].iter_mut().enumerate() {
                if mask & (1 << index) != 0 {
                    *value = params.next().copied();
                }
            }
        }
        let mut next_inst = if block == entry {
            Some(first)
        } else {
            function.layout.first_inst(block)
        };
        while let Some(inst) = next_inst {
            next_inst = function.layout.next_inst(inst);
            let data = function.dfg.insts[inst];
            if let Some(index) =
                word(function, inst, slots, words)?.filter(|index| selected & (1 << index) != 0)
            {
                match data {
                    InstructionData::Load { .. } => {
                        let result = function.dfg.first_result(inst);
                        let source = current[index].ok_or_else(invalid)?;
                        function.dfg.clear_results(inst);
                        function.dfg.change_to_alias(result, source);
                        function.layout.remove_inst(inst);
                    }
                    InstructionData::Store { args, .. } => current[index] = Some(args[0]),
                    _ => return Err(invalid()),
                }
            }
            if data.opcode().is_call() {
                let mask = reloads[reloads
                    .binary_search_by_key(&inst.as_u32(), |(inst, _)| inst.as_u32())
                    .map_err(|_| invalid())?]
                .1;
                current.fill(None);
                if mask != 0 {
                    let mut cursor = FuncCursor::new(function);
                    cursor.goto_inst(next_inst.ok_or_else(invalid)?);
                    for (index, value) in current[..words].iter_mut().enumerate() {
                        if mask & (1 << index) != 0 {
                            *value = Some(cursor.ins().load(
                                types::I64,
                                MemFlagsData::new(),
                                slots,
                                (index * 8) as i32,
                            ));
                        }
                    }
                }
            }
            let dfg = &mut function.dfg;
            match &mut dfg.insts[inst] {
                InstructionData::Jump { destination, .. } => append(
                    destination,
                    &current[..words],
                    &states,
                    &mut dfg.value_lists,
                )?,
                InstructionData::Brif { blocks, .. } => {
                    for call in blocks {
                        append(call, &current[..words], &states, &mut dfg.value_lists)?;
                    }
                }
                InstructionData::BranchTable { table, .. } => {
                    for call in dfg.jump_tables[*table].all_branches_mut() {
                        append(call, &current[..words], &states, &mut dfg.value_lists)?;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::{AbiParam, BlockArg};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    fn allocator() -> super::super::super::resources::BudgetAllocator {
        super::super::super::resources::BudgetAllocator(
            super::super::super::resources::Ledger::new(usize::MAX),
        )
    }

    fn fixture(unknown_store: bool) -> (Function, Value, Value, Inst, Inst, Inst) {
        let mut function = Function::new();
        function
            .signature
            .params
            .extend([AbiParam::new(types::I64); 3]);
        function.signature.returns.push(AbiParam::new(types::I64));
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let entry = builder.create_block();
        let body = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let slots = builder.block_params(entry)[0];
        let exit = builder.block_params(entry)[1];
        let other = builder.block_params(entry)[2];
        let value = builder.ins().iconst(types::I64, 7);
        builder.ins().store(MemFlagsData::new(), value, slots, 8);
        let store = builder.ins().store(
            MemFlagsData::new(),
            value,
            if unknown_store { other } else { slots },
            8,
        );
        let branch = builder.ins().jump(body, &[]);
        builder.switch_to_block(body);
        let result = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), slots, 8);
        let load = builder.func.dfg.value_def(result).unwrap_inst();
        builder.ins().return_(&[result]);
        builder.seal_all_blocks();
        (function, slots, exit, store, branch, load)
    }

    #[test]
    fn scalar_read_cache_transports_stored_values_and_preserves_stores() {
        let (mut function, slots, exit, store, branch, load) = fixture(false);
        let original_store = function.dfg.insts[store];
        let old_result = function.dfg.first_result(load);
        assert!(promote(&mut function, slots, exit, 1, allocator()).unwrap());
        assert_eq!(function.dfg.insts[store], original_store);
        assert!(function.layout.inst_block(load).is_none());
        let InstructionData::Jump { destination, .. } = function.dfg.insts[branch] else {
            unreachable!()
        };
        let values: Vec<_> = destination.args(&function.dfg.value_lists).collect();
        assert_eq!(values.len(), 1);
        let InstructionData::Store { args, .. } = original_store else {
            unreachable!()
        };
        assert_eq!(values[0], BlockArg::Value(args[0]));
        let body = destination.block(&function.dfg.value_lists);
        assert_eq!(
            function.dfg.resolve_aliases(old_result),
            function.dfg.block_params(body)[0]
        );
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
    }

    #[test]
    fn scalar_read_cache_skips_unknown_writes_and_large_prefixes_without_changes() {
        for (unknown, registers) in [(true, 1), (false, 17), (false, 0)] {
            let (mut function, slots, exit, ..) = fixture(unknown);
            let before = function.display().to_string();
            assert!(!promote(&mut function, slots, exit, registers, allocator()).unwrap());
            assert_eq!(function.display().to_string(), before);
        }
    }

    #[test]
    fn scalar_read_cache_rejects_misaligned_and_out_of_prefix_accesses() {
        for offset in [-8, 4, 16] {
            let (mut function, slots, exit, store, ..) = fixture(false);
            let InstructionData::Store { offset: actual, .. } = &mut function.dfg.insts[store]
            else {
                unreachable!()
            };
            *actual = offset.into();
            let before = function.display().to_string();
            assert!(promote(&mut function, slots, exit, 1, allocator()).is_err());
            assert_eq!(function.display().to_string(), before);
        }
    }

    fn insert_call(function: &mut Function, slots: Value, before: Inst) -> Inst {
        let mut signature = cranelift_codegen::ir::Signature::new(function.signature.call_conv);
        signature.params.push(AbiParam::new(types::I64));
        let signature = function.import_signature(signature);
        let helper = function.import_function(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::testcase("cache_clobber"),
            signature,
            colocated: false,
            patchable: false,
        });
        let mut cursor = FuncCursor::new(function);
        cursor.goto_inst(before);
        cursor.ins().call(helper, &[slots])
    }

    #[test]
    fn scalar_read_cache_reloads_live_words_after_opaque_calls() {
        let (mut function, slots, exit, _, branch, _) = fixture(false);
        let call = insert_call(&mut function, slots, branch);
        assert!(promote(&mut function, slots, exit, 1, allocator()).unwrap());
        let reload = function.layout.next_inst(call).unwrap();
        assert!(
            matches!(function.dfg.insts[reload], InstructionData::Load { opcode: Opcode::Load, arg, offset, .. } if arg == slots && i32::from(offset) == 8)
        );
        assert_eq!(function.layout.next_inst(reload), Some(branch));
        let InstructionData::Jump { destination, .. } = function.dfg.insts[branch] else {
            unreachable!()
        };
        assert_eq!(
            destination
                .args(&function.dfg.value_lists)
                .collect::<Vec<_>>(),
            [BlockArg::Value(function.dfg.first_result(reload))]
        );
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
    }

    #[test]
    fn scalar_read_cache_workspace_refusal_rolls_back_without_ir_changes() {
        for after in [0, 1] {
            let (mut function, slots, exit, _, branch, _) = fixture(false);
            insert_call(&mut function, slots, branch);
            let before = function.display().to_string();
            let allocator = allocator();
            let ledger = allocator.0.clone();
            ledger.fail_after(after);
            assert!(matches!(
                promote(&mut function, slots, exit, 1, allocator),
                Err(JitError::ResourceLimit("scalar read-cache liveness"))
            ));
            assert_eq!(ledger.current(), 0);
            assert_eq!(ledger.refusals(), 1);
            assert_eq!(function.display().to_string(), before);
        }
    }
}
