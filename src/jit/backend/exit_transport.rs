use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{
        types, AbiParam, Block, BlockArg, Function, Inst, InstBuilder, InstructionData,
        MemFlagsData, Opcode, Value,
    },
};

use super::JitError;

fn invalid() -> JitError {
    JitError::Compilation("invalid native exit transport".into())
}

fn tail(
    function: &Function,
    block: Block,
    output: Value,
) -> Result<([Inst; 4], [Value; 3]), JitError> {
    let mut tail = function.layout.block_insts(block).rev();
    let ret = tail.next().ok_or_else(invalid)?;
    if function.dfg.insts[ret].opcode() != Opcode::Return || !function.dfg.inst_args(ret).is_empty()
    {
        return Err(invalid());
    }
    let mut stores = [ret; 4];
    let mut values = [output; 3];
    for (index, ty, offset) in [(2, types::I32, 12), (1, types::I32, 8), (0, types::I64, 0)] {
        let inst = tail.next().ok_or_else(invalid)?;
        let InstructionData::Store {
            opcode: Opcode::Store,
            args,
            flags,
            offset: actual,
        } = function.dfg.insts[inst]
        else {
            return Err(invalid());
        };
        if function.dfg.resolve_aliases(args[1]) != output
            || function.dfg.resolve_aliases(args[0]) == output
            || function.dfg.value_type(args[0]) != ty
            || i32::from(actual) != offset
            || function.dfg.mem_flags[flags] != MemFlagsData::new()
        {
            return Err(invalid());
        }
        stores[index] = inst;
        values[index] = args[0];
    }
    Ok((stores, values))
}

fn verify_input(function: &Function) -> Result<Value, JitError> {
    let entry = function.layout.entry_block().ok_or_else(invalid)?;
    let params = function.dfg.block_params(entry);
    let types = [types::I64, types::I64, types::I32, types::I64, types::I64];
    if params.len() != 5
        || function
            .signature
            .params
            .iter()
            .copied()
            .ne(types.map(AbiParam::new))
        || !function.signature.returns.is_empty()
        || params
            .iter()
            .zip(types)
            .any(|(&value, ty)| function.dfg.value_type(value) != ty)
    {
        return Err(invalid());
    }
    let output = function.dfg.resolve_aliases(params[3]);
    let mut returns = 0;
    for block in function.layout.blocks() {
        let last = function.layout.last_inst(block).ok_or_else(invalid)?;
        let stores = if function.dfg.insts[last].opcode() == Opcode::Return {
            returns += 1;
            Some(tail(function, block, output)?.0)
        } else {
            None
        };
        for inst in function.layout.block_insts(block) {
            if function.dfg.insts[inst].opcode() == Opcode::Return && inst != last {
                return Err(invalid());
            }
            if function
                .dfg
                .inst_args(inst)
                .iter()
                .any(|&value| function.dfg.resolve_aliases(value) == output)
                && !stores.is_some_and(|stores| stores[..3].contains(&inst))
            {
                return Err(invalid());
            }
            for branch in function.dfg.insts[inst]
                .branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables)
            {
                if branch.args(&function.dfg.value_lists).any(|arg| matches!(arg, BlockArg::Value(value) if function.dfg.resolve_aliases(value) == output)) {
                    return Err(invalid());
                }
            }
        }
    }
    if returns == 0 {
        return Err(invalid());
    }
    Ok(output)
}

fn verify_tail(function: &Function, block: Block, values: [Value; 3]) -> Result<(), JitError> {
    let mut tail = function.layout.block_insts(block).rev();
    let ret = tail.next().ok_or_else(invalid)?;
    let packed = tail.next().ok_or_else(invalid)?;
    let shifted = tail.next().ok_or_else(invalid)?;
    let shift = tail.next().ok_or_else(invalid)?;
    let reason = tail.next().ok_or_else(invalid)?;
    let count = tail.next().ok_or_else(invalid)?;
    let result = |inst| -> Result<Value, JitError> {
        match function.dfg.inst_results(inst) {
            &[value] if function.dfg.value_type(value) == types::I64 => Ok(value),
            _ => Err(invalid()),
        }
    };
    for (inst, input) in [(count, values[1]), (reason, values[2])] {
        if function.dfg.insts[inst].opcode() != Opcode::Uextend
            || function.dfg.inst_args(inst) != [input]
            || function.dfg.value_type(input) != types::I32
        {
            return Err(invalid());
        }
        result(inst)?;
    }
    let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = function.dfg.insts[shift]
    else {
        return Err(invalid());
    };
    if imm.bits() != 32
        || function.dfg.insts[shifted].opcode() != Opcode::Ishl
        || function.dfg.inst_args(shifted) != [result(reason)?, result(shift)?]
        || function.dfg.insts[packed].opcode() != Opcode::Bor
        || function.dfg.inst_args(packed) != [result(count)?, result(shifted)?]
        || function.dfg.insts[ret].opcode() != Opcode::Return
        || function.dfg.inst_args(ret) != [values[0], result(packed)?]
        || function.dfg.value_type(values[0]) != types::I64
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn lower(function: &mut Function) -> Result<(), JitError> {
    let output = verify_input(function)?;
    super::fill_signature(&mut function.signature.returns, [types::I64; 2])?;
    let mut block = function.layout.entry_block();
    while let Some(current) = block {
        block = function.layout.next_block(current);
        let last = function.layout.last_inst(current).ok_or_else(invalid)?;
        if function.dfg.insts[last].opcode() != Opcode::Return {
            continue;
        }
        let (stores, values) = tail(function, current, output)?;
        let mut cursor = FuncCursor::new(function);
        cursor.goto_inst(stores[0]);
        let count = cursor.ins().uextend(types::I64, values[1]);
        let reason = cursor.ins().uextend(types::I64, values[2]);
        let reason = cursor.ins().ishl_imm_u(reason, 32);
        let counts = cursor.ins().bor(count, reason);
        cursor.ins().return_(&[values[0], counts]);
        for inst in stores {
            cursor.func.layout.remove_inst(inst);
        }
        verify_tail(cursor.func, current, values)?;
    }
    function.dfg.remove_block_param(output);
    function.signature.params.remove(3);
    Ok(())
}

#[cfg(test)]
mod tests;
