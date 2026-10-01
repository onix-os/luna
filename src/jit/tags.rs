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
use crate::opcode::Operation;

const ALL: u8 = (1 << (abi::REFERENCE + 1)) - 1;

#[derive(Clone, Copy)]
struct Store {
    inst: Inst,
    allowed: u8,
}

#[derive(Clone, Copy)]
struct Input {
    inst: Inst,
    tag: Value,
    bits: Value,
    float: bool,
}

pub(super) struct Stores {
    records: Vec<Store, BudgetAllocator>,
    inputs: Vec<Input, BudgetAllocator>,
    expected_inputs: Option<usize>,
    expected_float_inputs: Option<usize>,
    overflowed: bool,
}

fn refused() -> JitError {
    JitError::ResourceLimit("scalar tag verification")
}

fn invalid() -> JitError {
    JitError::Compilation("invalid scalar tag data flow".into())
}

impl Stores {
    pub fn new(graph: &FlowGraph, snapshot: &super::ir::Snapshot) -> Result<Self, JitError> {
        let allocator = snapshot.operations.allocator().clone();
        let mut capacity = 0usize;
        let mut input_capacity = 0usize;
        let mut float_count = 0usize;
        for &op in &snapshot.operations {
            let count = match op {
                Operation::Add { .. }
                | Operation::Sub { .. }
                | Operation::Mul { .. }
                | Operation::Eq { .. }
                | Operation::Less { .. }
                | Operation::LessEq { .. }
                | Operation::NumericForPrep { .. } => 4,
                Operation::Div { .. } => 2,
                Operation::NumericForLoop { .. } => 6,
                _ => 0,
            };
            input_capacity = input_capacity.checked_add(count).ok_or_else(refused)?;
            let floats = match op {
                Operation::Add { .. }
                | Operation::Sub { .. }
                | Operation::Mul { .. }
                | Operation::Div { .. }
                | Operation::NumericForPrep { .. } => 2,
                Operation::NumericForLoop { .. } => 3,
                _ => 0,
            };
            float_count = float_count.checked_add(floats).ok_or_else(refused)?;
        }
        for node in &graph.nodes {
            if node.lowering.native() {
                capacity = capacity
                    .checked_add(2 * node.access.writes.count())
                    .ok_or_else(refused)?;
            }
        }
        let mut records = Vec::new_in(allocator.clone());
        records.try_reserve_exact(capacity).map_err(|_| refused())?;
        let mut inputs = Vec::new_in(allocator);
        inputs
            .try_reserve_exact(input_capacity)
            .map_err(|_| refused())?;
        Ok(Self {
            records,
            inputs,
            expected_inputs: Some(input_capacity),
            expected_float_inputs: Some(float_count),
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

    pub fn numeric_input(&mut self, inst: Inst, tag: Value, bits: Value) {
        self.input(inst, tag, bits, false);
    }

    pub fn float_input(&mut self, inst: Inst, tag: Value, bits: Value) {
        self.input(inst, tag, bits, true);
    }

    fn input(&mut self, inst: Inst, tag: Value, bits: Value, float: bool) {
        if self.inputs.len() == self.inputs.capacity() {
            self.overflowed = true;
            return;
        }
        self.inputs.push(Input {
            inst,
            tag,
            bits,
            float,
        });
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
                .expected_inputs
                .is_some_and(|count| self.inputs.len() != count)
            || self.expected_float_inputs.is_some_and(|count| {
                self.inputs.iter().filter(|input| input.float).count() != count
            })
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
        if found == 0 && self.inputs.is_empty() {
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
            if analysis.unreachable(block) {
                continue;
            }
            let possible = analysis.possible(args[0], block)?;
            if possible == 0 || possible & !record.allowed != 0 {
                return Err(invalid());
            }
        }
        for input in &self.inputs {
            let block = function.layout.inst_block(input.inst).ok_or_else(invalid)?;
            if analysis.unreachable(block) {
                continue;
            }
            let allowed = match function.dfg.insts[input.inst].opcode() {
                Opcode::Iadd | Opcode::Isub | Opcode::Imul | Opcode::SaddOverflow => {
                    1 << abi::INTEGER
                }
                Opcode::Select | Opcode::Band => (1 << abi::INTEGER) | (1 << abi::NUMBER),
                _ => return Err(invalid()),
            };
            let possible = analysis.possible(input.tag, block)?;
            if possible == 0
                || possible & !allowed != 0
                || !analysis.pair(input.tag, input.bits)
                || !analysis.depends(input.inst, input.bits)?
                || !analysis.float_conversion(input, possible)
            {
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

    #[cfg(test)]
    pub fn corrupt_float_first(&self, function: &mut Function, payload: bool) {
        let input = self
            .inputs
            .iter()
            .find(|input| input.float)
            .expect("missing float selector");
        let InstructionData::Ternary { args, .. } = function.dfg.insts[input.inst] else {
            panic!("missing float select operands")
        };
        if payload {
            let conversion = function.dfg.value_def(args[1]).unwrap_inst();
            let InstructionData::Unary { opcode, .. } = &mut function.dfg.insts[conversion] else {
                panic!("missing integer conversion")
            };
            *opcode = Opcode::FcvtFromUint;
        } else {
            let InstructionData::Ternary { args, .. } = &mut function.dfg.insts[input.inst] else {
                unreachable!()
            };
            args.swap(1, 2);
        }
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
    fn float_conversion(&self, input: &Input, possible: u8) -> bool {
        let floating = self.function.dfg.insts[input.inst].opcode() == Opcode::Select
            && self
                .function
                .dfg
                .value_type(self.function.dfg.first_result(input.inst))
                == types::F64;
        if !input.float {
            return !floating;
        }
        if !floating {
            return false;
        }
        let InstructionData::Ternary { args, .. } = self.function.dfg.insts[input.inst] else {
            return false;
        };
        for tag in [abi::INTEGER, abi::NUMBER] {
            if possible & (1 << tag) == 0 {
                continue;
            }
            let mut remaining = 64;
            let Some(Some(integer)) = self.predicate(
                args[0],
                Some(self.function.dfg.resolve_aliases(input.tag)),
                tag,
                &mut remaining,
            ) else {
                return false;
            };
            let value = self
                .function
                .dfg
                .resolve_aliases(args[if integer { 1 } else { 2 }]);
            if self.function.dfg.value_type(value) != types::F64 {
                return false;
            }
            let ValueDef::Result(inst, 0) = self.function.dfg.value_def(value) else {
                return false;
            };
            let operand = match (tag, self.function.dfg.insts[inst]) {
                (
                    abi::INTEGER,
                    InstructionData::Unary {
                        opcode: Opcode::FcvtFromSint,
                        arg,
                    },
                ) => arg,
                (
                    abi::NUMBER,
                    InstructionData::LoadNoOffset {
                        opcode: Opcode::Bitcast,
                        arg,
                        ..
                    },
                ) => arg,
                _ => return false,
            };
            if self.function.dfg.resolve_aliases(operand)
                != self.function.dfg.resolve_aliases(input.bits)
            {
                return false;
            }
        }
        true
    }

    fn fresh(&mut self) -> Result<(), JitError> {
        self.epoch = self.epoch.checked_add(1).ok_or_else(refused)?;
        self.pending.clear();
        Ok(())
    }
    fn push(&mut self, value: Value) {
        let value = self.function.dfg.resolve_aliases(value);
        let seen = &mut self.seen[value.as_u32() as usize];
        if *seen != self.epoch {
            *seen = self.epoch;
            self.pending.push(value);
        }
    }

    fn possible(&mut self, value: Value, store: Block) -> Result<u8, JitError> {
        self.fresh()?;
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
                            for tag in 0..=abi::REFERENCE {
                                let mut remaining = 64;
                                if let Some(Some(truth)) =
                                    self.predicate(arg, Some(value), tag, &mut remaining)
                                {
                                    if truth != (yes == block) {
                                        possible &= !(1 << tag);
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

    fn predicate(
        &self,
        condition: Value,
        queried: Option<Value>,
        tag: u64,
        remaining: &mut usize,
    ) -> Option<Option<bool>> {
        *remaining = remaining.checked_sub(1)?;
        let condition = self.function.dfg.resolve_aliases(condition);
        let ValueDef::Result(inst, 0) = self.function.dfg.value_def(condition) else {
            return None;
        };
        match self.function.dfg.insts[inst] {
            InstructionData::IntCompare { cond, args, .. } => {
                let left = self.literal(args[0], queried, tag);
                let right = self.literal(args[1], queried, tag);
                Some(match (left, right, cond) {
                    (Some(left), Some(right), IntCC::Equal) => Some(left == right),
                    (Some(left), Some(right), IntCC::NotEqual) => Some(left != right),
                    _ => None,
                })
            }
            InstructionData::Binary { opcode, args }
                if matches!(opcode, Opcode::Band | Opcode::Bor | Opcode::Bxor) =>
            {
                let left = self.predicate(args[0], queried, tag, remaining)?;
                let right = self.predicate(args[1], queried, tag, remaining)?;
                Some(match (opcode, left, right) {
                    (Opcode::Band, Some(false), _) | (Opcode::Band, _, Some(false)) => Some(false),
                    (Opcode::Bor, Some(true), _) | (Opcode::Bor, _, Some(true)) => Some(true),
                    (Opcode::Band, Some(left), Some(right)) => Some(left & right),
                    (Opcode::Bor, Some(left), Some(right)) => Some(left | right),
                    (Opcode::Bxor, Some(left), Some(right)) => Some(left ^ right),
                    _ => None,
                })
            }
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } if matches!(imm.bits(), 0 | 1) => Some(Some(imm.bits() == 1)),
            _ => None,
        }
    }

    fn literal(&self, value: Value, queried: Option<Value>, tag: u64) -> Option<u64> {
        if queried.is_some_and(|queried| self.function.dfg.resolve_aliases(value) == queried) {
            return Some(tag);
        }
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
        Some(imm.bits() as u64)
    }

    fn unreachable(&self, mut block: Block) -> bool {
        loop {
            let mut predecessors = self.cfg.pred_iter(block);
            if let Some(predecessor) = predecessors.next() {
                if predecessors.next().is_none() {
                    if let InstructionData::Brif { arg, blocks, .. } =
                        self.function.dfg.insts[predecessor.inst]
                    {
                        let yes = blocks[0].block(&self.function.dfg.value_lists);
                        let no = blocks[1].block(&self.function.dfg.value_lists);
                        let mut remaining = 64;
                        if yes != no {
                            if let Some(Some(truth)) = self.predicate(arg, None, 0, &mut remaining)
                            {
                                if truth != (yes == block) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
            let Some(parent) = self.dominators.idom(block) else {
                return false;
            };
            block = parent;
        }
    }

    fn pair(&self, tag: Value, bits: Value) -> bool {
        if self.function.dfg.value_type(tag) != types::I64
            || self.function.dfg.value_type(bits) != types::I64
        {
            return false;
        }
        let (ValueDef::Result(tag_inst, 0), ValueDef::Result(bits_inst, 0)) = (
            self.function.dfg.value_def(tag),
            self.function.dfg.value_def(bits),
        ) else {
            return false;
        };
        match (
            self.function.dfg.insts[tag_inst],
            self.function.dfg.insts[bits_inst],
        ) {
            (
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    ..
                },
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    ..
                },
            ) => true,
            (
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg: tag_base,
                    offset: tag_offset,
                    ..
                },
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg: bits_base,
                    offset: bits_offset,
                    ..
                },
            ) => {
                self.function.dfg.resolve_aliases(tag_base)
                    == self.function.dfg.resolve_aliases(bits_base)
                    && i32::from(tag_offset).checked_add(8) == Some(i32::from(bits_offset))
                    && self.function.layout.next_inst(tag_inst) == Some(bits_inst)
            }
            _ => false,
        }
    }

    fn depends(&mut self, inst: Inst, bits: Value) -> Result<bool, JitError> {
        self.fresh()?;
        let bits = self.function.dfg.resolve_aliases(bits);
        for &value in self.function.dfg.inst_args(inst) {
            self.push(value);
        }
        while let Some(value) = self.pending.pop() {
            if value == bits {
                return Ok(true);
            }
            match self.function.dfg.value_def(value) {
                ValueDef::Result(inst, _) => {
                    for &value in self.function.dfg.inst_args(inst) {
                        self.push(value);
                    }
                }
                ValueDef::Param(block, index) => {
                    for predecessor in self.cfg.pred_iter(block) {
                        for destination in self.function.dfg.insts[predecessor.inst]
                            .branch_destination(
                                &self.function.dfg.jump_tables,
                                &self.function.dfg.exception_tables,
                            )
                        {
                            if destination.block(&self.function.dfg.value_lists) == block {
                                if let Some(BlockArg::Value(value)) =
                                    destination.args(&self.function.dfg.value_lists).nth(index)
                                {
                                    self.push(value);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
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
            expected_inputs: None,
            expected_float_inputs: None,
            inputs: {
                let mut inputs = Vec::new_in(records.allocator().clone());
                inputs.try_reserve_exact(32).unwrap();
                inputs
            },
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

    fn numeric_use(
        builder: &mut FunctionBuilder<'_>,
        stores: &mut Stores,
        tag: Value,
        bits: Value,
        integer: bool,
    ) {
        let result = if integer {
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().iadd(bits, one)
        } else {
            let is_integer = builder
                .ins()
                .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
            let integer = builder.ins().fcvt_from_sint(types::F64, bits);
            let float = builder.ins().bitcast(types::F64, MemFlagsData::new(), bits);
            builder.ins().select(is_integer, integer, float)
        };
        let inst = builder.func.dfg.value_def(result).unwrap_inst();
        if integer {
            stores.numeric_input(inst, tag, bits);
        } else {
            stores.float_input(inst, tag, bits);
        }
    }

    #[test]
    fn compound_numeric_guards_admit_correct_consumers_on_both_edges() {
        for (integer, inverted) in [(false, false), (false, true), (true, false)] {
            fixture(|builder, slots, choice, stores| {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 0);
                let bits = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 8);
                let is_integer = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
                let second = if integer {
                    builder.ins().icmp_imm_s(IntCC::NotEqual, choice, 0)
                } else {
                    builder
                        .ins()
                        .icmp_imm_s(IntCC::Equal, tag, abi::NUMBER as i64)
                };
                let mut condition = if integer {
                    builder.ins().band(is_integer, second)
                } else {
                    builder.ins().bor(is_integer, second)
                };
                if inverted {
                    condition = builder.ins().bxor_imm_u(condition, 1);
                }
                let accepted = builder.create_block();
                let declined = builder.create_block();
                let (yes, no) = if inverted {
                    (declined, accepted)
                } else {
                    (accepted, declined)
                };
                builder.ins().brif(condition, yes, &[], no, &[]);
                builder.switch_to_block(accepted);
                numeric_use(builder, stores, tag, bits, integer);
                builder.ins().return_(&[]);
                builder.switch_to_block(declined);
                builder.ins().return_(&[]);
            })
            .unwrap();
        }
    }

    #[test]
    fn numeric_consumers_reject_missing_or_scalar_only_guards() {
        for scalar_guard in [false, true] {
            rejected(fixture(|builder, slots, _, stores| {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 0);
                let bits = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 8);
                let accepted = builder.create_block();
                let declined = builder.create_block();
                let condition = if scalar_guard {
                    builder
                        .ins()
                        .icmp_imm_s(IntCC::NotEqual, tag, abi::REFERENCE as i64)
                } else {
                    builder.ins().iconst(types::I8, 1)
                };
                builder.ins().brif(condition, accepted, &[], declined, &[]);
                builder.switch_to_block(accepted);
                numeric_use(builder, stores, tag, bits, false);
                builder.ins().return_(&[]);
                builder.switch_to_block(declined);
                builder.ins().return_(&[]);
            }));
        }
    }

    #[test]
    fn input_tags_and_payloads_must_be_adjacent_reads_of_the_same_slot() {
        for (offset, intervening) in [(8, false), (24, false), (8, true)] {
            let result = fixture(|builder, slots, _, stores| {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 0);
                if intervening {
                    builder.ins().iconst(types::I64, 0);
                }
                let bits = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, offset);
                let condition = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
                let accepted = builder.create_block();
                let declined = builder.create_block();
                builder.ins().brif(condition, accepted, &[], declined, &[]);
                builder.switch_to_block(accepted);
                numeric_use(builder, stores, tag, bits, true);
                builder.ins().return_(&[]);
                builder.switch_to_block(declined);
                builder.ins().return_(&[]);
            });
            if offset == 8 && !intervening {
                result.unwrap();
            } else {
                rejected(result);
            }
        }
    }

    #[test]
    fn integer_consumers_require_integer_tags_and_the_recorded_payload() {
        for (tag_value, consumed) in [
            (abi::INTEGER, true),
            (abi::NUMBER, true),
            (abi::INTEGER, false),
        ] {
            let result = fixture(|builder, _, _, stores| {
                let tag = builder.ins().iconst(types::I64, tag_value as i64);
                let bits = builder.ins().iconst(types::I64, 42);
                let one = builder.ins().iconst(types::I64, 1);
                let actual = if consumed { bits } else { one };
                let result = builder.ins().iadd(actual, one);
                stores.numeric_input(builder.func.dfg.value_def(result).unwrap_inst(), tag, bits);
                builder.ins().return_(&[]);
            });
            if tag_value == abi::INTEGER && consumed {
                result.unwrap();
            } else {
                rejected(result);
            }
        }
    }

    #[test]
    fn unsupported_non_boolean_operands_cannot_launder_guard_facts() {
        rejected(fixture(|builder, slots, choice, stores| {
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, 0);
            let bits = builder
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, 8);
            let integer = builder
                .ins()
                .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
            let unknown = builder.ins().ireduce(types::I8, choice);
            let either = builder.ins().bor(integer, unknown);
            let condition = builder.ins().bxor_imm_u(either, 1);
            let accepted = builder.create_block();
            let declined = builder.create_block();
            builder.ins().brif(condition, declined, &[], accepted, &[]);
            builder.switch_to_block(accepted);
            numeric_use(builder, stores, tag, bits, true);
            builder.ins().return_(&[]);
            builder.switch_to_block(declined);
            builder.ins().return_(&[]);
        }));
    }

    #[test]
    fn bounded_predicates_refuse_deep_proofs_without_unbounded_recursion() {
        rejected(fixture(|builder, slots, _, stores| {
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, 0);
            let bits = builder
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, 8);
            let mut condition = builder
                .ins()
                .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
            for _ in 0..70 {
                condition = builder.ins().bxor_imm_u(condition, 0);
            }
            let accepted = builder.create_block();
            let declined = builder.create_block();
            builder.ins().brif(condition, accepted, &[], declined, &[]);
            builder.switch_to_block(accepted);
            numeric_use(builder, stores, tag, bits, true);
            builder.ins().return_(&[]);
            builder.switch_to_block(declined);
            builder.ins().return_(&[]);
        }));
    }

    #[test]
    fn constant_false_dominated_paths_are_vacuous_not_compiler_failures() {
        fixture(|builder, _, _, stores| {
            let tag = builder.ins().iconst(types::I64, abi::REFERENCE as i64);
            let bits = builder.ins().iconst(types::I64, 0);
            let condition = builder
                .ins()
                .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
            let impossible = builder.create_block();
            let declined = builder.create_block();
            builder
                .ins()
                .brif(condition, impossible, &[], declined, &[]);
            builder.switch_to_block(impossible);
            numeric_use(builder, stores, tag, bits, true);
            builder.ins().return_(&[]);
            builder.switch_to_block(declined);
            builder.ins().return_(&[]);
        })
        .unwrap();
    }

    #[derive(Clone, Copy)]
    enum FloatFault {
        None,
        Swapped,
        WrongTag,
        Unknown,
        NumberCondition,
        Unsigned,
        IntegerPayload,
        NumberPayload,
        DeepPredicate,
    }

    fn float_fixture(
        fault: FloatFault,
        inverted: bool,
        constant: Option<u64>,
    ) -> Result<(), JitError> {
        fixture(|builder, slots, choice, stores| {
            let (tag, bits) = if let Some(tag) = constant {
                (
                    builder.ins().iconst(types::I64, tag as i64),
                    builder.ins().iconst(types::I64, -41),
                )
            } else {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 0);
                let bits = builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 8);
                let integer = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
                let number = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, tag, abi::NUMBER as i64);
                let numeric = builder.ins().bor(integer, number);
                let accepted = builder.create_block();
                let declined = builder.create_block();
                builder.ins().brif(numeric, accepted, &[], declined, &[]);
                builder.switch_to_block(declined);
                builder.ins().return_(&[]);
                builder.switch_to_block(accepted);
                (tag, bits)
            };
            let tested = if matches!(fault, FloatFault::WrongTag) {
                builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slots, 16)
            } else {
                tag
            };
            let mut condition = if matches!(fault, FloatFault::Unknown) {
                builder.ins().icmp_imm_s(IntCC::NotEqual, choice, 0)
            } else {
                let wanted = if matches!(fault, FloatFault::NumberCondition) {
                    abi::NUMBER
                } else {
                    abi::INTEGER
                };
                builder.ins().icmp_imm_s(
                    if inverted {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    },
                    tested,
                    wanted as i64,
                )
            };
            if matches!(fault, FloatFault::DeepPredicate) {
                for _ in 0..70 {
                    condition = builder.ins().bxor_imm_u(condition, 0);
                }
            }
            let integer_bits = if matches!(fault, FloatFault::IntegerPayload) {
                choice
            } else {
                bits
            };
            let integer = if matches!(fault, FloatFault::Unsigned) {
                builder.ins().fcvt_from_uint(types::F64, integer_bits)
            } else {
                builder.ins().fcvt_from_sint(types::F64, integer_bits)
            };
            let number_bits = if matches!(fault, FloatFault::NumberPayload) {
                choice
            } else {
                bits
            };
            let number = builder
                .ins()
                .bitcast(types::F64, MemFlagsData::new(), number_bits);
            let swap = inverted ^ matches!(fault, FloatFault::Swapped);
            let (yes, no) = if swap {
                (number, integer)
            } else {
                (integer, number)
            };
            let result = builder.ins().select(condition, yes, no);
            stores.float_input(builder.func.dfg.value_def(result).unwrap_inst(), tag, bits);
            builder.ins().return_(&[]);
        })
    }

    #[test]
    fn float_selectors_preserve_both_numeric_tags_and_equivalent_polarities() {
        for constant in [None, Some(abi::INTEGER), Some(abi::NUMBER)] {
            for inverted in [false, true] {
                float_fixture(FloatFault::None, inverted, constant).unwrap();
            }
        }
    }

    #[test]
    fn float_selectors_reject_wrong_tags_unknown_conditions_and_swapped_arms() {
        for fault in [
            FloatFault::Swapped,
            FloatFault::WrongTag,
            FloatFault::Unknown,
            FloatFault::NumberCondition,
        ] {
            for inverted in [false, true] {
                rejected(float_fixture(fault, inverted, None));
            }
        }
        for tag in [abi::INTEGER, abi::NUMBER] {
            rejected(float_fixture(FloatFault::Swapped, false, Some(tag)));
        }
    }

    #[test]
    fn float_conversions_reject_unsigned_and_foreign_payloads() {
        for fault in [
            FloatFault::Unsigned,
            FloatFault::IntegerPayload,
            FloatFault::NumberPayload,
        ] {
            rejected(float_fixture(fault, false, None));
        }
        rejected(float_fixture(
            FloatFault::Unsigned,
            false,
            Some(abi::INTEGER),
        ));
        rejected(float_fixture(
            FloatFault::NumberPayload,
            false,
            Some(abi::NUMBER),
        ));
    }

    #[test]
    fn float_selector_proof_is_bounded_and_checks_only_possible_tags() {
        rejected(float_fixture(FloatFault::DeepPredicate, false, None));
        float_fixture(FloatFault::Unsigned, false, Some(abi::NUMBER)).unwrap();
        float_fixture(FloatFault::NumberPayload, false, Some(abi::INTEGER)).unwrap();
    }

    #[test]
    fn float_obligations_cannot_be_removed_or_reclassified_by_emitted_shape() {
        for (floating, marked, count) in [
            (true, false, None),
            (false, true, None),
            (true, true, Some(0)),
            (false, false, Some(1)),
        ] {
            rejected(fixture(|builder, _, _, stores| {
                let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
                let bits = builder.ins().iconst(types::I64, -41);
                numeric_use(builder, stores, tag, bits, !floating);
                stores.inputs.last_mut().unwrap().float = marked;
                stores.expected_float_inputs = count;
                builder.ins().return_(&[]);
            }));
        }
    }

    #[test]
    fn missing_input_records_and_input_growth_are_compiler_errors() {
        for missing in [true, false] {
            rejected(fixture(|builder, _, _, stores| {
                if missing {
                    stores.expected_inputs = Some(1);
                } else {
                    stores.inputs = Vec::new_in(stores.inputs.allocator().clone());
                }
                if !missing {
                    let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
                    let bits = builder.ins().iconst(types::I64, 42);
                    numeric_use(builder, stores, tag, bits, true);
                }
                builder.ins().return_(&[]);
            }));
        }
    }
}
