use allocator_api2::vec::Vec;
use cranelift_codegen::{
    dominator_tree::DominatorTree,
    flowgraph::ControlFlowGraph,
    ir::{
        condcodes::IntCC, types, Block, BlockArg, Function, Inst, InstructionData, MemFlagsData,
        Opcode, Value, ValueDef,
    },
};

use super::{abi, flow::FlowGraph, resources::BudgetAllocator, JitError};
use crate::opcode::{Operation, RCIndex};

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

#[derive(Clone, Copy)]
struct Arithmetic {
    pc: usize,
    result: Value,
    inputs: [(Value, Value); 2],
    store: Inst,
    floating: bool,
}

#[derive(Clone, Copy)]
struct Truth {
    pc: usize,
    tag: Value,
    bits: Value,
    point: Inst,
}

#[derive(Clone, Copy)]
pub(super) struct Comparison {
    pub pc: usize,
    pub inputs: [(Value, Value); 2],
    pub same: Value,
    pub mixed: Value,
    pub split: Inst,
    pub branch: Inst,
}

#[derive(Clone, Copy)]
pub(super) struct ForPrep {
    pub pc: usize,
    pub inputs: [(Value, Value); 2],
    pub nonzero: Inst,
    pub split: Inst,
    pub stores: [Inst; 2],
    pub next: [Inst; 2],
}

#[derive(Clone, Copy)]
struct TransferWrite {
    pc: usize,
    ordinal: usize,
    inst: Inst,
}

#[derive(Clone, Copy)]
struct TransferEdge {
    pc: usize,
    inst: Inst,
    split: Option<Inst>,
}

#[derive(Clone, Copy)]
pub(super) struct LoopArm {
    pub tag: Value,
    pub bits: Value,
    pub condition: Value,
    pub next: Inst,
}

#[derive(Clone, Copy)]
pub(super) struct ForLoop {
    pub pc: usize,
    pub inputs: [(Value, Value); 3],
    pub split: Inst,
    pub arms: [LoopArm; 2],
    pub store: Inst,
    pub branch: Inst,
    pub visible_store: Inst,
    pub next: [Inst; 2],
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum ComparisonCorruption {
    SameCondition,
    MixedGuard,
    MixedBound,
    Split,
    Phi,
    Targets,
    Count,
    Source,
    Polarity,
    Missing,
    ProgramCounter,
    Growth,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LoopCorruption {
    IndexOpcode,
    FloatOpcode,
    Guard,
    Split,
    Overflow,
    Direction,
    Limit,
    Phi,
    Source,
    Store,
    VisibleStore,
    Targets,
    Count,
    Missing,
    ProgramCounter,
    Growth,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TransferCorruption {
    Payload,
    Source,
    Destination,
    Split,
    Targets,
    Count,
    MissingWrite,
    MissingEdge,
    ProgramCounter,
    Ordinal,
    GrowthWrite,
    GrowthEdge,
    ExtraStore,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum TruthCorruption {
    Payload,
    Source,
    Condition,
    Targets,
    Count,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum ArithmeticCorruption {
    Opcode,
    Operands,
    Source,
    Destination,
}

pub(super) struct Stores {
    pub helper_calls: super::helper_flow::Calls,
    transfer_writes: Vec<TransferWrite, BudgetAllocator>,
    transfer_edges: Vec<TransferEdge, BudgetAllocator>,
    expected_transfers: Option<(usize, usize)>,
    preps: Vec<ForPrep, BudgetAllocator>,
    loops: Vec<ForLoop, BudgetAllocator>,
    expected_preps: Option<usize>,
    expected_loops: Option<usize>,
    comparisons: Vec<Comparison, BudgetAllocator>,
    expected_comparisons: Option<usize>,
    truths: Vec<Truth, BudgetAllocator>,
    expected_truths: Option<usize>,
    records: Vec<Store, BudgetAllocator>,
    inputs: Vec<Input, BudgetAllocator>,
    arithmetic: Vec<Arithmetic, BudgetAllocator>,
    expected_arithmetic: Option<usize>,
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
        let mut arithmetic_count = 0usize;
        let mut truth_count = 0usize;
        let mut comparison_count = 0usize;
        let mut prep_count = 0usize;
        let mut loop_count = 0usize;
        let mut transfer_writes = 0usize;
        let mut transfer_edges = 0usize;
        for &op in &snapshot.operations {
            if let Some(writes) = scalar_transfer_writes(op, snapshot) {
                transfer_writes = transfer_writes.checked_add(writes).ok_or_else(refused)?;
                transfer_edges = transfer_edges.checked_add(1).ok_or_else(refused)?;
            }
            if matches!(op, Operation::NumericForPrep { .. }) {
                prep_count = prep_count.checked_add(1).ok_or_else(refused)?;
            }
            if matches!(op, Operation::NumericForLoop { .. }) {
                loop_count = loop_count.checked_add(1).ok_or_else(refused)?;
            }
            if matches!(
                op,
                Operation::Eq { .. } | Operation::Less { .. } | Operation::LessEq { .. }
            ) {
                comparison_count = comparison_count.checked_add(1).ok_or_else(refused)?;
            }
            if matches!(op, Operation::Not { .. } | Operation::Test { .. }) {
                truth_count = truth_count.checked_add(1).ok_or_else(refused)?;
            }
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
            arithmetic_count = arithmetic_count
                .checked_add(match op {
                    Operation::Add { .. } | Operation::Sub { .. } | Operation::Mul { .. } => 2,
                    Operation::Div { .. } => 1,
                    _ => 0,
                })
                .ok_or_else(refused)?;
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
        let mut inputs = Vec::new_in(allocator.clone());
        inputs
            .try_reserve_exact(input_capacity)
            .map_err(|_| refused())?;
        let mut arithmetic = Vec::new_in(allocator.clone());
        arithmetic
            .try_reserve_exact(arithmetic_count)
            .map_err(|_| refused())?;
        let mut truths = Vec::new_in(allocator.clone());
        truths
            .try_reserve_exact(truth_count)
            .map_err(|_| refused())?;
        let mut comparisons = Vec::new_in(allocator.clone());
        comparisons
            .try_reserve_exact(comparison_count)
            .map_err(|_| refused())?;
        let mut preps = Vec::new_in(allocator.clone());
        preps.try_reserve_exact(prep_count).map_err(|_| refused())?;
        let mut loops = Vec::new_in(allocator.clone());
        loops.try_reserve_exact(loop_count).map_err(|_| refused())?;
        let mut writes = Vec::new_in(allocator.clone());
        writes
            .try_reserve_exact(transfer_writes)
            .map_err(|_| refused())?;
        let mut edges = Vec::new_in(allocator);
        edges
            .try_reserve_exact(transfer_edges)
            .map_err(|_| refused())?;
        Ok(Self {
            helper_calls: super::helper_flow::Calls::new(snapshot)?,
            transfer_writes: writes,
            transfer_edges: edges,
            expected_transfers: Some((transfer_writes, transfer_edges)),
            preps,
            loops,
            expected_preps: Some(prep_count),
            expected_loops: Some(loop_count),
            comparisons,
            expected_comparisons: Some(comparison_count),
            truths,
            expected_truths: Some(truth_count),
            records,
            inputs,
            arithmetic,
            expected_arithmetic: Some(arithmetic_count),
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

    pub fn truth(&mut self, pc: usize, tag: Value, bits: Value, point: Inst) {
        if self.truths.len() == self.truths.capacity() {
            self.overflowed = true;
            return;
        }
        self.truths.push(Truth {
            pc,
            tag,
            bits,
            point,
        });
    }

    pub fn comparison(&mut self, record: Comparison) {
        if self.comparisons.len() == self.comparisons.capacity() {
            self.overflowed = true;
            return;
        }
        self.comparisons.push(record);
    }

    pub fn for_prep(&mut self, record: ForPrep) {
        if self.preps.len() == self.preps.capacity() {
            self.overflowed = true;
            return;
        }
        self.preps.push(record);
    }

    pub fn for_loop(&mut self, record: ForLoop) {
        if self.loops.len() == self.loops.capacity() {
            self.overflowed = true;
            return;
        }
        self.loops.push(record);
    }

    pub fn transfer_write(&mut self, pc: usize, ordinal: usize, inst: Inst) {
        if self.transfer_writes.len() == self.transfer_writes.capacity() {
            self.overflowed = true;
            return;
        }
        self.transfer_writes
            .push(TransferWrite { pc, ordinal, inst });
    }

    pub fn transfer_edge(&mut self, pc: usize, inst: Inst, split: Option<Inst>) {
        if self.transfer_edges.len() == self.transfer_edges.capacity() {
            self.overflowed = true;
            return;
        }
        self.transfer_edges.push(TransferEdge { pc, inst, split });
    }

    pub fn arithmetic(
        &mut self,
        pc: usize,
        result: Value,
        inputs: [(Value, Value); 2],
        store: Inst,
        floating: bool,
    ) {
        if self.arithmetic.len() == self.arithmetic.capacity() {
            self.overflowed = true;
            return;
        }
        self.arithmetic.push(Arithmetic {
            pc,
            result,
            inputs,
            store,
            floating,
        });
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
            || self.expected_transfers.is_some_and(|counts| {
                counts != (self.transfer_writes.len(), self.transfer_edges.len())
            })
            || self
                .expected_preps
                .is_some_and(|count| self.preps.len() != count)
            || self
                .expected_loops
                .is_some_and(|count| self.loops.len() != count)
            || self
                .expected_comparisons
                .is_some_and(|count| self.comparisons.len() != count)
            || self
                .expected_truths
                .is_some_and(|count| self.truths.len() != count)
            || self
                .expected_arithmetic
                .is_some_and(|count| self.arithmetic.len() != count)
            || self
                .inputs
                .windows(2)
                .any(|pair| pair[0].inst.as_u32() > pair[1].inst.as_u32())
            || self
                .inputs
                .windows(3)
                .any(|group| group[0].inst == group[2].inst)
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
    pub fn corrupt_transfer(
        &mut self,
        function: &mut Function,
        fault: TransferCorruption,
        registers: usize,
    ) {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::InstBuilder,
        };
        use TransferCorruption::*;
        let edge = if matches!(fault, Source | Split) {
            *self
                .transfer_edges
                .iter()
                .find(|edge| edge.split.is_some())
                .expect("missing scalar move")
        } else {
            self.transfer_edges[0]
        };
        match fault {
            MissingWrite => {
                self.transfer_writes.clear();
                return;
            }
            MissingEdge => {
                self.transfer_edges.clear();
                return;
            }
            ProgramCounter => {
                self.transfer_edges[0].pc += 1;
                return;
            }
            Ordinal => {
                self.transfer_writes[0].ordinal += 1;
                return;
            }
            GrowthWrite => {
                let record = self.transfer_writes[0];
                self.transfer_write(record.pc, record.ordinal, record.inst);
                return;
            }
            GrowthEdge => {
                self.transfer_edge(edge.pc, edge.inst, edge.split);
                return;
            }
            Targets | Count => {
                let InstructionData::Jump { destination, .. } = function.dfg.insts[edge.inst]
                else {
                    unreachable!()
                };
                let arg = destination.args(&function.dfg.value_lists).last().unwrap();
                if fault == Count {
                    let BlockArg::Value(value) = arg else {
                        unreachable!()
                    };
                    let inst = function.dfg.value_def(value).unwrap_inst();
                    let InstructionData::Binary { opcode, .. } = &mut function.dfg.insts[inst]
                    else {
                        unreachable!()
                    };
                    *opcode = Opcode::Isub;
                } else {
                    let target = function
                        .layout
                        .blocks()
                        .find(|block| {
                            *block != destination.block(&function.dfg.value_lists)
                                && function.dfg.block_params(*block).len() == 1
                                && function
                                    .dfg
                                    .value_type(function.dfg.block_params(*block)[0])
                                    == types::I32
                        })
                        .unwrap();
                    let call = cranelift_codegen::ir::BlockCall::new(
                        target,
                        [arg],
                        &mut function.dfg.value_lists,
                    );
                    let InstructionData::Jump { destination, .. } =
                        &mut function.dfg.insts[edge.inst]
                    else {
                        unreachable!()
                    };
                    *destination = call;
                }
                return;
            }
            Split => {
                let InstructionData::Brif { blocks, .. } =
                    &mut function.dfg.insts[edge.split.unwrap()]
                else {
                    unreachable!()
                };
                blocks.swap(0, 1);
                return;
            }
            _ => {}
        }
        let store = self
            .transfer_writes
            .iter()
            .find(|record| record.pc == edge.pc)
            .unwrap()
            .inst;
        let InstructionData::Store { args, offset, .. } = function.dfg.insts[store] else {
            unreachable!()
        };
        let prior = function.layout.prev_inst(store).unwrap();
        match fault {
            Payload => {
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(store);
                let wrong = cursor.ins().iconst(types::I64, 123);
                let InstructionData::Store { args, .. } = &mut cursor.func.dfg.insts[store] else {
                    unreachable!()
                };
                args[0] = wrong;
            }
            Source => {
                let InstructionData::Store { args: tag_args, .. } = function.dfg.insts[prior]
                else {
                    unreachable!()
                };
                for value in [tag_args[0], args[0]] {
                    let inst = function.dfg.value_def(value).unwrap_inst();
                    let InstructionData::Load { offset, .. } = &mut function.dfg.insts[inst] else {
                        unreachable!()
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
            Destination => {
                for inst in [prior, store] {
                    let InstructionData::Store { offset, .. } = &mut function.dfg.insts[inst]
                    else {
                        unreachable!()
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
            ExtraStore => {
                let InstructionData::Store { args: tag_args, .. } = function.dfg.insts[prior]
                else {
                    unreachable!()
                };
                let allowed = self
                    .records
                    .iter()
                    .find(|record| record.inst == prior)
                    .unwrap()
                    .allowed;
                let offset = (i32::from(offset) - 8 + 16) % (registers as i32 * 16);
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(edge.inst);
                let point = cursor
                    .ins()
                    .store(MemFlagsData::new(), tag_args[0], args[1], offset);
                cursor
                    .ins()
                    .store(MemFlagsData::new(), args[0], args[1], offset + 8);
                self.record(point, allowed);
            }
            _ => unreachable!(),
        }
    }

    pub fn verify_transfers(
        &self,
        function: &Function,
        slots: Value,
        snapshot: &super::ir::Snapshot,
        blocks: &[Block],
        fallback: Block,
    ) -> Result<(), JitError> {
        if blocks.len() != snapshot.operations.len() {
            return Err(invalid());
        }
        let mut cfg = ControlFlowGraph::new();
        if self
            .transfer_edges
            .iter()
            .any(|record| record.split.is_some())
        {
            cfg.compute(function);
        }
        let mut writes = 0usize;
        let mut edges = self.transfer_edges.iter();
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            let Some(count) = scalar_transfer_writes(op, snapshot) else {
                continue;
            };
            let edge = edges.next().ok_or_else(invalid)?;
            if edge.pc != pc {
                return Err(invalid());
            }
            let block = function.layout.inst_block(edge.inst).ok_or_else(invalid)?;
            if function.layout.last_inst(block) != Some(edge.inst) {
                return Err(invalid());
            }
            let end = writes.checked_add(count).ok_or_else(invalid)?;
            let records = self.transfer_writes.get(writes..end).ok_or_else(invalid)?;
            for (ordinal, record) in records.iter().enumerate() {
                if record.pc != pc
                    || record.ordinal != ordinal
                    || function.layout.inst_block(record.inst) != Some(block)
                {
                    return Err(invalid());
                }
                let destination = match op {
                    Operation::Move { dest, .. }
                    | Operation::LoadConstant { dest, .. }
                    | Operation::LoadBool { dest, .. } => dest.0,
                    Operation::LoadNil { dest, .. } => dest
                        .0
                        .checked_add(u8::try_from(ordinal).map_err(|_| invalid())?)
                        .ok_or_else(invalid)?,
                    _ => return Err(invalid()),
                };
                let (tag, bits) = payload_store(function, record.inst, slots, destination)?;
                let valid = match op {
                    Operation::Move { source, .. } => {
                        let split = edge.split.ok_or_else(invalid)?;
                        let InstructionData::Brif {
                            arg, blocks: arms, ..
                        } = function.dfg.insts[split]
                        else {
                            return Err(invalid());
                        };
                        let ValueDef::Result(test, 0) =
                            function.dfg.value_def(function.dfg.resolve_aliases(arg))
                        else {
                            return Err(invalid());
                        };
                        let InstructionData::IntCompare {
                            opcode: Opcode::Icmp,
                            cond: IntCC::NotEqual,
                            args,
                        } = function.dfg.insts[test]
                        else {
                            return Err(invalid());
                        };
                        source_operand(
                            function,
                            slots,
                            snapshot,
                            RCIndex::Register(source),
                            tag,
                            bits,
                        ) && same_value(function, args[0], tag)
                            && literal(function, args[1]) == Some(abi::REFERENCE)
                            && arms[0].block(&function.dfg.value_lists) == block
                            && arms[1].block(&function.dfg.value_lists) != block
                            && arms[0].args(&function.dfg.value_lists).next().is_none()
                            && single_predecessor(&cfg, block, split)
                    }
                    Operation::LoadConstant { constant, .. } => snapshot
                        .constants
                        .get(usize::from(constant.0))
                        .is_some_and(|slot| {
                            slot.tag != abi::REFERENCE
                                && literal(function, tag) == Some(slot.tag)
                                && literal(function, bits) == Some(slot.bits)
                        }),
                    Operation::LoadBool { value, .. } => {
                        literal(function, tag) == Some(abi::BOOLEAN)
                            && literal(function, bits) == Some(u64::from(value))
                    }
                    Operation::LoadNil { .. } => {
                        literal(function, tag) == Some(abi::NIL)
                            && literal(function, bits) == Some(0)
                    }
                    _ => false,
                };
                if !valid {
                    return Err(invalid());
                }
            }
            if matches!(op, Operation::Move { .. }) != edge.split.is_some() {
                return Err(invalid());
            }
            for inst in function.layout.block_insts(block) {
                if function.dfg.insts[inst].opcode().can_store()
                    && !records.iter().any(|record| {
                        record.inst == inst || function.layout.prev_inst(record.inst) == Some(inst)
                    })
                {
                    return Err(invalid());
                }
            }
            let target = match op {
                Operation::LoadBool { skip_next, .. } => pc + 1 + usize::from(skip_next),
                Operation::Jump {
                    offset,
                    close_upvalues,
                } if close_upvalues.is_none() => (pc + 1)
                    .checked_add_signed(isize::from(offset))
                    .ok_or_else(invalid)?,
                _ => pc + 1,
            };
            let fuel = *function
                .dfg
                .block_params(blocks[pc])
                .first()
                .ok_or_else(invalid)?;
            loop_advance(function, edge.inst, blocks, target, fuel, fallback)?;
            writes = end;
        }
        if writes != self.transfer_writes.len() || edges.next().is_some() {
            return Err(invalid());
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn corrupt_loop(
        &mut self,
        function: &mut Function,
        prep: bool,
        fault: LoopCorruption,
        registers: usize,
    ) {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::InstBuilder,
        };
        use LoopCorruption::*;
        match fault {
            Missing => {
                if prep {
                    self.preps.clear();
                } else {
                    self.loops.clear();
                }
                return;
            }
            ProgramCounter => {
                if prep {
                    self.preps[0].pc += 1;
                } else {
                    self.loops[0].pc += 1;
                }
                return;
            }
            Growth => {
                if prep {
                    self.for_prep(self.preps[0]);
                } else {
                    self.for_loop(self.loops[0]);
                }
                return;
            }
            _ => {}
        }
        let (split, store, next, source) = if prep {
            let record = self.preps[0];
            (
                record.split,
                record.stores[0],
                record.next[0],
                record.inputs[0],
            )
        } else {
            let record = self.loops[0];
            (record.split, record.store, record.next[0], record.inputs[0])
        };
        match fault {
            IndexOpcode => {
                let value = if prep {
                    let InstructionData::Store { args, .. } = function.dfg.insts[store] else {
                        unreachable!()
                    };
                    args[0]
                } else {
                    self.loops[0].arms[0].bits
                };
                let inst = function.dfg.value_def(value).unwrap_inst();
                let InstructionData::Binary { opcode, .. } = &mut function.dfg.insts[inst] else {
                    unreachable!()
                };
                *opcode = if prep {
                    Opcode::Iadd
                } else {
                    Opcode::UaddOverflow
                };
            }
            FloatOpcode => {
                let value = if prep {
                    let InstructionData::Store { args, .. } =
                        function.dfg.insts[self.preps[0].stores[1]]
                    else {
                        unreachable!()
                    };
                    args[0]
                } else {
                    self.loops[0].arms[1].bits
                };
                let cast = function.dfg.value_def(value).unwrap_inst();
                let InstructionData::LoadNoOffset { arg, .. } = function.dfg.insts[cast] else {
                    unreachable!()
                };
                let inst = function.dfg.value_def(arg).unwrap_inst();
                let InstructionData::Binary { opcode, .. } = &mut function.dfg.insts[inst] else {
                    unreachable!()
                };
                *opcode = if prep { Opcode::Fadd } else { Opcode::Fsub };
            }
            Guard | Overflow => {
                let inst = if prep {
                    self.preps[0].nonzero
                } else {
                    function
                        .dfg
                        .value_def(self.loops[0].arms[0].condition)
                        .unwrap_inst()
                };
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(inst);
                let yes = cursor.ins().iconst(types::I8, 1);
                match &mut cursor.func.dfg.insts[inst] {
                    InstructionData::Brif { arg, .. } => *arg = yes,
                    InstructionData::Binary { args, .. } => args[0] = yes,
                    _ => unreachable!(),
                }
            }
            Split => {
                let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[split] else {
                    unreachable!()
                };
                blocks.swap(0, 1);
            }
            Direction | Limit => {
                let record = self.loops[0];
                let block = function.layout.inst_block(record.arms[0].next).unwrap();
                let condition = if fault == Direction {
                    IntCC::SignedLessThan
                } else {
                    IntCC::SignedGreaterThanOrEqual
                };
                let inst = function.layout.block_insts(block).find(|inst| matches!(function.dfg.insts[*inst], InstructionData::IntCompare { cond, .. } if cond == condition)).unwrap();
                let InstructionData::IntCompare { cond, args, .. } = &mut function.dfg.insts[inst]
                else {
                    unreachable!()
                };
                if fault == Direction {
                    *cond = IntCC::SignedGreaterThanOrEqual;
                } else {
                    args[1] = record.inputs[2].1;
                }
            }
            Phi => {
                let arm = self.loops[0].arms[0];
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(arm.next);
                let yes = cursor.ins().iconst(types::I8, 1);
                let dfg = &mut cursor.func.dfg;
                let InstructionData::Jump { destination, .. } = &mut dfg.insts[arm.next] else {
                    unreachable!()
                };
                destination.clear(&mut dfg.value_lists);
                for value in [arm.tag, arm.bits, yes] {
                    destination.append_argument(value, &mut dfg.value_lists);
                }
            }
            Source => {
                for value in [source.0, source.1] {
                    let inst = function.dfg.value_def(value).unwrap_inst();
                    let InstructionData::Load { offset, .. } = &mut function.dfg.insts[inst] else {
                        unreachable!()
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
            Store | VisibleStore => {
                let payload = if fault == VisibleStore {
                    self.loops[0].visible_store
                } else {
                    store
                };
                let prior = function.layout.prev_inst(payload).unwrap();
                for inst in [prior, payload] {
                    let InstructionData::Store { offset, .. } = &mut function.dfg.insts[inst]
                    else {
                        unreachable!()
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
            Targets => {
                let InstructionData::Jump {
                    destination: left, ..
                } = function.dfg.insts[next]
                else {
                    unreachable!()
                };
                let other = if prep {
                    self.preps[0].next[1]
                } else {
                    self.loops[0].next[1]
                };
                let InstructionData::Jump {
                    destination: right, ..
                } = function.dfg.insts[other]
                else {
                    unreachable!()
                };
                if prep {
                    let target = function
                        .layout
                        .blocks()
                        .find(|block| {
                            *block != left.block(&function.dfg.value_lists)
                                && function.dfg.block_params(*block).len() == 1
                                && function
                                    .dfg
                                    .value_type(function.dfg.block_params(*block)[0])
                                    == types::I32
                        })
                        .unwrap();
                    let count = left.args(&function.dfg.value_lists).last().unwrap();
                    let call = cranelift_codegen::ir::BlockCall::new(
                        target,
                        [count],
                        &mut function.dfg.value_lists,
                    );
                    let InstructionData::Jump { destination, .. } = &mut function.dfg.insts[next]
                    else {
                        unreachable!()
                    };
                    *destination = call;
                } else {
                    let left_arg = left.args(&function.dfg.value_lists).last().unwrap();
                    let right_arg = right.args(&function.dfg.value_lists).last().unwrap();
                    let left_call = cranelift_codegen::ir::BlockCall::new(
                        right.block(&function.dfg.value_lists),
                        [left_arg],
                        &mut function.dfg.value_lists,
                    );
                    let right_call = cranelift_codegen::ir::BlockCall::new(
                        left.block(&function.dfg.value_lists),
                        [right_arg],
                        &mut function.dfg.value_lists,
                    );
                    let InstructionData::Jump { destination, .. } = &mut function.dfg.insts[next]
                    else {
                        unreachable!()
                    };
                    *destination = left_call;
                    let InstructionData::Jump { destination, .. } = &mut function.dfg.insts[other]
                    else {
                        unreachable!()
                    };
                    *destination = right_call;
                }
            }
            Count => {
                let InstructionData::Jump { destination, .. } = function.dfg.insts[next] else {
                    unreachable!()
                };
                let value = destination.args(&function.dfg.value_lists).last().unwrap();
                let BlockArg::Value(value) = value else {
                    unreachable!()
                };
                let inst = function.dfg.value_def(value).unwrap_inst();
                let InstructionData::Binary { opcode, .. } = &mut function.dfg.insts[inst] else {
                    unreachable!()
                };
                *opcode = Opcode::Isub;
            }
            _ => unreachable!(),
        }
    }

    pub fn verify_loops(
        &self,
        function: &Function,
        slots: Value,
        snapshot: &super::ir::Snapshot,
        blocks: &[Block],
        fallback: Block,
        guard: Block,
    ) -> Result<(), JitError> {
        use super::shape::{numeric_loop, LoopExpression as Part};
        if blocks.len() != snapshot.operations.len() {
            return Err(invalid());
        }
        let mut cfg = ControlFlowGraph::new();
        if !self.preps.is_empty() || !self.loops.is_empty() {
            cfg.compute(function);
        }
        let mut preps = self.preps.iter();
        let mut loops = self.loops.iter();
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            let (base, jump, prep) = match op {
                Operation::NumericForPrep { base, jump } => (base.0, jump, true),
                Operation::NumericForLoop { base, jump } => (base.0, jump, false),
                _ => continue,
            };
            let target = (pc + 1)
                .checked_add_signed(isize::from(jump))
                .ok_or_else(invalid)?;
            let count = *function
                .dfg
                .block_params(blocks[pc])
                .first()
                .ok_or_else(invalid)?;
            if prep {
                let record = preps.next().ok_or_else(invalid)?;
                if record.pc != pc {
                    return Err(invalid());
                }
                for (input, index) in record.inputs.into_iter().zip([base, base + 2]) {
                    if !source_operand(
                        function,
                        slots,
                        snapshot,
                        RCIndex::Register(crate::types::RegisterIndex(index)),
                        input.0,
                        input.1,
                    ) {
                        return Err(invalid());
                    }
                }
                let InstructionData::Brif {
                    arg,
                    blocks: destinations,
                    ..
                } = function.dfg.insts[record.nonzero]
                else {
                    return Err(invalid());
                };
                let accepted = destinations[0].block(&function.dfg.value_lists);
                let mut declined = destinations[1].args(&function.dfg.value_lists);
                let Some(BlockArg::Value(exit_pc)) = declined.next() else {
                    return Err(invalid());
                };
                let Some(BlockArg::Value(exit_count)) = declined.next() else {
                    return Err(invalid());
                };
                if !numeric_loop(function, arg, &record.inputs, Part::Nonzero)
                    || destinations[0]
                        .args(&function.dfg.value_lists)
                        .next()
                        .is_some()
                    || function.layout.inst_block(record.split) != Some(accepted)
                    || !single_predecessor(&cfg, accepted, record.nonzero)
                    || destinations[1].block(&function.dfg.value_lists) != guard
                    || literal(function, exit_pc) != Some(pc as u64)
                    || !same_value(function, exit_count, count)
                    || declined.next().is_some()
                {
                    return Err(invalid());
                }
                let arms = loop_split(
                    function,
                    &cfg,
                    record.split,
                    &record.inputs,
                    Part::PrepSplit,
                )?;
                for (index, part, tag) in [
                    (0, Part::PrepInteger, abi::INTEGER),
                    (1, Part::PrepFloat, abi::NUMBER),
                ] {
                    let (stored_tag, bits) =
                        payload_store(function, record.stores[index], slots, base)?;
                    if literal(function, stored_tag) != Some(tag)
                        || !numeric_loop(function, bits, &record.inputs, part)
                        || function.layout.inst_block(record.stores[index]) != Some(arms[index])
                        || function.layout.last_inst(arms[index]) != Some(record.next[index])
                    {
                        return Err(invalid());
                    }
                    loop_advance(
                        function,
                        record.next[index],
                        blocks,
                        target,
                        count,
                        fallback,
                    )?;
                    only_loop_stores(function, arms[index], &record.stores[index..=index])?;
                }
            } else {
                let record = loops.next().ok_or_else(invalid)?;
                if record.pc != pc {
                    return Err(invalid());
                }
                for (input, index) in record.inputs.into_iter().zip([base, base + 1, base + 2]) {
                    if !source_operand(
                        function,
                        slots,
                        snapshot,
                        RCIndex::Register(crate::types::RegisterIndex(index)),
                        input.0,
                        input.1,
                    ) {
                        return Err(invalid());
                    }
                }
                let arms = loop_split(
                    function,
                    &cfg,
                    record.split,
                    &record.inputs,
                    Part::StepSplit,
                )?;
                let join = function
                    .layout
                    .inst_block(record.branch)
                    .ok_or_else(invalid)?;
                let params = function.dfg.block_params(join);
                if params.len() != 3
                    || params
                        .iter()
                        .map(|value| function.dfg.value_type(*value))
                        .ne([types::I64, types::I64, types::I8])
                    || function.layout.last_inst(join) != Some(record.branch)
                {
                    return Err(invalid());
                }
                let mut seen = [false; 2];
                for predecessor in cfg.pred_iter(join) {
                    let index = record
                        .arms
                        .iter()
                        .position(|arm| arm.next == predecessor.inst)
                        .ok_or_else(invalid)?;
                    if seen[index] {
                        return Err(invalid());
                    }
                    seen[index] = true;
                }
                if seen != [true, true] {
                    return Err(invalid());
                }
                for (index, tag, bits_part, condition_part) in [
                    (0, abi::INTEGER, Part::StepInteger, Part::ConditionInteger),
                    (1, abi::NUMBER, Part::StepFloat, Part::ConditionFloat),
                ] {
                    let arm = record.arms[index];
                    let InstructionData::Jump { destination, .. } = function.dfg.insts[arm.next]
                    else {
                        return Err(invalid());
                    };
                    let expected = [arm.tag, arm.bits, arm.condition];
                    let mut actual = destination.args(&function.dfg.value_lists);
                    let arguments_match = expected.into_iter().all(|expected| {
                        matches!(actual.next(), Some(BlockArg::Value(value)) if same_value(function, value, expected))
                    });
                    if destination.block(&function.dfg.value_lists) != join
                        || function.layout.last_inst(arms[index]) != Some(arm.next)
                        || !arguments_match
                        || actual.next().is_some()
                        || literal(function, arm.tag) != Some(tag)
                        || !numeric_loop(function, arm.bits, &record.inputs, bits_part)
                        || !numeric_loop(function, arm.condition, &record.inputs, condition_part)
                    {
                        return Err(invalid());
                    }
                    only_loop_stores(function, arms[index], &[])?;
                }
                let (tag, bits) = payload_store(function, record.store, slots, base)?;
                let InstructionData::Brif {
                    arg,
                    blocks: destinations,
                    ..
                } = function.dfg.insts[record.branch]
                else {
                    return Err(invalid());
                };
                if function.layout.inst_block(record.store) != Some(join)
                    || !same_value(function, tag, params[0])
                    || !same_value(function, bits, params[1])
                    || !same_value(function, arg, params[2])
                {
                    return Err(invalid());
                }
                only_loop_stores(function, join, &[record.store])?;
                for (index, next_pc) in [(0, target), (1, pc + 1)] {
                    let block = destinations[index].block(&function.dfg.value_lists);
                    if destinations[index]
                        .args(&function.dfg.value_lists)
                        .next()
                        .is_some()
                        || !single_predecessor(&cfg, block, record.branch)
                        || function.layout.last_inst(block) != Some(record.next[index])
                    {
                        return Err(invalid());
                    }
                    loop_advance(
                        function,
                        record.next[index],
                        blocks,
                        next_pc,
                        count,
                        fallback,
                    )?;
                    if index == 0 {
                        let (tag, bits) =
                            payload_store(function, record.visible_store, slots, base + 3)?;
                        if function.layout.inst_block(record.visible_store) != Some(block)
                            || !same_value(function, tag, params[0])
                            || !same_value(function, bits, params[1])
                        {
                            return Err(invalid());
                        }
                        only_loop_stores(function, block, &[record.visible_store])?;
                    } else {
                        only_loop_stores(function, block, &[])?;
                    }
                }
            }
        }
        if preps.next().is_some() || loops.next().is_some() {
            return Err(invalid());
        }
        Ok(())
    }

    pub fn verify_comparisons(
        &self,
        function: &Function,
        slots: Value,
        snapshot: &super::ir::Snapshot,
        blocks: &[Block],
    ) -> Result<(), JitError> {
        if blocks.len() != snapshot.operations.len() {
            return Err(invalid());
        }
        let mut cfg = ControlFlowGraph::new();
        if !self.comparisons.is_empty() {
            cfg.compute(function);
        }
        let mut records = self.comparisons.iter();
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            let (left, right, skip_if) = match op {
                Operation::Eq {
                    left,
                    right,
                    skip_if,
                }
                | Operation::Less {
                    left,
                    right,
                    skip_if,
                }
                | Operation::LessEq {
                    left,
                    right,
                    skip_if,
                } => (left, right, skip_if),
                _ => continue,
            };
            let record = records.next().ok_or_else(invalid)?;
            if record.pc != pc {
                return Err(invalid());
            }
            for ((tag, bits), operand) in record.inputs.into_iter().zip([left, right]) {
                if !source_operand(function, slots, snapshot, operand, tag, bits) {
                    return Err(invalid());
                }
            }
            let InstructionData::Brif {
                arg, blocks: arms, ..
            } = function.dfg.insts[record.split]
            else {
                return Err(invalid());
            };
            let ValueDef::Result(test, 0) =
                function.dfg.value_def(function.dfg.resolve_aliases(arg))
            else {
                return Err(invalid());
            };
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond: IntCC::Equal,
                args,
                ..
            } = function.dfg.insts[test]
            else {
                return Err(invalid());
            };
            if args.map(|value| function.dfg.resolve_aliases(value))
                != record
                    .inputs
                    .map(|(tag, _)| function.dfg.resolve_aliases(tag))
            {
                return Err(invalid());
            }
            for (arm, value, mixed) in
                [(arms[0], record.same, false), (arms[1], record.mixed, true)]
            {
                let target = arm.block(&function.dfg.value_lists);
                let ValueDef::Result(inst, 0) =
                    function.dfg.value_def(function.dfg.resolve_aliases(value))
                else {
                    return Err(invalid());
                };
                let mut predecessors = cfg.pred_iter(target);
                if arm.args(&function.dfg.value_lists).next().is_some()
                    || function.layout.inst_block(inst) != Some(target)
                    || predecessors
                        .next()
                        .is_none_or(|pred| pred.inst != record.split)
                    || predecessors.next().is_some()
                    || !super::shape::comparison(function, value, record.inputs, op, mixed)
                {
                    return Err(invalid());
                }
                let first = self
                    .inputs
                    .partition_point(|input| input.inst.as_u32() < inst.as_u32());
                for (tag, bits) in record.inputs {
                    if !self.inputs[first..].iter().take(2).any(|input| {
                        input.inst == inst && !input.float && input.tag == tag && input.bits == bits
                    }) {
                        return Err(invalid());
                    }
                }
            }
            let InstructionData::Brif {
                arg,
                blocks: destinations,
                ..
            } = function.dfg.insts[record.branch]
            else {
                return Err(invalid());
            };
            let join = function
                .layout
                .inst_block(record.branch)
                .ok_or_else(invalid)?;
            let params = function.dfg.block_params(join);
            if params.len() != 1
                || function.dfg.value_type(params[0]) != types::I8
                || !super::shape::polarity(function, arg, params[0], !skip_if)
            {
                return Err(invalid());
            }
            let mut seen = [false; 2];
            for predecessor in cfg.pred_iter(join) {
                let InstructionData::Jump { destination, .. } =
                    function.dfg.insts[predecessor.inst]
                else {
                    return Err(invalid());
                };
                let mut args = destination.args(&function.dfg.value_lists);
                let Some(BlockArg::Value(value)) = args.next() else {
                    return Err(invalid());
                };
                let index = [record.same, record.mixed]
                    .iter()
                    .position(|expected| {
                        function.dfg.resolve_aliases(*expected)
                            == function.dfg.resolve_aliases(value)
                    })
                    .ok_or_else(invalid)?;
                if seen[index]
                    || args.next().is_some()
                    || function.layout.inst_block(predecessor.inst)
                        != Some(arms[index].block(&function.dfg.value_lists))
                {
                    return Err(invalid());
                }
                seen[index] = true;
            }
            if seen != [true, true] {
                return Err(invalid());
            }
            let count = *function
                .dfg
                .block_params(blocks[pc])
                .first()
                .ok_or_else(invalid)?;
            for (destination, target) in destinations.into_iter().zip([pc + 2, pc + 1]) {
                if blocks.get(target).copied() != Some(destination.block(&function.dfg.value_lists))
                {
                    return Err(invalid());
                }
                let mut args = destination.args(&function.dfg.value_lists);
                let Some(BlockArg::Value(value)) = args.next() else {
                    return Err(invalid());
                };
                if args.next().is_some() || !super::shape::increment(function, value, count) {
                    return Err(invalid());
                }
            }
        }
        if records.next().is_some() {
            return Err(invalid());
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn corrupt_comparison(
        &mut self,
        function: &mut Function,
        fault: ComparisonCorruption,
        registers: usize,
    ) {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::immediates::Ieee64,
            ir::InstBuilder,
        };
        let record = *self.comparisons.first().expect("missing comparison");
        match fault {
            ComparisonCorruption::Missing => self.comparisons.clear(),
            ComparisonCorruption::ProgramCounter => self.comparisons[0].pc += 1,
            ComparisonCorruption::Growth => self.comparison(record),
            ComparisonCorruption::SameCondition => {
                let select = function.dfg.value_def(record.same).unwrap_inst();
                let InstructionData::Ternary { args, .. } = function.dfg.insts[select] else {
                    unreachable!()
                };
                let test = function.dfg.value_def(args[1]).unwrap_inst();
                let InstructionData::IntCompare { cond, .. } = &mut function.dfg.insts[test] else {
                    unreachable!()
                };
                *cond = if *cond == IntCC::Equal {
                    IntCC::SignedLessThan
                } else {
                    IntCC::Equal
                };
            }
            ComparisonCorruption::MixedGuard => {
                let inst = function.dfg.value_def(record.mixed).unwrap_inst();
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(inst);
                let yes = cursor.ins().iconst(types::I8, 1);
                let InstructionData::Binary { args, .. } = &mut cursor.func.dfg.insts[inst] else {
                    unreachable!()
                };
                args[1] = yes;
            }
            ComparisonCorruption::MixedBound => {
                let inst = function.dfg.value_def(record.mixed).unwrap_inst();
                let block = function.layout.inst_block(inst).unwrap();
                let constant = function.layout.block_insts(block).find(|inst| matches!(function.dfg.insts[*inst], InstructionData::UnaryIeee64 { opcode: Opcode::F64const, imm } if imm.bits() == 9_223_372_036_854_775_808.0f64.to_bits())).expect("missing comparison boundary");
                let InstructionData::UnaryIeee64 { imm, .. } = &mut function.dfg.insts[constant]
                else {
                    unreachable!()
                };
                *imm = Ieee64::with_float(4_611_686_018_427_387_904.0);
            }
            ComparisonCorruption::Split => {
                let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[record.split]
                else {
                    unreachable!()
                };
                blocks.swap(0, 1);
            }
            ComparisonCorruption::Phi => {
                let value = record.same;
                let block = function
                    .layout
                    .inst_block(function.dfg.value_def(value).unwrap_inst())
                    .unwrap();
                let jump = function.layout.last_inst(block).unwrap();
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(jump);
                let yes = cursor.ins().iconst(types::I8, 1);
                let dfg = &mut cursor.func.dfg;
                let InstructionData::Jump { destination, .. } = &mut dfg.insts[jump] else {
                    unreachable!()
                };
                destination.clear(&mut dfg.value_lists);
                destination.append_argument(yes, &mut dfg.value_lists);
            }
            ComparisonCorruption::Targets => {
                let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[record.branch]
                else {
                    unreachable!()
                };
                blocks.swap(0, 1);
            }
            ComparisonCorruption::Polarity => {
                let InstructionData::Brif { arg, .. } = function.dfg.insts[record.branch] else {
                    unreachable!()
                };
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(record.branch);
                let opposite = cursor.ins().bxor_imm_u(arg, 1);
                let InstructionData::Brif { arg, .. } = &mut cursor.func.dfg.insts[record.branch]
                else {
                    unreachable!()
                };
                *arg = opposite;
            }
            ComparisonCorruption::Count => {
                let InstructionData::Brif { blocks, .. } = function.dfg.insts[record.branch] else {
                    unreachable!()
                };
                let Some(BlockArg::Value(value)) = blocks[0].args(&function.dfg.value_lists).next()
                else {
                    unreachable!()
                };
                let inst = function.dfg.value_def(value).unwrap_inst();
                let InstructionData::Binary { opcode, .. } = &mut function.dfg.insts[inst] else {
                    unreachable!()
                };
                *opcode = Opcode::Isub;
            }
            ComparisonCorruption::Source => {
                for value in [record.inputs[0].0, record.inputs[0].1] {
                    let inst = function.dfg.value_def(value).unwrap_inst();
                    let InstructionData::Load { offset, .. } = &mut function.dfg.insts[inst] else {
                        unreachable!()
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
        }
    }

    pub fn verify_truths(
        &self,
        function: &Function,
        slots: Value,
        snapshot: &super::ir::Snapshot,
        blocks: &[Block],
    ) -> Result<(), JitError> {
        if blocks.len() != snapshot.operations.len() {
            return Err(invalid());
        }
        let mut records = self.truths.iter();
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            let source = match op {
                Operation::Not { source, .. } => source,
                Operation::Test { value, .. } => value,
                _ => continue,
            };
            let record = records.next().ok_or_else(invalid)?;
            if record.pc != pc
                || !source_operand(
                    function,
                    slots,
                    snapshot,
                    RCIndex::Register(source),
                    record.tag,
                    record.bits,
                )
            {
                return Err(invalid());
            }
            match op {
                Operation::Not { dest, .. } => {
                    let InstructionData::Store {
                        args,
                        offset,
                        flags,
                        ..
                    } = function.dfg.insts[record.point]
                    else {
                        return Err(invalid());
                    };
                    if function.dfg.resolve_aliases(args[1]) != function.dfg.resolve_aliases(slots)
                        || i32::from(offset) != i32::from(dest.0) * 16 + 8
                        || function.dfg.mem_flags[flags] != MemFlagsData::new()
                        || !super::shape::truth(
                            function,
                            args[0],
                            record.tag,
                            record.bits,
                            true,
                            true,
                        )
                    {
                        return Err(invalid());
                    }
                    let prior = function
                        .layout
                        .prev_inst(record.point)
                        .ok_or_else(invalid)?;
                    let InstructionData::Store {
                        args,
                        offset,
                        flags,
                        ..
                    } = function.dfg.insts[prior]
                    else {
                        return Err(invalid());
                    };
                    if function.dfg.resolve_aliases(args[1]) != function.dfg.resolve_aliases(slots)
                        || i32::from(offset) != i32::from(dest.0) * 16
                        || function.dfg.mem_flags[flags] != MemFlagsData::new()
                        || literal(function, args[0]) != Some(abi::BOOLEAN)
                    {
                        return Err(invalid());
                    }
                }
                Operation::Test { is_true, .. } => {
                    let InstructionData::Brif {
                        arg,
                        blocks: destinations,
                        ..
                    } = function.dfg.insts[record.point]
                    else {
                        return Err(invalid());
                    };
                    if !super::shape::truth(function, arg, record.tag, record.bits, !is_true, false)
                    {
                        return Err(invalid());
                    }
                    let count = *function
                        .dfg
                        .block_params(blocks[pc])
                        .first()
                        .ok_or_else(invalid)?;
                    for (destination, target) in destinations.into_iter().zip([pc + 2, pc + 1]) {
                        if blocks.get(target).copied()
                            != Some(destination.block(&function.dfg.value_lists))
                        {
                            return Err(invalid());
                        }
                        let mut args = destination.args(&function.dfg.value_lists);
                        let Some(BlockArg::Value(value)) = args.next() else {
                            return Err(invalid());
                        };
                        if args.next().is_some() || !super::shape::increment(function, value, count)
                        {
                            return Err(invalid());
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        if records.next().is_some() {
            return Err(invalid());
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn corrupt_truth(&self, function: &mut Function, fault: TruthCorruption, registers: usize) {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::InstBuilder,
        };
        let record = self.truths.first().expect("missing truth operation");
        match fault {
            TruthCorruption::Payload => {
                let InstructionData::Store { args, .. } = &mut function.dfg.insts[record.point]
                else {
                    panic!("missing logical-not payload")
                };
                args[0] = record.bits;
            }
            TruthCorruption::Source => {
                for value in [record.tag, record.bits] {
                    let inst = function.dfg.value_def(value).unwrap_inst();
                    let InstructionData::Load { offset, .. } = &mut function.dfg.insts[inst] else {
                        panic!("missing source register")
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
            TruthCorruption::Condition => {
                let InstructionData::Brif { arg, .. } = function.dfg.insts[record.point] else {
                    panic!("missing truth branch")
                };
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(record.point);
                let wrong = cursor.ins().bxor_imm_u(arg, 1);
                let InstructionData::Brif { arg, .. } = &mut cursor.func.dfg.insts[record.point]
                else {
                    unreachable!()
                };
                *arg = wrong;
            }
            TruthCorruption::Targets => {
                let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[record.point]
                else {
                    panic!("missing truth branch")
                };
                blocks.swap(0, 1);
            }
            TruthCorruption::Count => {
                let InstructionData::Brif { blocks, .. } = function.dfg.insts[record.point] else {
                    panic!("missing truth branch")
                };
                let Some(BlockArg::Value(value)) = blocks[0].args(&function.dfg.value_lists).next()
                else {
                    panic!("missing fuel count")
                };
                let inst = function.dfg.value_def(value).unwrap_inst();
                let InstructionData::Binary { opcode, .. } = &mut function.dfg.insts[inst] else {
                    panic!("missing fuel increment")
                };
                *opcode = Opcode::Isub;
            }
        }
    }

    pub fn verify_arithmetic(
        &self,
        function: &Function,
        slots: Value,
        snapshot: &super::ir::Snapshot,
    ) -> Result<(), JitError> {
        if self
            .inputs
            .windows(2)
            .any(|pair| pair[0].inst.as_u32() > pair[1].inst.as_u32())
            || self
                .inputs
                .windows(3)
                .any(|group| group[0].inst == group[2].inst)
        {
            return Err(invalid());
        }
        let mut records = self.arithmetic.iter();
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            let (dest, left, right, integer, float) = match op {
                Operation::Add { dest, left, right } => {
                    (dest, left, right, Some(Opcode::Iadd), Opcode::Fadd)
                }
                Operation::Sub { dest, left, right } => {
                    (dest, left, right, Some(Opcode::Isub), Opcode::Fsub)
                }
                Operation::Mul { dest, left, right } => {
                    (dest, left, right, Some(Opcode::Imul), Opcode::Fmul)
                }
                Operation::Div { dest, left, right } => (dest, left, right, None, Opcode::Fdiv),
                _ => continue,
            };
            for opcode in integer.into_iter().chain(std::iter::once(float)) {
                let record = records.next().ok_or_else(invalid)?;
                let floating = opcode == float;
                if record.pc != pc || record.floating != floating {
                    return Err(invalid());
                }
                let result = function.dfg.resolve_aliases(record.result);
                let ValueDef::Result(inst, 0) = function.dfg.value_def(result) else {
                    return Err(invalid());
                };
                let InstructionData::Binary {
                    opcode: actual,
                    args,
                } = function.dfg.insts[inst]
                else {
                    return Err(invalid());
                };
                if actual != opcode
                    || function.dfg.value_type(result)
                        != if floating { types::F64 } else { types::I64 }
                {
                    return Err(invalid());
                }
                for ((tag, bits), (operand, value)) in record
                    .inputs
                    .into_iter()
                    .zip([(left, args[0]), (right, args[1])])
                {
                    if !source_operand(function, slots, snapshot, operand, tag, bits) {
                        return Err(invalid());
                    }
                    let value = function.dfg.resolve_aliases(value);
                    let consumer = if floating {
                        let ValueDef::Result(inst, 0) = function.dfg.value_def(value) else {
                            return Err(invalid());
                        };
                        inst
                    } else {
                        if value != function.dfg.resolve_aliases(bits) {
                            return Err(invalid());
                        }
                        inst
                    };
                    let start = self
                        .inputs
                        .partition_point(|input| input.inst.as_u32() < consumer.as_u32());
                    if !self.inputs[start..]
                        .iter()
                        .take_while(|input| input.inst == consumer)
                        .any(|input| {
                            input.float == floating
                                && function.dfg.resolve_aliases(input.tag)
                                    == function.dfg.resolve_aliases(tag)
                                && function.dfg.resolve_aliases(input.bits)
                                    == function.dfg.resolve_aliases(bits)
                        })
                    {
                        return Err(invalid());
                    }
                }
                let InstructionData::Store {
                    args,
                    offset,
                    flags,
                    ..
                } = function.dfg.insts[record.store]
                else {
                    return Err(invalid());
                };
                let stored = function.dfg.resolve_aliases(args[0]);
                if function.dfg.resolve_aliases(args[1]) != function.dfg.resolve_aliases(slots)
                    || function.dfg.mem_flags[flags] != MemFlagsData::new()
                    || i32::from(offset) != i32::from(dest.0) * 16 + 8
                    || function.dfg.value_type(stored) != types::I64
                    || function.layout.inst_block(record.store) != function.layout.inst_block(inst)
                    || function.layout.inst_block(inst).is_none()
                {
                    return Err(invalid());
                }
                if floating {
                    let ValueDef::Result(cast, 0) = function.dfg.value_def(stored) else {
                        return Err(invalid());
                    };
                    let InstructionData::LoadNoOffset {
                        opcode: Opcode::Bitcast,
                        arg,
                        flags,
                    } = function.dfg.insts[cast]
                    else {
                        return Err(invalid());
                    };
                    if function.dfg.resolve_aliases(arg) != result
                        || function.dfg.mem_flags[flags] != MemFlagsData::new()
                    {
                        return Err(invalid());
                    }
                } else if stored != result {
                    return Err(invalid());
                }
                let prior = function
                    .layout
                    .prev_inst(record.store)
                    .ok_or_else(invalid)?;
                let InstructionData::Store {
                    args,
                    offset,
                    flags,
                    ..
                } = function.dfg.insts[prior]
                else {
                    return Err(invalid());
                };
                if function.dfg.resolve_aliases(args[1]) != function.dfg.resolve_aliases(slots)
                    || function.dfg.mem_flags[flags] != MemFlagsData::new()
                    || i32::from(offset) != i32::from(dest.0) * 16
                    || literal(function, args[0])
                        != Some(if floating { abi::NUMBER } else { abi::INTEGER })
                {
                    return Err(invalid());
                }
            }
        }
        if records.next().is_some() {
            return Err(invalid());
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn corrupt_arithmetic_first(
        &self,
        function: &mut Function,
        fault: ArithmeticCorruption,
        registers: usize,
    ) {
        let record = self.arithmetic.first().expect("missing arithmetic");
        match fault {
            ArithmeticCorruption::Opcode | ArithmeticCorruption::Operands => {
                let inst = function.dfg.value_def(record.result).unwrap_inst();
                let InstructionData::Binary { opcode, args } = &mut function.dfg.insts[inst] else {
                    panic!("missing arithmetic operands")
                };
                if matches!(fault, ArithmeticCorruption::Operands) {
                    args.swap(0, 1);
                } else {
                    *opcode = match *opcode {
                        Opcode::Iadd => Opcode::Isub,
                        Opcode::Isub | Opcode::Imul => Opcode::Iadd,
                        Opcode::Fadd => Opcode::Fsub,
                        _ => Opcode::Fadd,
                    };
                }
            }
            ArithmeticCorruption::Source => {
                for value in [record.inputs[0].0, record.inputs[0].1] {
                    let inst = function.dfg.value_def(value).unwrap_inst();
                    let InstructionData::Load { offset, .. } = &mut function.dfg.insts[inst] else {
                        panic!("missing register operand")
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
            ArithmeticCorruption::Destination => {
                let prior = function.layout.prev_inst(record.store).unwrap();
                for inst in [prior, record.store] {
                    let InstructionData::Store { offset, .. } = &mut function.dfg.insts[inst]
                    else {
                        panic!("missing destination store")
                    };
                    *offset = ((i32::from(*offset) + 16) % (registers as i32 * 16)).into();
                }
            }
        }
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

fn literal(function: &Function, value: Value) -> Option<u64> {
    let value = function.dfg.resolve_aliases(value);
    if function.dfg.value_type(value) != types::I64 {
        return None;
    }
    let ValueDef::Result(inst, 0) = function.dfg.value_def(value) else {
        return None;
    };
    match function.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits() as u64),
        _ => None,
    }
}

fn source_operand(
    function: &Function,
    slots: Value,
    snapshot: &super::ir::Snapshot,
    operand: RCIndex,
    tag: Value,
    bits: Value,
) -> bool {
    let tag = function.dfg.resolve_aliases(tag);
    let bits = function.dfg.resolve_aliases(bits);
    if function.dfg.value_type(tag) != types::I64 || function.dfg.value_type(bits) != types::I64 {
        return false;
    }
    match operand {
        RCIndex::Constant(index) => {
            snapshot
                .constants
                .get(usize::from(index.0))
                .is_some_and(|slot| {
                    literal(function, tag) == Some(slot.tag)
                        && literal(function, bits) == Some(slot.bits)
                })
        }
        RCIndex::Register(index) => {
            if usize::from(index.0) >= snapshot.registers {
                return false;
            }
            let (ValueDef::Result(tag_inst, 0), ValueDef::Result(bits_inst, 0)) =
                (function.dfg.value_def(tag), function.dfg.value_def(bits))
            else {
                return false;
            };
            let slots = function.dfg.resolve_aliases(slots);
            let offset = i32::from(index.0) * 16;
            matches!(function.dfg.insts[tag_inst], InstructionData::Load { opcode: Opcode::Load, arg, offset: actual, flags } if function.dfg.resolve_aliases(arg) == slots && i32::from(actual) == offset && function.dfg.mem_flags[flags] == MemFlagsData::new())
                && matches!(function.dfg.insts[bits_inst], InstructionData::Load { opcode: Opcode::Load, arg, offset: actual, flags } if function.dfg.resolve_aliases(arg) == slots && i32::from(actual) == offset + 8 && function.dfg.mem_flags[flags] == MemFlagsData::new())
                && function.layout.next_inst(tag_inst) == Some(bits_inst)
        }
    }
}

fn same_value(function: &Function, left: Value, right: Value) -> bool {
    function.dfg.resolve_aliases(left) == function.dfg.resolve_aliases(right)
}

fn single_predecessor(cfg: &ControlFlowGraph, block: Block, inst: Inst) -> bool {
    let mut predecessors = cfg.pred_iter(block);
    predecessors
        .next()
        .is_some_and(|predecessor| predecessor.inst == inst)
        && predecessors.next().is_none()
}

fn loop_split(
    function: &Function,
    cfg: &ControlFlowGraph,
    inst: Inst,
    inputs: &[(Value, Value)],
    part: super::shape::LoopExpression,
) -> Result<[Block; 2], JitError> {
    let InstructionData::Brif { arg, blocks, .. } = function.dfg.insts[inst] else {
        return Err(invalid());
    };
    if !super::shape::numeric_loop(function, arg, inputs, part) {
        return Err(invalid());
    }
    let targets = blocks.map(|arm| arm.block(&function.dfg.value_lists));
    for (arm, target) in blocks.into_iter().zip(targets) {
        if arm.args(&function.dfg.value_lists).next().is_some()
            || !single_predecessor(cfg, target, inst)
        {
            return Err(invalid());
        }
    }
    Ok(targets)
}

fn payload_store(
    function: &Function,
    inst: Inst,
    slots: Value,
    register: u8,
) -> Result<(Value, Value), JitError> {
    let prior = function.layout.prev_inst(inst).ok_or_else(invalid)?;
    let mut values = [slots; 2];
    for (index, point, offset) in [
        (0, prior, i32::from(register) * 16),
        (1, inst, i32::from(register) * 16 + 8),
    ] {
        let InstructionData::Store {
            opcode: Opcode::Store,
            args,
            offset: actual,
            flags,
        } = function.dfg.insts[point]
        else {
            return Err(invalid());
        };
        if !same_value(function, args[1], slots)
            || i32::from(actual) != offset
            || function.dfg.value_type(args[0]) != types::I64
            || function.dfg.mem_flags[flags] != MemFlagsData::new()
        {
            return Err(invalid());
        }
        values[index] = args[0];
    }
    Ok((values[0], values[1]))
}

fn loop_advance(
    function: &Function,
    inst: Inst,
    blocks: &[Block],
    target: usize,
    count: Value,
    fallback: Block,
) -> Result<(), JitError> {
    let InstructionData::Jump { destination, .. } = function.dfg.insts[inst] else {
        return Err(invalid());
    };
    let mut args = destination.args(&function.dfg.value_lists);
    if let Some(block) = blocks.get(target) {
        if destination.block(&function.dfg.value_lists) != *block {
            return Err(invalid());
        }
    } else {
        let Some(BlockArg::Value(pc)) = args.next() else {
            return Err(invalid());
        };
        if target != blocks.len()
            || destination.block(&function.dfg.value_lists) != fallback
            || literal(function, pc) != Some(target as u64)
        {
            return Err(invalid());
        }
    }
    let Some(BlockArg::Value(value)) = args.next() else {
        return Err(invalid());
    };
    if args.next().is_some() || !super::shape::increment(function, value, count) {
        return Err(invalid());
    }
    Ok(())
}

fn only_loop_stores(function: &Function, block: Block, allowed: &[Inst]) -> Result<(), JitError> {
    for inst in function.layout.block_insts(block) {
        if function.dfg.insts[inst].opcode().can_store()
            && !allowed.iter().any(|payload| {
                *payload == inst || function.layout.prev_inst(*payload) == Some(inst)
            })
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn scalar_transfer_writes(op: Operation, snapshot: &super::ir::Snapshot) -> Option<usize> {
    match op {
        Operation::Move { .. } | Operation::LoadBool { .. } => Some(1),
        Operation::LoadConstant { constant, .. } => snapshot
            .constants
            .get(usize::from(constant.0))
            .filter(|slot| slot.tag != abi::REFERENCE)
            .map(|_| 1),
        Operation::LoadNil { count, .. } => Some(usize::from(count)),
        Operation::Jump { close_upvalues, .. } if close_upvalues.is_none() => Some(0),
        _ => None,
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
                        flags,
                    },
                ) if self.function.dfg.mem_flags[flags] == MemFlagsData::new() => arg,
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
        fixture_in(None, action)
    }

    fn fixture_in(
        snapshot: Option<&super::super::ir::Snapshot>,
        action: impl FnOnce(&mut FunctionBuilder<'_>, Value, Value, &mut Stores),
    ) -> Result<(), JitError> {
        let allocator = BudgetAllocator(Ledger::new(1024 * 1024));
        let mut records = Vec::new_in(allocator);
        records.try_reserve_exact(8).unwrap();
        let mut stores = Stores {
            helper_calls: super::super::helper_flow::Calls::empty(records.allocator().clone()),
            transfer_writes: Vec::new_in(records.allocator().clone()),
            transfer_edges: Vec::new_in(records.allocator().clone()),
            expected_transfers: None,
            preps: Vec::new_in(records.allocator().clone()),
            loops: Vec::new_in(records.allocator().clone()),
            expected_preps: None,
            expected_loops: None,
            comparisons: {
                let mut comparisons = Vec::new_in(records.allocator().clone());
                comparisons.try_reserve_exact(8).unwrap();
                comparisons
            },
            expected_comparisons: None,
            truths: {
                let mut truths = Vec::new_in(records.allocator().clone());
                truths.try_reserve_exact(8).unwrap();
                truths
            },
            expected_truths: None,
            arithmetic: {
                let mut arithmetic = Vec::new_in(records.allocator().clone());
                arithmetic.try_reserve_exact(8).unwrap();
                arithmetic
            },
            expected_arithmetic: None,
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
        let result = stores.verify(&function, slots, None, 4).and_then(|_| {
            if let Some(snapshot) = snapshot {
                stores.verify_arithmetic(&function, slots, snapshot)
            } else {
                Ok(())
            }
        });
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

    fn arithmetic_snapshot(kind: usize, constants: bool) -> super::super::ir::Snapshot {
        use crate::types::{ConstantIndex8, RegisterIndex, VarCount};
        let left = if constants {
            RCIndex::Constant(ConstantIndex8(0))
        } else {
            RCIndex::Register(RegisterIndex(0))
        };
        let right = if constants {
            RCIndex::Constant(ConstantIndex8(1))
        } else {
            RCIndex::Register(RegisterIndex(1))
        };
        let dest = RegisterIndex(2);
        let op = match kind {
            0 => Operation::Add { dest, left, right },
            1 => Operation::Sub { dest, left, right },
            2 => Operation::Mul { dest, left, right },
            _ => Operation::Div { dest, left, right },
        };
        super::super::ir::Snapshot {
            operations: super::super::resources::owned(&[
                op,
                Operation::Return {
                    start: dest,
                    count: VarCount::constant(1),
                },
            ]),
            constants: super::super::resources::owned(&[
                abi::Slot {
                    tag: abi::INTEGER,
                    bits: (-41i64) as u64,
                },
                abi::Slot {
                    tag: abi::NUMBER,
                    bits: 2.0f64.to_bits(),
                },
            ]),
            registers: 4,
            upvalues: 0,
            prototypes: 0,
        }
    }

    fn arithmetic_fixture(
        kind: usize,
        constants: bool,
        change: impl FnOnce(&mut FunctionBuilder<'_>, &mut Stores),
    ) -> Result<(), JitError> {
        let snapshot = arithmetic_snapshot(kind, constants);
        snapshot.verify().unwrap();
        fixture_in(Some(&snapshot), |builder, slots, _, stores| {
            let mut pairs = [(slots, slots); 2];
            for (index, pair) in pairs.iter_mut().enumerate() {
                *pair = if constants {
                    let slot = snapshot.constants[index];
                    (
                        builder.ins().iconst(types::I64, slot.tag as i64),
                        builder.ins().iconst(types::I64, slot.bits as i64),
                    )
                } else {
                    (
                        builder.ins().load(
                            types::I64,
                            MemFlagsData::new(),
                            slots,
                            index as i32 * 16,
                        ),
                        builder.ins().load(
                            types::I64,
                            MemFlagsData::new(),
                            slots,
                            index as i32 * 16 + 8,
                        ),
                    )
                };
                let integer = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, pair.0, abi::INTEGER as i64);
                let number = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, pair.0, abi::NUMBER as i64);
                let numeric = builder.ins().bor(integer, number);
                let accepted = builder.create_block();
                let declined = builder.create_block();
                builder.ins().brif(numeric, accepted, &[], declined, &[]);
                builder.switch_to_block(declined);
                builder.ins().return_(&[]);
                builder.switch_to_block(accepted);
            }
            let float = builder.create_block();
            if kind == 3 {
                builder.ins().jump(float, &[]);
            } else {
                let li = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, pairs[0].0, abi::INTEGER as i64);
                let ri = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, pairs[1].0, abi::INTEGER as i64);
                let both = builder.ins().band(li, ri);
                let integer = builder.create_block();
                builder.ins().brif(both, integer, &[], float, &[]);
                builder.switch_to_block(integer);
                let result = match kind {
                    0 => builder.ins().iadd(pairs[0].1, pairs[1].1),
                    1 => builder.ins().isub(pairs[0].1, pairs[1].1),
                    _ => builder.ins().imul(pairs[0].1, pairs[1].1),
                };
                let inst = builder.func.dfg.value_def(result).unwrap_inst();
                for (tag, bits) in pairs {
                    stores.numeric_input(inst, tag, bits);
                }
                let tag = builder.ins().iconst(types::I64, abi::INTEGER as i64);
                let store = builder.ins().store(MemFlagsData::new(), tag, slots, 32);
                stores.record(store, NUMERIC);
                let store = builder.ins().store(MemFlagsData::new(), result, slots, 40);
                stores.arithmetic(0, result, pairs, store, false);
                builder.ins().return_(&[]);
            }
            builder.switch_to_block(float);
            let mut values = [slots; 2];
            for ((tag, bits), value) in pairs.into_iter().zip(&mut values) {
                let integer = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
                let converted = builder.ins().fcvt_from_sint(types::F64, bits);
                let number = builder.ins().bitcast(types::F64, MemFlagsData::new(), bits);
                *value = builder.ins().select(integer, converted, number);
                stores.float_input(builder.func.dfg.value_def(*value).unwrap_inst(), tag, bits);
            }
            let result = match kind {
                0 => builder.ins().fadd(values[0], values[1]),
                1 => builder.ins().fsub(values[0], values[1]),
                2 => builder.ins().fmul(values[0], values[1]),
                _ => builder.ins().fdiv(values[0], values[1]),
            };
            let bits = builder
                .ins()
                .bitcast(types::I64, MemFlagsData::new(), result);
            let tag = builder.ins().iconst(types::I64, abi::NUMBER as i64);
            let store = builder.ins().store(MemFlagsData::new(), tag, slots, 32);
            stores.record(store, NUMERIC);
            let store = builder.ins().store(MemFlagsData::new(), bits, slots, 40);
            stores.arithmetic(0, result, pairs, store, true);
            builder.ins().return_(&[]);
            change(builder, stores);
        })
    }

    #[test]
    fn arithmetic_records_match_all_four_source_operations_and_operand_kinds() {
        for kind in 0..4 {
            for constants in [false, true] {
                arithmetic_fixture(kind, constants, |_, _| {}).unwrap();
            }
        }
    }

    #[test]
    fn arithmetic_source_proof_rejects_wrong_opcodes_order_registers_and_destinations() {
        for fault in [
            ArithmeticCorruption::Opcode,
            ArithmeticCorruption::Operands,
            ArithmeticCorruption::Source,
            ArithmeticCorruption::Destination,
        ] {
            rejected(arithmetic_fixture(1, false, |builder, stores| {
                stores.corrupt_arithmetic_first(builder.func, fault, 4)
            }));
        }
        rejected(arithmetic_fixture(3, false, |builder, stores| {
            stores.corrupt_arithmetic_first(builder.func, ArithmeticCorruption::Opcode, 4)
        }));
    }

    #[test]
    fn arithmetic_constant_provenance_checks_exact_tag_and_payload_bits() {
        for tag in [false, true] {
            rejected(arithmetic_fixture(3, true, |builder, stores| {
                let record = stores.arithmetic[0];
                let value = if tag {
                    record.inputs[1].0
                } else {
                    record.inputs[1].1
                };
                let inst = builder.func.dfg.value_def(value).unwrap_inst();
                let InstructionData::UnaryImm { imm, .. } = &mut builder.func.dfg.insts[inst]
                else {
                    unreachable!()
                };
                *imm = if tag { abi::INTEGER as i64 } else { 123 }.into();
            }));
        }
    }

    #[test]
    fn arithmetic_result_proof_checks_payload_and_result_tag_stores() {
        for tag in [false, true] {
            rejected(arithmetic_fixture(1, false, |builder, stores| {
                let record = stores.arithmetic[0];
                if tag {
                    use cranelift_codegen::cursor::{Cursor, FuncCursor};
                    let prior = builder.func.layout.prev_inst(record.store).unwrap();
                    let mut cursor = FuncCursor::new(builder.func);
                    cursor.goto_inst(prior);
                    let wrong = cursor.ins().iconst(types::I64, abi::NUMBER as i64);
                    let InstructionData::Store { args, .. } = &mut cursor.func.dfg.insts[prior]
                    else {
                        unreachable!()
                    };
                    args[0] = wrong;
                } else {
                    let InstructionData::Store { args, .. } =
                        &mut builder.func.dfg.insts[record.store]
                    else {
                        unreachable!()
                    };
                    args[0] = record.inputs[0].1;
                }
            }));
        }
    }

    #[test]
    fn arithmetic_record_envelopes_reject_missing_extra_pc_arm_and_growth() {
        for mode in 0..6 {
            rejected(arithmetic_fixture(1, false, |_, stores| match mode {
                0 => stores.arithmetic.clear(),
                1 => {
                    let record = stores.arithmetic[0];
                    stores.arithmetic.push(record);
                }
                2 => stores.arithmetic[0].pc = 1,
                3 => stores.arithmetic[0].floating = true,
                4 => {
                    let input = stores.inputs[0];
                    stores.inputs.insert(0, input);
                }
                _ => {
                    let record = stores.arithmetic[0];
                    stores.arithmetic = Vec::new_in(stores.arithmetic.allocator().clone());
                    stores.arithmetic(
                        record.pc,
                        record.result,
                        record.inputs,
                        record.store,
                        record.floating,
                    );
                }
            }));
        }
    }

    #[test]
    fn arithmetic_memory_shapes_reject_noncanonical_load_store_and_cast_flags() {
        for mode in 0..5 {
            rejected(arithmetic_fixture(3, false, |builder, stores| {
                let record = stores.arithmetic[0];
                let flags = builder.func.dfg.mem_flags.insert_unchecked(
                    MemFlagsData::new().with_endianness(cranelift_codegen::ir::Endianness::Big),
                );
                let inst = match mode {
                    0 => builder.func.dfg.value_def(record.inputs[0].1).unwrap_inst(),
                    1 => record.store,
                    2 => builder.func.layout.prev_inst(record.store).unwrap(),
                    3 => {
                        let InstructionData::Store { args, .. } =
                            builder.func.dfg.insts[record.store]
                        else {
                            unreachable!()
                        };
                        builder.func.dfg.value_def(args[0]).unwrap_inst()
                    }
                    _ => {
                        let input = stores.inputs.iter().find(|input| input.float).unwrap();
                        let InstructionData::Ternary { args, .. } =
                            builder.func.dfg.insts[input.inst]
                        else {
                            unreachable!()
                        };
                        builder.func.dfg.value_def(args[2]).unwrap_inst()
                    }
                };
                match &mut builder.func.dfg.insts[inst] {
                    InstructionData::Load { flags: value, .. }
                    | InstructionData::Store { flags: value, .. }
                    | InstructionData::LoadNoOffset { flags: value, .. } => *value = flags,
                    _ => unreachable!(),
                }
            }));
        }
    }

    #[test]
    fn arithmetic_record_allocation_refusal_releases_partial_storage() {
        let snapshot = arithmetic_snapshot(0, false);
        let graph = FlowGraph::new(&snapshot).unwrap();
        let ledger = snapshot.operations.allocator().0.clone();
        let before = ledger.current();
        let stores = graph
            .nodes
            .iter()
            .filter(|node| node.lowering.native())
            .map(|node| 2 * node.access.writes.count())
            .sum::<usize>();
        ledger.set_limit(
            before + stores * std::mem::size_of::<Store>() + 4 * std::mem::size_of::<Input>(),
        );
        assert!(matches!(
            Stores::new(&graph, &snapshot),
            Err(JitError::ResourceLimit("scalar tag verification"))
        ));
        assert_eq!(ledger.current(), before);
        assert_eq!(ledger.refusals(), 1);
    }

    #[test]
    fn comparison_record_allocation_refusal_releases_partial_storage() {
        use crate::types::{RegisterIndex, VarCount};
        let snapshot = super::super::ir::Snapshot {
            operations: super::super::resources::owned(&[
                Operation::Less {
                    left: RCIndex::Register(RegisterIndex(0)),
                    right: RCIndex::Register(RegisterIndex(1)),
                    skip_if: true,
                },
                Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                },
                Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[]),
            registers: 2,
            upvalues: 0,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        let graph = FlowGraph::new(&snapshot).unwrap();
        let ledger = snapshot.operations.allocator().0.clone();
        let before = ledger.current();
        let allowance = 4 * std::mem::size_of::<Input>();
        ledger.set_limit(before + allowance);
        assert!(matches!(
            Stores::new(&graph, &snapshot),
            Err(JitError::ResourceLimit("scalar tag verification"))
        ));
        assert_eq!(ledger.current(), before);
        assert_eq!(ledger.peak(), before + allowance);
        assert_eq!(ledger.refusals(), 1);
    }

    #[test]
    fn loop_record_allocation_refusal_releases_partial_storage() {
        use crate::types::{RegisterIndex as R, VarCount};
        for prep in [false, true] {
            let snapshot = super::super::ir::Snapshot {
                operations: super::super::resources::owned(&[
                    if prep {
                        Operation::NumericForPrep {
                            base: R(0),
                            jump: 1,
                        }
                    } else {
                        Operation::NumericForLoop {
                            base: R(0),
                            jump: 1,
                        }
                    },
                    Operation::Return {
                        start: R(0),
                        count: VarCount::constant(0),
                    },
                    Operation::Return {
                        start: R(0),
                        count: VarCount::constant(0),
                    },
                ]),
                constants: super::super::resources::owned(&[]),
                registers: 4,
                upvalues: 0,
                prototypes: 0,
            };
            snapshot.verify().unwrap();
            let graph = FlowGraph::new(&snapshot).unwrap();
            let ledger = snapshot.operations.allocator().0.clone();
            let before = ledger.current();
            let stores = graph
                .nodes
                .iter()
                .filter(|node| node.lowering.native())
                .map(|node| 2 * node.access.writes.count())
                .sum::<usize>();
            let inputs = if prep { 4 } else { 6 };
            let allowance =
                stores * std::mem::size_of::<Store>() + inputs * std::mem::size_of::<Input>();
            ledger.set_limit(before + allowance);
            assert!(matches!(
                Stores::new(&graph, &snapshot),
                Err(JitError::ResourceLimit("scalar tag verification"))
            ));
            assert_eq!(ledger.current(), before);
            assert_eq!(ledger.peak(), before + allowance);
            assert_eq!(ledger.refusals(), 1);
        }
    }

    #[test]
    fn transfer_write_and_edge_allocation_refusal_releases_partial_storage() {
        use crate::types::{RegisterIndex as R, VarCount};
        for stage in 0..3 {
            let empty = stage == 2;
            let snapshot = super::super::ir::Snapshot {
                operations: super::super::resources::owned(&[
                    if empty {
                        Operation::LoadNil {
                            dest: R(0),
                            count: 0,
                        }
                    } else {
                        Operation::LoadBool {
                            dest: R(0),
                            value: true,
                            skip_next: false,
                        }
                    },
                    Operation::Return {
                        start: R(0),
                        count: VarCount::constant(0),
                    },
                ]),
                constants: super::super::resources::owned(&[]),
                registers: 1,
                upvalues: 0,
                prototypes: 0,
            };
            snapshot.verify().unwrap();
            let graph = FlowGraph::new(&snapshot).unwrap();
            let ledger = snapshot.operations.allocator().0.clone();
            let before = ledger.current();
            let allowance = if empty {
                0
            } else {
                2 * std::mem::size_of::<Store>()
                    + if stage == 1 {
                        std::mem::size_of::<TransferWrite>()
                    } else {
                        0
                    }
            };
            let peak = ledger.peak();
            ledger.set_limit(before + allowance);
            assert!(matches!(
                Stores::new(&graph, &snapshot),
                Err(JitError::ResourceLimit("scalar tag verification"))
            ));
            assert_eq!(ledger.current(), before);
            assert_eq!(ledger.peak(), peak.max(before + allowance));
            assert_eq!(ledger.refusals(), 1);
        }
    }

    #[test]
    fn helper_call_allocation_refusal_releases_partial_storage() {
        use crate::types::{RegisterIndex as R, VarCount};
        let snapshot = super::super::ir::Snapshot {
            operations: super::super::resources::owned(&[
                Operation::NewTable {
                    dest: R(0),
                    array_size: 1,
                    map_size: 0,
                },
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[]),
            registers: 1,
            upvalues: 0,
            prototypes: 0,
        };
        let graph = FlowGraph::new(&snapshot).unwrap();
        let ledger = snapshot.operations.allocator().0.clone();
        let before = ledger.current();
        let allowance = 2 * std::mem::size_of::<Store>();
        let peak = ledger.peak();
        ledger.set_limit(before + allowance);
        assert!(matches!(
            Stores::new(&graph, &snapshot),
            Err(JitError::ResourceLimit("helper call verification"))
        ));
        assert_eq!(ledger.current(), before);
        assert_eq!(ledger.peak(), peak.max(before + allowance));
        assert_eq!(ledger.refusals(), 1);
    }

    fn truth_fixture(
        test: Option<bool>,
        change: impl FnOnce(&mut FunctionBuilder<'_>, &mut Stores, &[Block]),
    ) -> Result<(), JitError> {
        use crate::types::{RegisterIndex, VarCount};
        let snapshot = super::super::ir::Snapshot {
            operations: super::super::resources::owned(&[
                if let Some(is_true) = test {
                    Operation::Test {
                        value: RegisterIndex(0),
                        is_true,
                    }
                } else {
                    Operation::Not {
                        dest: RegisterIndex(1),
                        source: RegisterIndex(0),
                    }
                },
                Operation::Return {
                    start: RegisterIndex(1),
                    count: VarCount::constant(1),
                },
                Operation::Return {
                    start: RegisterIndex(1),
                    count: VarCount::constant(1),
                },
            ]),
            constants: super::super::resources::owned(&[]),
            registers: 4,
            upvalues: 0,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        let graph = FlowGraph::new(&snapshot).unwrap();
        let baseline = snapshot.operations.allocator().0.current();
        let mut stores = Stores::new(&graph, &snapshot).unwrap();
        let mut function = Function::new();
        function.signature.params.push(AbiParam::new(types::I64));
        let mut context = FunctionBuilderContext::new();
        let slots;
        let blocks;
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            slots = builder.block_params(entry)[0];
            blocks = [
                builder.create_block(),
                builder.create_block(),
                builder.create_block(),
            ];
            for block in blocks {
                builder.append_block_param(block, types::I32);
            }
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(blocks[0], &[zero.into()]);
            builder.switch_to_block(blocks[0]);
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, 0);
            let bits = builder
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, 8);
            let nil = builder.ins().icmp_imm_s(IntCC::Equal, tag, abi::NIL as i64);
            let boolean = builder
                .ins()
                .icmp_imm_s(IntCC::Equal, tag, abi::BOOLEAN as i64);
            let zero = builder.ins().icmp_imm_s(IntCC::Equal, bits, 0);
            let false_boolean = builder.ins().band(boolean, zero);
            let false_value = builder.ins().bor(nil, false_boolean);
            let truth = builder.ins().bxor_imm_u(false_value, 1);
            let point = if let Some(is_true) = test {
                let condition = if is_true {
                    truth
                } else {
                    builder.ins().bxor_imm_u(truth, 1)
                };
                let count = builder.block_params(blocks[0])[0];
                let increment = builder.ins().iadd_imm_s(count, 1);
                builder.ins().brif(
                    condition,
                    blocks[2],
                    &[increment.into()],
                    blocks[1],
                    &[increment.into()],
                )
            } else {
                let opposite = builder.ins().bxor_imm_u(truth, 1);
                let payload = builder.ins().uextend(types::I64, opposite);
                let tag = builder.ins().iconst(types::I64, abi::BOOLEAN as i64);
                let store = builder.ins().store(MemFlagsData::new(), tag, slots, 16);
                stores.record(store, 1 << abi::BOOLEAN);
                let point = builder.ins().store(MemFlagsData::new(), payload, slots, 24);
                let count = builder.block_params(blocks[0])[0];
                builder.ins().jump(blocks[1], &[count.into()]);
                point
            };
            stores.truth(0, tag, bits, point);
            for block in [blocks[1], blocks[2]] {
                builder.switch_to_block(block);
                builder.ins().return_(&[]);
            }
            change(&mut builder, &mut stores, &blocks);
            builder.seal_all_blocks();
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(settings::Flags::new(settings::builder()))
                .unwrap();
            builder.finalize(isa.frontend_config());
        }
        cranelift_codegen::verify_function(&function, &settings::Flags::new(settings::builder()))
            .unwrap();
        let result = stores
            .verify(&function, slots, None, 4)
            .and_then(|_| stores.verify_truths(&function, slots, &snapshot, &blocks));
        let ledger = snapshot.operations.allocator().0.clone();
        drop(stores);
        assert_eq!(ledger.current(), baseline);
        result
    }

    #[test]
    fn boolean_source_records_accept_not_and_both_test_polarities() {
        for test in [None, Some(false), Some(true)] {
            truth_fixture(test, |_, _, _| {}).unwrap();
        }
    }

    #[test]
    fn boolean_source_records_reject_payload_and_operand_corruption() {
        for fault in [TruthCorruption::Payload, TruthCorruption::Source] {
            rejected(truth_fixture(None, |builder, stores, _| {
                stores.corrupt_truth(builder.func, fault, 4)
            }));
        }
    }

    #[test]
    fn boolean_test_records_reject_polarity_target_and_fuel_corruption() {
        for is_true in [false, true] {
            for fault in [
                TruthCorruption::Condition,
                TruthCorruption::Targets,
                TruthCorruption::Count,
            ] {
                rejected(truth_fixture(Some(is_true), |builder, stores, _| {
                    stores.corrupt_truth(builder.func, fault, 4)
                }));
            }
        }
    }

    #[test]
    fn boolean_record_counts_pc_and_growth_cannot_remove_obligations() {
        for mode in 0..4 {
            rejected(truth_fixture(None, |_, stores, _| match mode {
                0 => stores.truths.clear(),
                1 => stores.truths[0].pc = 1,
                2 => stores.expected_truths = Some(2),
                _ => {
                    let record = stores.truths[0];
                    stores.truths = Vec::new_in(stores.truths.allocator().clone());
                    stores.truth(record.pc, record.tag, record.bits, record.point);
                }
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
