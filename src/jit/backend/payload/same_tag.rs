use super::*;
use crate::jit::preds::Predecessors;

#[cfg(test)]
#[path = "same_tag_tests.rs"]
mod tests;

#[derive(Clone, Copy)]
pub(super) struct Proof {
    store: Inst,
    index: u32,
    tag: IrValue,
}

pub(super) struct Analysis {
    predecessors: Predecessors,
    remaining: usize,
    verification_remaining: usize,
}

impl Analysis {
    pub(super) fn new(f: &Function, allocator: BudgetAllocator) -> Result<Self, JitError> {
        let (instructions, blocks) = size(f);
        let remaining = instructions
            .checked_add(blocks)
            .and_then(|n| n.checked_mul(64))
            .ok_or(JitError::ResourceLimit("payload tag proof work"))?;
        Ok(Self {
            predecessors: Predecessors::new(f, allocator)?,
            remaining,
            verification_remaining: remaining,
        })
    }

    pub(super) fn admit(
        &mut self,
        f: &Function,
        slots: IrValue,
        store: Inst,
        index: u32,
        tag: IrValue,
    ) -> Option<Proof> {
        let proof = Proof { store, index, tag };
        verify(f, &self.predecessors, slots, proof, &mut self.remaining).then_some(proof)
    }

    pub(super) fn verify(&mut self, f: &Function, slots: IrValue, proof: Proof) -> bool {
        verify(
            f,
            &self.predecessors,
            slots,
            proof,
            &mut self.verification_remaining,
        )
    }
}

fn verify(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    proof: Proof,
    remaining: &mut usize,
) -> bool {
    let InstructionData::Store {
        opcode: Opcode::Store,
        args,
        offset,
        ..
    } = f.dfg.insts[proof.store]
    else {
        return false;
    };
    if proof.index >= 256
        || args != [proof.tag, slots]
        || i64::from(i32::from(offset)) != i64::from(proof.index) * 16
    {
        return false;
    }
    verify_at(
        f,
        predecessors,
        slots,
        proof.store,
        proof.index,
        proof.tag,
        remaining,
    )
}

pub(super) fn verify_at(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    at: Inst,
    index: u32,
    tag: IrValue,
    remaining: &mut usize,
) -> bool {
    if index >= 256 {
        return false;
    }
    let Some(tag) = immediate(f, tag) else {
        return false;
    };
    if !matches!(
        tag as u64,
        abi::NIL | abi::BOOLEAN | abi::INTEGER | abi::NUMBER
    ) {
        return false;
    }
    let Some(previous) = f.layout.prev_inst(at) else {
        return false;
    };
    search(f, predecessors, slots, index, tag, previous, 32, remaining)
}

fn search(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    index: u32,
    tag: i64,
    mut inst: Inst,
    depth: usize,
    remaining: &mut usize,
) -> bool {
    if depth == 0 {
        return false;
    }
    loop {
        let Some(next) = remaining.checked_sub(1) else {
            return false;
        };
        *remaining = next;
        let opcode = f.dfg.insts[inst].opcode();
        if opcode.can_store() || opcode.is_call() {
            return false;
        }
        if let Some(previous) = f.layout.prev_inst(inst) {
            inst = previous;
            continue;
        }
        let Some(block) = f.layout.inst_block(inst) else {
            return false;
        };
        if f.layout.entry_block() == Some(block) {
            return false;
        }
        let mut parents = predecessors.pred_iter(block).peekable();
        return parents.peek().is_some()
            && parents.all(|parent| {
                if let InstructionData::Brif { arg, blocks, .. } = f.dfg.insts[parent.inst] {
                    if blocks[0].block(&f.dfg.value_lists) == block
                        && blocks[1].block(&f.dfg.value_lists) != block
                    {
                        if let Some(load) = predicate(f, arg, slots, index, tag, 8, remaining) {
                            return fresh(f, predecessors, load, parent.inst, depth - 1, remaining);
                        }
                    }
                }
                search(
                    f,
                    predecessors,
                    slots,
                    index,
                    tag,
                    parent.inst,
                    depth - 1,
                    remaining,
                )
            });
    }
}

fn fresh(
    f: &Function,
    predecessors: &Predecessors,
    load: Inst,
    mut inst: Inst,
    depth: usize,
    remaining: &mut usize,
) -> bool {
    if depth == 0 {
        return false;
    }
    loop {
        let Some(next) = remaining.checked_sub(1) else {
            return false;
        };
        *remaining = next;
        if inst == load {
            return true;
        }
        let opcode = f.dfg.insts[inst].opcode();
        if opcode.can_store() || opcode.is_call() {
            return false;
        }
        if let Some(previous) = f.layout.prev_inst(inst) {
            inst = previous;
            continue;
        }
        let Some(block) = f.layout.inst_block(inst) else {
            return false;
        };
        if f.layout.entry_block() == Some(block) {
            return false;
        }
        let mut parents = predecessors.pred_iter(block).peekable();
        return parents.peek().is_some()
            && parents
                .all(|parent| fresh(f, predecessors, load, parent.inst, depth - 1, remaining));
    }
}

fn definition(f: &Function, value: IrValue) -> Option<Inst> {
    match f.dfg.value_def(value) {
        cranelift_codegen::ir::ValueDef::Result(inst, 0) => Some(inst),
        _ => None,
    }
}

fn immediate(f: &Function, value: IrValue) -> Option<i64> {
    let inst = definition(f, value)?;
    let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = f.dfg.insts[inst]
    else {
        return None;
    };
    let immediate = i64::from(imm);
    reads::constant(f, inst, immediate).ok()?;
    Some(immediate)
}

fn predicate(
    f: &Function,
    value: IrValue,
    slots: IrValue,
    index: u32,
    tag: i64,
    depth: usize,
    remaining: &mut usize,
) -> Option<Inst> {
    *remaining = remaining.checked_sub(1)?;
    if depth == 0 {
        return None;
    }
    let inst = definition(f, value)?;
    match f.dfg.insts[inst] {
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } if f.dfg.value_type(value) == types::I8 => {
            predicate(f, args[0], slots, index, tag, depth - 1, remaining)
                .or_else(|| predicate(f, args[1], slots, index, tag, depth - 1, remaining))
        }
        InstructionData::IntCompare {
            cond: IntCC::Equal,
            args,
            ..
        } => {
            reads::compare(f, inst, IntCC::Equal, args).ok()?;
            let loaded = if immediate(f, args[1]) == Some(tag) {
                args[0]
            } else if immediate(f, args[0]) == Some(tag) {
                args[1]
            } else {
                return None;
            };
            let load = definition(f, loaded)?;
            reads::load(f, load, slots, index as i32 * 16, types::I64).ok()?;
            Some(load)
        }
        _ => None,
    }
}

impl Proof {
    pub(super) fn matches(self, index: u32, tag: IrValue) -> bool {
        self.index == index && self.tag == tag
    }
}
