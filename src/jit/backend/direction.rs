use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{condcodes::IntCC, types, Function, Inst, InstBuilder, InstructionData, Opcode, ValueDef},
};

fn candidates(function: &Function, select: Inst) -> Option<[Inst; 2]> {
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args,
    } = function.dfg.insts[select]
    else {
        return None;
    };
    if function.dfg.value_type(args[0]) != types::I8 {
        return None;
    }
    let mut comparisons = [select; 2];
    let mut operands = None;
    for (index, code) in [
        IntCC::SignedGreaterThanOrEqual,
        IntCC::SignedLessThanOrEqual,
    ]
    .into_iter()
    .enumerate()
    {
        let ValueDef::Result(inst, 0) = function.dfg.value_def(args[index + 1]) else {
            return None;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: inputs,
        } = function.dfg.insts[inst]
        else {
            return None;
        };
        if cond != code
            || inputs
                .iter()
                .any(|&value| function.dfg.value_type(value) != types::I64)
            || operands.is_some_and(|previous| previous != inputs)
            || function.layout.inst_block(inst) != function.layout.inst_block(select)
        {
            return None;
        }
        let uses = function
            .layout
            .blocks()
            .flat_map(|block| function.layout.block_insts(block))
            .flat_map(|inst| function.dfg.inst_args(inst))
            .filter(|&&value| function.dfg.resolve_aliases(value) == args[index + 1])
            .count();
        let branch_use = function.layout.blocks().flat_map(|block| function.layout.block_insts(block))
            .flat_map(|inst| function.dfg.insts[inst].branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables))
            .flat_map(|call| call.args(&function.dfg.value_lists))
            .any(|arg| matches!(arg, cranelift_codegen::ir::BlockArg::Value(value) if function.dfg.resolve_aliases(value) == args[index + 1]));
        if uses != 1 || branch_use {
            return None;
        }
        operands = Some(inputs);
        comparisons[index] = inst;
    }
    (function.layout.prev_inst(select) == Some(comparisons[1])
        && function.layout.prev_inst(comparisons[1]) == Some(comparisons[0]))
    .then_some(comparisons)
}

pub(super) fn rewrite(function: &mut Function) -> usize {
    if function.dfg.num_insts() > 8192 {
        return 0;
    }
    let mut changed = 0;
    while changed < 32 {
        let found = function
            .layout
            .blocks()
            .flat_map(|block| function.layout.block_insts(block))
            .find_map(|inst| candidates(function, inst).map(|comparisons| (inst, comparisons)));
        let Some((select, comparisons)) = found else {
            return changed;
        };
        let InstructionData::Ternary { args, .. } = function.dfg.insts[select] else {
            unreachable!()
        };
        let old = function.dfg.first_result(select);
        let source = function.layout.inst_block(select).unwrap();
        let join = function.dfg.make_block();
        let arms = [function.dfg.make_block(), function.dfg.make_block()];
        let value = function.dfg.append_block_param(join, types::I8);
        let mut cursor = FuncCursor::new(function);
        cursor.goto_inst(select);
        cursor.insert_block(join);
        cursor.remove_inst();
        cursor.func.dfg.clear_results(select);
        cursor.func.dfg.change_to_alias(old, value);
        for (index, arm) in arms.into_iter().enumerate() {
            cursor.func.layout.remove_inst(comparisons[index]);
            cursor.func.layout.insert_block(arm, join);
            cursor.func.layout.append_inst(comparisons[index], arm);
            cursor.goto_bottom(arm);
            cursor.ins().jump(join, &[args[index + 1].into()]);
        }
        cursor.goto_bottom(source);
        cursor.ins().brif(args[0], arms[0], &[], arms[1], &[]);
        changed += 1;
    }
    changed
}

#[cfg(test)]
mod tests;
