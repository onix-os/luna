use super::tag_effects::Effects;
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
    cycles: bool,
    effects: Effects,
    stores: Option<BudgetVec<bool, BudgetAllocator>>,
}

struct Route<'a> {
    active: [Option<Inst>; 32],
    witnessed: bool,
    cycles: bool,
    effects: Effects,
    stores: Option<&'a [bool]>,
}

impl<'a> Route<'a> {
    fn new(cycles: bool, effects: Effects, stores: Option<&'a [bool]>) -> Self {
        Self {
            active: [None; 32],
            witnessed: false,
            cycles,
            effects,
            stores,
        }
    }
}

impl Analysis {
    pub(super) fn fresh_before(
        &mut self,
        f: &Function,
        load: Inst,
        at: Inst,
        verification: bool,
    ) -> bool {
        let remaining = if verification {
            &mut self.verification_remaining
        } else {
            &mut self.remaining
        };
        if let Some(previous) = f.layout.prev_inst(at) {
            return fresh(f, &self.predecessors, load, previous, 32, remaining, None);
        }
        let Some(block) = f.layout.inst_block(at) else {
            return false;
        };
        if f.layout.entry_block() == Some(block) {
            return false;
        }
        let mut parents = self.predecessors.pred_iter(block).peekable();
        parents.peek().is_some()
            && parents.all(|parent| {
                fresh(
                    f,
                    &self.predecessors,
                    load,
                    parent.inst,
                    31,
                    remaining,
                    None,
                )
            })
    }

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
            cycles: false,
            effects: Effects::Opaque,
            stores: None,
        })
    }

    pub(super) fn with_cycles(f: &Function, allocator: BudgetAllocator) -> Result<Self, JitError> {
        let mut analysis = Self::new(f, allocator)?;
        analysis.cycles = true;
        Ok(analysis)
    }

    pub(super) fn with_source_stores(
        f: &Function,
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        let mut stores = BudgetVec::new_in(allocator.clone());
        stores
            .try_reserve_exact(f.dfg.num_insts())
            .map_err(|_| JitError::ResourceLimit("payload source store effects"))?;
        stores.resize(f.dfg.num_insts(), false);
        let mut analysis = Self::with_cycles(f, allocator)?;
        analysis.effects = Effects::SourceSlots;
        analysis.stores = Some(stores);
        Ok(analysis)
    }

    pub(super) fn record_bound_write(&mut self, tag: Inst, bits: Inst) -> Result<(), JitError> {
        let stores = self.stores.as_mut().ok_or_else(invalid)?;
        let tag = tag.as_u32() as usize;
        let bits = bits.as_u32() as usize;
        if tag >= stores.len() || bits >= stores.len() {
            return Err(invalid());
        }
        stores[tag] = true;
        stores[bits] = true;
        Ok(())
    }

    pub(super) fn with_bound_stores(
        f: &Function,
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        let mut analysis = Self::with_cycles(f, allocator)?;
        analysis.effects = Effects::BoundMetadata;
        Ok(analysis)
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
        verify(
            f,
            &self.predecessors,
            slots,
            proof,
            &mut self.remaining,
            self.cycles,
            self.effects,
            self.stores.as_deref(),
        )
        .then_some(proof)
    }

    pub(super) fn verify(&mut self, f: &Function, slots: IrValue, proof: Proof) -> bool {
        verify(
            f,
            &self.predecessors,
            slots,
            proof,
            &mut self.verification_remaining,
            self.cycles,
            self.effects,
            self.stores.as_deref(),
        )
    }

    pub(super) fn admit_numeric(
        &mut self,
        f: &Function,
        slots: IrValue,
        at: Inst,
        index: u32,
    ) -> bool {
        numeric_with_cycles(
            f,
            &self.predecessors,
            slots,
            at,
            index,
            &mut self.remaining,
            self.cycles,
            self.effects,
            self.stores.as_deref(),
        )
    }

    pub(super) fn verify_numeric(
        &mut self,
        f: &Function,
        slots: IrValue,
        at: Inst,
        index: u32,
    ) -> bool {
        numeric_with_cycles(
            f,
            &self.predecessors,
            slots,
            at,
            index,
            &mut self.verification_remaining,
            self.cycles,
            self.effects,
            self.stores.as_deref(),
        )
    }
}

pub(super) fn numeric_bound_at(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    at: Inst,
    index: u32,
    remaining: &mut usize,
) -> bool {
    numeric_with_cycles(
        f,
        predecessors,
        slots,
        at,
        index,
        remaining,
        true,
        Effects::BoundMetadata,
        None,
    )
}

fn numeric_with_cycles(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    at: Inst,
    index: u32,
    remaining: &mut usize,
    cycles: bool,
    effects: Effects,
    stores: Option<&[bool]>,
) -> bool {
    if index >= 256 {
        return false;
    }
    [abi::INTEGER, abi::NUMBER].into_iter().any(|tag| {
        let mut route = Route::new(cycles, effects, stores);
        let proven = if let Some(previous) = f.layout.prev_inst(at) {
            search(
                f,
                predecessors,
                slots,
                index,
                tag as i64,
                previous,
                32,
                remaining,
                &mut route,
            )
        } else {
            search_parents(
                f,
                predecessors,
                slots,
                index,
                tag as i64,
                at,
                32,
                remaining,
                &mut route,
            )
        };
        proven && route.witnessed
    })
}

fn verify(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    proof: Proof,
    remaining: &mut usize,
    cycles: bool,
    effects: Effects,
    stores: Option<&[bool]>,
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
    verify_at_with_cycles(
        f,
        predecessors,
        slots,
        proof.store,
        proof.index,
        proof.tag,
        remaining,
        cycles,
        effects,
        stores,
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
    verify_at_with_cycles(
        f,
        predecessors,
        slots,
        at,
        index,
        tag,
        remaining,
        true,
        Effects::Opaque,
        None,
    )
}

pub(super) fn verify_bound_at(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    at: Inst,
    index: u32,
    tag: IrValue,
    remaining: &mut usize,
) -> bool {
    verify_at_with_cycles(
        f,
        predecessors,
        slots,
        at,
        index,
        tag,
        remaining,
        true,
        Effects::BoundMetadata,
        None,
    )
}

fn verify_at_with_cycles(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    at: Inst,
    index: u32,
    tag: IrValue,
    remaining: &mut usize,
    cycles: bool,
    effects: Effects,
    stores: Option<&[bool]>,
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
    let mut route = Route::new(cycles, effects, stores);
    let proven = if let Some(previous) = f.layout.prev_inst(at) {
        search(
            f,
            predecessors,
            slots,
            index,
            tag,
            previous,
            32,
            remaining,
            &mut route,
        )
    } else if cycles {
        search_parents(
            f,
            predecessors,
            slots,
            index,
            tag,
            at,
            32,
            remaining,
            &mut route,
        )
    } else {
        false
    };
    proven && route.witnessed
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
    route: &mut Route<'_>,
) -> bool {
    if depth == 0 || *remaining == 0 {
        return false;
    }
    let level = route.active.len() - depth;
    if route.cycles && route.active[..level].contains(&Some(inst)) {
        *remaining -= 1;
        return true;
    }
    route.active[level] = Some(inst);
    loop {
        let Some(next) = remaining.checked_sub(1) else {
            return false;
        };
        *remaining = next;
        let opcode = f.dfg.insts[inst].opcode();
        if opcode.is_call()
            || (opcode.can_store() && !route.effects.preserves(f, inst, slots, index, route.stores))
        {
            return false;
        }
        if let Some(previous) = f.layout.prev_inst(inst) {
            inst = previous;
            continue;
        }
        return search_parents(
            f,
            predecessors,
            slots,
            index,
            tag,
            inst,
            depth,
            remaining,
            route,
        );
    }
}

fn search_parents(
    f: &Function,
    predecessors: &Predecessors,
    slots: IrValue,
    index: u32,
    tag: i64,
    inst: Inst,
    depth: usize,
    remaining: &mut usize,
    route: &mut Route<'_>,
) -> bool {
    if depth == 0 {
        return false;
    }
    let Some(block) = f.layout.inst_block(inst) else {
        return false;
    };
    if f.layout.entry_block() == Some(block) {
        return false;
    }
    let mut parents = predecessors.pred_iter(block).peekable();
    parents.peek().is_some()
        && parents.all(|parent| {
            if let InstructionData::Brif { arg, blocks, .. } = f.dfg.insts[parent.inst] {
                if blocks[0].block(&f.dfg.value_lists) == block
                    && blocks[1].block(&f.dfg.value_lists) != block
                {
                    if let Some(load) = predicate(f, arg, slots, index, tag, 8, remaining) {
                        let proven = fresh(
                            f,
                            predecessors,
                            load,
                            parent.inst,
                            depth - 1,
                            remaining,
                            Some((route.effects, slots, index, route.stores)),
                        );
                        route.witnessed |= proven;
                        return proven;
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
                route,
            )
        })
}

fn fresh(
    f: &Function,
    predecessors: &Predecessors,
    load: Inst,
    mut inst: Inst,
    depth: usize,
    remaining: &mut usize,
    effect: Option<(Effects, IrValue, u32, Option<&[bool]>)>,
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
        if opcode.is_call()
            || (opcode.can_store()
                && !effect.is_some_and(|(policy, slots, index, stores)| {
                    policy.preserves(f, inst, slots, index, stores)
                }))
        {
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
                fresh(
                    f,
                    predecessors,
                    load,
                    parent.inst,
                    depth - 1,
                    remaining,
                    effect,
                )
            });
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
