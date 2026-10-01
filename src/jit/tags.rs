use allocator_api2::vec::Vec;
use cranelift_codegen::{
    dominator_tree::DominatorTree,
    flowgraph::ControlFlowGraph,
    ir::{
        condcodes::IntCC, types, Block, BlockArg, Function, Inst, InstructionData, Opcode, Value,
        ValueDef,
    },
};

use super::{abi, flow::FlowGraph, resources::BudgetAllocator, JitError};

const ALL: u8 = (1 << (abi::REFERENCE + 1)) - 1;

#[derive(Clone, Copy)]
struct Store {
    inst: Inst,
    allowed: u8,
}

pub(super) struct Stores {
    records: Vec<Store, BudgetAllocator>,
    overflowed: bool,
}

fn refused() -> JitError {
    JitError::ResourceLimit("scalar tag verification")
}

fn invalid() -> JitError {
    JitError::Compilation("invalid scalar tag data flow".into())
}

impl Stores {
    pub fn new(graph: &FlowGraph, allocator: BudgetAllocator) -> Result<Self, JitError> {
        let mut capacity = 0usize;
        for node in &graph.nodes {
            if node.lowering.native() {
                capacity = capacity
                    .checked_add(2 * node.access.writes.count())
                    .ok_or_else(refused)?;
            }
        }
        let mut records = Vec::new_in(allocator);
        records.try_reserve_exact(capacity).map_err(|_| refused())?;
        Ok(Self {
            records,
            overflowed: false,
        })
    }

    pub fn record(&mut self, inst: Inst, allowed: u8) {
        if self.records.len() == self.records.capacity() {
            self.overflowed = true;
            return;
        }
        self.records.push(Store { inst, allowed });
    }

    pub fn verify(
        &self,
        function: &Function,
        slots: Value,
        exit: Option<Value>,
        registers: usize,
    ) -> Result<(), JitError> {
        if registers > 256
            || self.overflowed
            || self
                .records
                .windows(2)
                .any(|pair| pair[0].inst.as_u32() >= pair[1].inst.as_u32())
        {
            return Err(invalid());
        }
        let mut found = 0usize;
        for block in function.layout.blocks() {
            for inst in function.layout.block_insts(block) {
                let opcode = function.dfg.insts[inst].opcode();
                if opcode.can_store() && opcode != Opcode::Store {
                    return Err(invalid());
                }
                let InstructionData::Store { args, offset, .. } = function.dfg.insts[inst] else {
                    continue;
                };
                let address = function.dfg.resolve_aliases(args[1]);
                let offset = i32::from(offset);
                let ty = function.dfg.value_type(args[0]);
                if address != function.dfg.resolve_aliases(slots) {
                    if exit.is_some_and(|exit| address == function.dfg.resolve_aliases(exit))
                        && matches!((offset, ty), (0, types::I64) | (8 | 12, types::I32))
                    {
                        continue;
                    }
                    return Err(invalid());
                }
                if offset < 0 || offset as usize >= registers * 16 || ty != types::I64 {
                    return Err(invalid());
                }
                if offset % 16 == 8 {
                    continue;
                }
                if offset % 16 != 0 {
                    return Err(invalid());
                }
                self.records
                    .binary_search_by_key(&inst.as_u32(), |record| record.inst.as_u32())
                    .map_err(|_| invalid())?;
                found += 1;
            }
        }
        if found != self.records.len() {
            return Err(invalid());
        }
        if found == 0 {
            return Ok(());
        }
        let mut cfg = ControlFlowGraph::new();
        cfg.compute(function);
        let mut dominators = DominatorTree::new();
        dominators.compute(function, &cfg);
        let mut pending = Vec::new_in(self.records.allocator().clone());
        pending
            .try_reserve_exact(function.dfg.num_values())
            .map_err(|_| refused())?;
        let mut seen = Vec::new_in(self.records.allocator().clone());
        seen.try_reserve_exact(function.dfg.num_values())
            .map_err(|_| refused())?;
        seen.resize(function.dfg.num_values(), 0u32);
        let mut analysis = Analysis {
            function,
            cfg: &cfg,
            dominators: &dominators,
            slots,
            registers,
            pending,
            seen,
            epoch: 0,
        };
        for record in &self.records {
            let InstructionData::Store { args, .. } = function.dfg.insts[record.inst] else {
                return Err(invalid());
            };
            let block = function
                .layout
                .inst_block(record.inst)
                .ok_or_else(invalid)?;
            let possible = analysis.possible(args[0], block)?;
            if possible == 0 || possible & !record.allowed != 0 {
                return Err(invalid());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn corrupt_first(&self, function: &mut Function) {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::InstBuilder,
        };
        let inst = self.records.first().expect("missing scalar store").inst;
        let mut cursor = FuncCursor::new(function);
        cursor.goto_inst(inst);
        let tag = cursor.ins().iconst(types::I64, abi::REFERENCE as i64);
        let InstructionData::Store { args, .. } = &mut cursor.func.dfg.insts[inst] else {
            panic!("missing recorded store")
        };
        args[0] = tag;
    }
}

struct Analysis<'a> {
    function: &'a Function,
    cfg: &'a ControlFlowGraph,
    dominators: &'a DominatorTree,
    slots: Value,
    registers: usize,
    pending: Vec<Value, BudgetAllocator>,
    seen: Vec<u32, BudgetAllocator>,
    epoch: u32,
}

impl Analysis<'_> {
    fn push(&mut self, value: Value) {
        let value = self.function.dfg.resolve_aliases(value);
        let seen = &mut self.seen[value.as_u32() as usize];
        if *seen != self.epoch {
            *seen = self.epoch;
            self.pending.push(value);
        }
    }

    fn possible(&mut self, value: Value, store: Block) -> Result<u8, JitError> {
        self.epoch = self.epoch.checked_add(1).ok_or_else(refused)?;
        self.pending.clear();
        self.push(value);
        let mut possible = 0;
        while let Some(value) = self.pending.pop() {
            if self.function.dfg.value_type(value) != types::I64 {
                return Err(invalid());
            }
            match self.function.dfg.value_def(value) {
                ValueDef::Result(inst, 0) => match self.function.dfg.insts[inst] {
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                    } => {
                        let tag = imm.bits();
                        if !(0..=abi::REFERENCE as i64).contains(&tag) {
                            return Err(invalid());
                        }
                        possible |= 1 << tag;
                    }
                    InstructionData::Load {
                        opcode: Opcode::Load,
                        arg,
                        offset,
                        ..
                    } => {
                        let offset = i32::from(offset);
                        if self.function.dfg.resolve_aliases(arg)
                            != self.function.dfg.resolve_aliases(self.slots)
                            || offset < 0
                            || offset % 16 != 0
                            || offset as usize >= self.registers * 16
                        {
                            return Err(invalid());
                        }
                        possible |= self.guarded(value, store);
                    }
                    _ => return Err(invalid()),
                },
                ValueDef::Param(block, index) => {
                    let mut incoming = false;
                    for predecessor in self.cfg.pred_iter(block) {
                        for destination in self.function.dfg.insts[predecessor.inst]
                            .branch_destination(
                                &self.function.dfg.jump_tables,
                                &self.function.dfg.exception_tables,
                            )
                        {
                            if destination.block(&self.function.dfg.value_lists) == block {
                                let Some(BlockArg::Value(value)) =
                                    destination.args(&self.function.dfg.value_lists).nth(index)
                                else {
                                    return Err(invalid());
                                };
                                self.push(value);
                                incoming = true;
                            }
                        }
                    }
                    if !incoming {
                        return Err(invalid());
                    }
                }
                _ => return Err(invalid()),
            }
        }
        Ok(possible)
    }

    fn guarded(&self, value: Value, mut block: Block) -> u8 {
        let mut possible = ALL;
        loop {
            let mut predecessors = self.cfg.pred_iter(block);
            if let Some(predecessor) = predecessors.next() {
                if predecessors.next().is_none() {
                    if let InstructionData::Brif { arg, blocks, .. } =
                        self.function.dfg.insts[predecessor.inst]
                    {
                        let yes = blocks[0].block(&self.function.dfg.value_lists);
                        let no = blocks[1].block(&self.function.dfg.value_lists);
                        if yes != no {
                            let arg = self.function.dfg.resolve_aliases(arg);
                            if let ValueDef::Result(inst, 0) = self.function.dfg.value_def(arg) {
                                if let InstructionData::IntCompare { cond, args, .. } =
                                    self.function.dfg.insts[inst]
                                {
                                    let constant = if self.function.dfg.resolve_aliases(args[0])
                                        == value
                                    {
                                        Some(args[1])
                                    } else if self.function.dfg.resolve_aliases(args[1]) == value {
                                        Some(args[0])
                                    } else {
                                        None
                                    };
                                    if let Some(tag) =
                                        constant.and_then(|constant| self.constant_tag(constant))
                                    {
                                        let bit = 1 << tag;
                                        possible &= match (cond, yes == block) {
                                            (IntCC::Equal, true) | (IntCC::NotEqual, false) => bit,
                                            (IntCC::NotEqual, true) | (IntCC::Equal, false) => {
                                                ALL & !bit
                                            }
                                            _ => ALL,
                                        };
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let Some(parent) = self.dominators.idom(block) else {
                break;
            };
            block = parent;
        }
        possible
    }

    fn constant_tag(&self, value: Value) -> Option<u8> {
        let ValueDef::Result(inst, 0) = self.function.dfg.value_def(value) else {
            return None;
        };
        let InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } = self.function.dfg.insts[inst]
        else {
            return None;
        };
        (0..=abi::REFERENCE as i64)
            .contains(&imm.bits())
            .then_some(imm.bits() as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::super::resources::Ledger;
    use super::*;
    use cranelift_codegen::{
        ir::{AbiParam, InstBuilder, MemFlagsData},
        settings,
    };
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    const SCALAR: u8 = (1 << abi::REFERENCE) - 1;
    const NUMERIC: u8 = (1 << abi::INTEGER) | (1 << abi::NUMBER);

    fn fixture(
        action: impl FnOnce(&mut FunctionBuilder<'_>, Value, Value, &mut Stores),
    ) -> Result<(), JitError> {
        let allocator = BudgetAllocator(Ledger::new(1024 * 1024));
        let mut records = Vec::new_in(allocator);
        records.try_reserve_exact(8).unwrap();
        let mut stores = Stores {
            records,
            overflowed: false,
        };
        let mut function = Function::new();
        function
            .signature
            .params
            .extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
        let mut context = FunctionBuilderContext::new();
        let slots;
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            slots = builder.block_params(entry)[0];
            let choice = builder.block_params(entry)[1];
            action(&mut builder, slots, choice, &mut stores);
            builder.seal_all_blocks();
            let flags = settings::Flags::new(settings::builder());
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(flags)
                .unwrap();
            builder.finalize(isa.frontend_config());
        }
        cranelift_codegen::verify_function(&function, &settings::Flags::new(settings::builder()))
            .unwrap();
        let result = stores.verify(&function, slots, None, 4);
        let ledger = stores.records.allocator().0.clone();
        drop(stores);
        assert_eq!(ledger.current(), 0);
        result
    }

    fn write(
        builder: &mut FunctionBuilder<'_>,
        stores: &mut Stores,
        slots: Value,
        tag: Value,
        allowed: u8,
    ) {
        let inst = builder.ins().store(MemFlagsData::new(), tag, slots, 0);
        stores.record(inst, allowed);
    }

    fn rejected(result: Result<(), JitError>) {
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow"),
            "{result:?}"
        );
    }

    #[test]
    fn only_a_dominating_scalar_guard_admits_a_dynamic_slot_tag() {
        for (cond, on_true, valid) in [
            (IntCC::NotEqual, true, true),
            (IntCC::Equal, false, true),
            (IntCC::Equal, true, false),
            (IntCC::NotEqual, false, false),
        ] {
            let result = fixture(|builder, slots, _, stores| {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 0);
                let scalar = builder.ins().icmp_imm_s(cond, tag, abi::REFERENCE as i64);
                let accepted = builder.create_block();
                let declined = builder.create_block();
                let (yes, no) = if on_true {
                    (accepted, declined)
                } else {
                    (declined, accepted)
                };
                builder.ins().brif(scalar, yes, &[], no, &[]);
                builder.switch_to_block(accepted);
                write(builder, stores, slots, tag, SCALAR);
                builder.ins().return_(&[]);
                builder.switch_to_block(declined);
                builder.ins().return_(&[]);
            });
            if valid {
                result.unwrap();
            } else {
                rejected(result);
            }
        }
        rejected(fixture(|builder, slots, _, stores| {
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, 0);
            write(builder, stores, slots, tag, SCALAR);
            builder.ins().return_(&[]);
        }));
    }

    #[test]
    fn merged_or_duplicate_edges_cannot_launder_a_guard_fact() {
        for duplicate in [false, true] {
            rejected(fixture(|builder, slots, _, stores| {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 0);
                let scalar = builder
                    .ins()
                    .icmp_imm_s(IntCC::NotEqual, tag, abi::REFERENCE as i64);
                let accepted = builder.create_block();
                let declined = if duplicate {
                    accepted
                } else {
                    builder.create_block()
                };
                builder.ins().brif(scalar, accepted, &[], declined, &[]);
                if !duplicate {
                    builder.switch_to_block(declined);
                    builder.ins().jump(accepted, &[]);
                }
                builder.switch_to_block(accepted);
                write(builder, stores, slots, tag, SCALAR);
                builder.ins().return_(&[]);
            }));
        }
    }

    #[test]
    fn unrelated_or_non_tag_loads_do_not_supply_scalar_proofs() {
        for offset in [0, 8, 64] {
            rejected(fixture(|builder, slots, _, stores| {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, offset);
                let tested = if offset == 0 {
                    builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), slots, 16)
                } else {
                    tag
                };
                let scalar =
                    builder
                        .ins()
                        .icmp_imm_s(IntCC::NotEqual, tested, abi::REFERENCE as i64);
                let accepted = builder.create_block();
                let declined = builder.create_block();
                builder.ins().brif(scalar, accepted, &[], declined, &[]);
                builder.switch_to_block(accepted);
                write(builder, stores, slots, tag, SCALAR);
                builder.ins().return_(&[]);
                builder.switch_to_block(declined);
                builder.ins().return_(&[]);
            }));
        }
    }

    #[test]
    fn phi_unions_preserve_opcode_types_and_reject_reference_contamination() {
        for right_tag in [abi::NUMBER, abi::REFERENCE, abi::BOOLEAN] {
            let result = fixture(|builder, slots, choice, stores| {
                let condition = builder.ins().icmp_imm_s(IntCC::NotEqual, choice, 0);
                let left = builder.create_block();
                let right = builder.create_block();
                let join = builder.create_block();
                builder.append_block_param(join, types::I64);
                builder.ins().brif(condition, left, &[], right, &[]);
                builder.switch_to_block(left);
                let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
                builder.ins().jump(join, &[tag.into()]);
                builder.switch_to_block(right);
                let tag = builder.ins().iconst(types::I64, right_tag as i64);
                builder.ins().jump(join, &[tag.into()]);
                builder.switch_to_block(join);
                let tag = builder.block_params(join)[0];
                write(builder, stores, slots, tag, NUMERIC);
                builder.ins().return_(&[]);
            });
            if right_tag == abi::NUMBER {
                result.unwrap();
            } else {
                rejected(result);
            }
        }
    }

    #[test]
    fn cyclic_phis_terminate_and_require_a_valid_seed() {
        fixture(|builder, slots, choice, stores| {
            let condition = builder.ins().icmp_imm_s(IntCC::NotEqual, choice, 0);
            let head = builder.create_block();
            let done = builder.create_block();
            builder.append_block_param(head, types::I64);
            builder.append_block_param(done, types::I64);
            let seed = builder.ins().iconst(types::I64, abi::INTEGER as i64);
            builder.ins().jump(head, &[seed.into()]);
            builder.switch_to_block(head);
            let tag = builder.block_params(head)[0];
            builder
                .ins()
                .brif(condition, head, &[tag.into()], done, &[tag.into()]);
            builder.switch_to_block(done);
            let tag = builder.block_params(done)[0];
            write(builder, stores, slots, tag, NUMERIC);
            builder.ins().return_(&[]);
        })
        .unwrap();
        rejected(fixture(|builder, slots, _, stores| {
            builder.ins().return_(&[]);
            let cycle = builder.create_block();
            builder.append_block_param(cycle, types::I64);
            builder.switch_to_block(cycle);
            let tag = builder.block_params(cycle)[0];
            write(builder, stores, slots, tag, NUMERIC);
            builder.ins().jump(cycle, &[tag.into()]);
        }));
    }

    #[test]
    fn scratch_tag_stores_need_records_and_valid_scalar_constants() {
        for tag in [abi::REFERENCE as i64, -1, 5] {
            rejected(fixture(|builder, slots, _, stores| {
                let tag = builder.ins().iconst(types::I64, tag);
                write(builder, stores, slots, tag, SCALAR);
                builder.ins().return_(&[]);
            }));
        }
        rejected(fixture(|builder, slots, _, _| {
            let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
            builder.ins().store(MemFlagsData::new(), tag, slots, 0);
            builder.ins().return_(&[]);
        }));
    }

    #[test]
    fn verifier_workspace_refusal_is_typed_and_rolls_back_charges() {
        let result = fixture(|builder, slots, _, stores| {
            let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
            write(builder, stores, slots, tag, NUMERIC);
            builder.ins().return_(&[]);
            let ledger = &stores.records.allocator().0;
            ledger.set_limit(ledger.current());
        });
        assert!(matches!(
            result,
            Err(JitError::ResourceLimit("scalar tag verification"))
        ));
    }

    #[test]
    fn partial_workspace_refusal_and_record_overflow_reclaim_storage() {
        let mut retained = None;
        let result = fixture(|builder, slots, _, stores| {
            let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
            write(builder, stores, slots, tag, NUMERIC);
            builder.ins().return_(&[]);
            let ledger = stores.records.allocator().0.clone();
            let pending = builder.func.dfg.num_values() * std::mem::size_of::<Value>();
            let peak = ledger.current() + pending;
            ledger.set_limit(peak);
            retained = Some((ledger, peak));
        });
        assert!(matches!(
            result,
            Err(JitError::ResourceLimit("scalar tag verification"))
        ));
        let (ledger, peak) = retained.unwrap();
        assert_eq!(
            (ledger.current(), ledger.peak(), ledger.refusals()),
            (0, peak, 1)
        );
        rejected(fixture(|builder, slots, _, stores| {
            stores.records = Vec::new_in(stores.records.allocator().clone());
            let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
            write(builder, stores, slots, tag, NUMERIC);
            builder.ins().return_(&[]);
        }));
    }

    #[test]
    fn partial_or_aliased_memory_stores_cannot_bypass_slot_validation() {
        for partial in [false, true] {
            rejected(fixture(|builder, slots, _, stores| {
                let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
                let inst = if partial {
                    builder.ins().istore8(MemFlagsData::new(), tag, slots, 0)
                } else {
                    let address = builder.ins().iadd_imm_s(slots, 16);
                    builder.ins().store(MemFlagsData::new(), tag, address, 0)
                };
                stores.record(inst, NUMERIC);
                builder.ins().return_(&[]);
            }));
        }
    }
}
