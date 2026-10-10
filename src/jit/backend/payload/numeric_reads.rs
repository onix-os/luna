use super::reads::{bitcast, block, branch, compare, constant, jump, load, value};
use super::*;
use crate::jit::{dominance::Dominators, preds::Predecessors};

#[cfg(test)]
#[path = "numeric_read_tests.rs"]
mod tests;

#[derive(Clone, Copy)]
pub(super) struct Guard {
    tag: Inst,
    branch: Inst,
    accepted: Block,
    rejected: Block,
}

pub(super) struct Read {
    guard: Guard,
    blocks: [Block; 4],
    result: IrValue,
    index: u32,
    cast: Option<(Inst, IrValue)>,
}

fn definition(f: &Function, value: IrValue) -> Option<Inst> {
    match f.dfg.value_def(value) {
        cranelift_codegen::ir::ValueDef::Result(inst, 0) => Some(inst),
        _ => None,
    }
}

fn numeric(f: &Function, condition: IrValue, tag: IrValue) -> Option<()> {
    let cmp = definition(f, condition)?;
    let InstructionData::IntCompare {
        cond: IntCC::Equal,
        args,
        ..
    } = f.dfg.insts[cmp]
    else {
        return None;
    };
    compare(f, cmp, IntCC::Equal, args).ok()?;
    constant(f, definition(f, args[1])?, abi::INTEGER as i64).ok()?;
    let masked = definition(f, args[0])?;
    let InstructionData::Binary {
        opcode: Opcode::Band,
        args,
    } = f.dfg.insts[masked]
    else {
        return None;
    };
    value(f, masked, Opcode::Band, &args, types::I64).ok()?;
    if args[0] != tag {
        return None;
    }
    constant(f, definition(f, args[1])?, -2).ok()?;
    Some(())
}

pub(super) fn candidate(f: &Function, inst: Inst, slots: IrValue, index: u32) -> Option<Guard> {
    let ty = f
        .dfg
        .inst_results(inst)
        .first()
        .map(|&v| f.dfg.value_type(v))?;
    if !matches!(ty, types::I64 | types::F64) {
        return None;
    }
    load(f, inst, slots, index as i32 * 16 + 8, ty).ok()?;
    let parent = f.layout.inst_block(inst)?;
    let tag = f.layout.prev_inst(inst)?;
    let tag_value = load(f, tag, slots, index as i32 * 16, types::I64).ok()?;
    let branch = f.layout.last_inst(parent)?;
    let InstructionData::Brif { arg, blocks, .. } = f.dfg.insts[branch] else {
        return None;
    };
    numeric(f, arg, tag_value)?;
    let accepted = blocks[0].block(&f.dfg.value_lists);
    let rejected = blocks[1].block(&f.dfg.value_lists);
    if accepted == rejected || blocks[0].args(&f.dfg.value_lists).next().is_some() {
        return None;
    }
    Some(Guard {
        tag,
        branch,
        accepted,
        rejected,
    })
}

fn fresh(
    f: &Function,
    predecessors: &Predecessors,
    tag: Inst,
    mut end: Inst,
    remaining: &mut usize,
) -> bool {
    loop {
        let Some(next) = remaining.checked_sub(1) else {
            return false;
        };
        *remaining = next;
        if end == tag {
            return true;
        }
        if !matches!(
            f.dfg.insts[end].opcode(),
            Opcode::Iconst
                | Opcode::Load
                | Opcode::Band
                | Opcode::Icmp
                | Opcode::Brif
                | Opcode::Jump
                | Opcode::Uextend
                | Opcode::Bitcast
        ) {
            return false;
        }
        if let Some(previous) = f.layout.prev_inst(end) {
            end = previous;
            continue;
        }
        let Some(block) = f.layout.inst_block(end) else {
            return false;
        };
        let mut parents = predecessors.pred_iter(block).peekable();
        return parents.peek().is_some()
            && parents.all(|parent| fresh(f, predecessors, tag, parent.inst, remaining));
    }
}

fn guard(f: &Function, g: Guard, slots: IrValue, index: u32, predecessors: &Predecessors) -> bool {
    let Ok(tag) = load(f, g.tag, slots, index as i32 * 16, types::I64) else {
        return false;
    };
    let InstructionData::Brif { arg, blocks, .. } = f.dfg.insts[g.branch] else {
        return false;
    };
    if numeric(f, arg, tag).is_none()
        || blocks[0].block(&f.dfg.value_lists) != g.accepted
        || blocks[1].block(&f.dfg.value_lists) != g.rejected
        || g.accepted == g.rejected
        || blocks[0].args(&f.dfg.value_lists).next().is_some()
        || !f.dfg.block_params(g.accepted).is_empty()
        || f.layout.entry_block() == Some(g.accepted)
    {
        return false;
    }
    let mut incoming = predecessors.pred_iter(g.accepted);
    incoming.next().is_some_and(|p| p.inst == g.branch)
        && incoming.next().is_none()
        && fresh(f, predecessors, g.tag, g.branch, &mut 256)
}

pub(super) struct Analysis {
    predecessors: Predecessors,
    dominators: Dominators,
    remaining: usize,
}

impl Analysis {
    pub(super) fn new(f: &Function, allocator: BudgetAllocator) -> Result<Self, JitError> {
        let predecessors = Predecessors::new(f, allocator.clone())?;
        let dominators = Dominators::new(f, &predecessors, allocator)?;
        let (instructions, blocks) = size(f);
        let remaining = instructions
            .checked_add(blocks)
            .and_then(|n| n.checked_mul(64))
            .ok_or(JitError::ResourceLimit("payload numeric read work"))?;
        Ok(Self {
            predecessors,
            dominators,
            remaining,
        })
    }

    pub(super) fn admit(
        &mut self,
        f: &Function,
        inst: Inst,
        slots: IrValue,
        index: u32,
    ) -> Option<Guard> {
        let g = candidate(f, inst, slots, index)?;
        if !guard(f, g, slots, index, &self.predecessors) {
            return None;
        }
        let result = f.dfg.first_result(inst);
        for block in f.layout.blocks() {
            for user in f.layout.block_insts(block) {
                self.remaining = self.remaining.checked_sub(1)?;
                let used = f.dfg.inst_args(user).iter().any(|&v| f.dfg.resolve_aliases(v) == result)
                    || f.dfg.insts[user].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                        .iter().any(|b| b.args(&f.dfg.value_lists).any(|v| matches!(v, BlockArg::Value(v) if f.dfg.resolve_aliases(v) == result)));
                if used {
                    let mut at = block;
                    while at != g.accepted {
                        self.remaining = self.remaining.checked_sub(1)?;
                        at = self.dominators.idom(at)?;
                    }
                }
            }
        }
        Some(g)
    }
}

pub(super) fn emit(f: &mut Function, inst: Inst, slots: IrValue, index: u32, guard: Guard) -> Read {
    let before = guard.accepted;
    let first = f.layout.first_inst(before).unwrap();
    f.layout.remove_inst(inst);
    f.layout.insert_inst(inst, first);
    let after = f.dfg.make_block();
    f.layout.split_block(after, inst);
    let result = f.dfg.first_result(inst);
    let (result, cast) = if f.dfg.value_type(result) == types::F64 {
        let parameter = f.dfg.append_block_param(after, types::I64);
        f.replace(inst)
            .bitcast(types::F64, MemFlagsData::new(), parameter);
        (parameter, Some((inst, result)))
    } else {
        f.dfg.detach_inst_results(inst);
        f.dfg.attach_block_param(after, result);
        f.layout.remove_inst(inst);
        (result, None)
    };
    let wide = block(f);
    let zero = block(f);
    let mut c = FuncCursor::new(f);
    c.goto_bottom(before);
    let pointer = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
    );
    let null = c.ins().iconst(types::I64, 0);
    let nonnull = c.ins().icmp(IntCC::NotEqual, pointer, null);
    c.ins().brif(nonnull, wide, &[], zero, &[]);
    c.goto_bottom(wide);
    let bits = c.ins().load(types::I64, MemFlagsData::new(), pointer, 0);
    c.ins().jump(after, &[bits.into()]);
    c.goto_bottom(zero);
    c.ins().jump(after, &[null.into()]);
    Read {
        guard,
        blocks: [before, after, wide, zero],
        result,
        index,
        cast,
    }
}

pub(super) fn verify(
    f: &Function,
    read: &Read,
    slots: IrValue,
    predecessors: &Predecessors,
) -> Result<(), JitError> {
    let [before, after, wide, zero] = read.blocks;
    if before != read.guard.accepted
        || !guard(f, read.guard, slots, read.index, predecessors)
        || f.dfg.block_params(after) != [read.result]
        || f.dfg.value_type(read.result) != types::I64
    {
        return Err(invalid());
    }
    for b in [before, wide, zero] {
        if !f.dfg.block_params(b).is_empty() {
            return Err(invalid());
        }
    }
    if let Some((inst, result)) = read.cast {
        if f.layout.first_inst(after) != Some(inst)
            || bitcast(f, inst, read.result, types::F64)? != result
        {
            return Err(invalid());
        }
    }
    let [pointer, null, nonnull, split] = shape(f.layout.block_insts(before))?;
    let pointer = load(
        f,
        pointer,
        slots,
        read.index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
        types::I64,
    )?;
    let null = constant(f, null, 0)?;
    let nonnull = compare(f, nonnull, IntCC::NotEqual, [pointer, null])?;
    branch(f, split, nonnull, wide, zero)?;
    let [bits, end] = shape(f.layout.block_insts(wide))?;
    let bits = load(f, bits, pointer, 0, types::I64)?;
    jump(f, end, after, bits)?;
    let [zero_end] = shape(f.layout.block_insts(zero))?;
    jump(f, zero_end, after, null)?;
    for b in [wide, zero] {
        let mut incoming = predecessors.pred_iter(b);
        if !incoming.next().is_some_and(|p| p.inst == split) || incoming.next().is_some() {
            return Err(invalid());
        }
    }
    let mut incoming = predecessors.pred_iter(after);
    let a = incoming.next().ok_or_else(invalid)?.inst;
    let b = incoming.next().ok_or_else(invalid)?.inst;
    if incoming.next().is_some() || !((a == end && b == zero_end) || (a == zero_end && b == end)) {
        return Err(invalid());
    }
    Ok(())
}
