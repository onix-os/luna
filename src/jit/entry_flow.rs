use allocator_api2::vec::Vec;
use cranelift_codegen::{
    flowgraph::ControlFlowGraph,
    ir::{
        condcodes::IntCC, types, Block, BlockArg, BlockCall, Function, Inst, InstBuilder,
        InstructionData, JumpTableData, Opcode, Value, ValueDef,
    },
};
use cranelift_frontend::FunctionBuilder;

use super::{ir::Snapshot, resources::BudgetAllocator, JitError};

#[derive(Clone, Copy)]
pub(super) struct Point {
    pub trampoline: Block,
    pub body: Block,
    pub region: [u32; 2],
}

#[derive(Clone, Copy)]
pub(super) struct Root {
    pub dispatch: Block,
    pub unknown: Block,
}

pub(super) struct Paths {
    points: Vec<Point, BudgetAllocator>,
    expected: usize,
    overflowed: bool,
}

fn invalid() -> JitError {
    JitError::Compilation("invalid entry or budget data flow".into())
}

impl Paths {
    pub fn new(snapshot: &Snapshot) -> Result<Self, JitError> {
        let mut points = Vec::new_in(snapshot.operations.allocator().clone());
        points
            .try_reserve_exact(snapshot.operations.len())
            .map_err(|_| JitError::ResourceLimit("entry path verification"))?;
        Ok(Self {
            points,
            expected: snapshot.operations.len(),
            overflowed: false,
        })
    }

    pub fn record(&mut self, point: Point) {
        if self.points.len() == self.points.capacity() {
            self.overflowed = true;
        } else {
            self.points.push(point);
        }
    }

    pub fn body(&self, pc: usize) -> Block {
        self.points[pc].body
    }

    pub fn region(&mut self, pc: usize, start: u32, end: u32) {
        self.points[pc].region = [start, end];
    }

    pub fn emit_entry(
        &self,
        builder: &mut FunctionBuilder<'_>,
        headers: &[Block],
        fallback: Block,
    ) -> Root {
        let parameters: [Value; 5] = builder
            .block_params(builder.current_block().unwrap())
            .try_into()
            .unwrap();
        let root = Root {
            dispatch: builder.create_block(),
            unknown: builder.create_block(),
        };
        let length = builder.ins().iconst(types::I64, headers.len() as i64);
        let valid = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, parameters[1], length);
        builder
            .ins()
            .brif(valid, root.dispatch, &[], root.unknown, &[]);
        builder.switch_to_block(root.dispatch);
        let index = builder.ins().ireduce(types::I32, parameters[1]);
        let default = BlockCall::new(root.unknown, [], &mut builder.func.dfg.value_lists);
        let branches: std::vec::Vec<_> = self
            .points
            .iter()
            .map(|point| BlockCall::new(point.trampoline, [], &mut builder.func.dfg.value_lists))
            .collect();
        let table = builder.create_jump_table(JumpTableData::new(default, &branches));
        builder.ins().br_table(index, table);
        for (point, &header) in self.points.iter().zip(headers) {
            builder.switch_to_block(point.trampoline);
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(header, &[zero.into()]);
        }
        builder.switch_to_block(root.unknown);
        let zero = builder.ins().iconst(types::I32, 0);
        builder
            .ins()
            .jump(fallback, &[parameters[1].into(), zero.into()]);
        root
    }

    pub fn verify(
        &self,
        function: &Function,
        snapshot: &Snapshot,
        headers: &[Block],
        root: Root,
        fallback: Block,
        exhausted: Block,
    ) -> Result<(), JitError> {
        if self.overflowed
            || self.points.len() != self.expected
            || self.expected != snapshot.operations.len()
            || headers.len() != self.expected
            || self.expected == 0
            || self.expected > u32::MAX as usize
        {
            return Err(invalid());
        }
        let entry = function.layout.entry_block().ok_or_else(invalid)?;
        let params: [Value; 5] = function
            .dfg
            .block_params(entry)
            .try_into()
            .map_err(|_| invalid())?;
        let [length, test, branch] = instructions::<3>(function, entry)?;
        let length = result(function, length, types::I64)?;
        if !constant(function, length, types::I64, self.expected as u64)
            || !compare(function, test, IntCC::UnsignedLessThan, params[1], length)
        {
            return Err(invalid());
        }
        let valid = result(function, test, types::I8)?;
        let arms = conditional(function, branch, valid)?;
        no_args(function, arms[0], root.dispatch)?;
        no_args(function, arms[1], root.unknown)?;
        let [reduce, table_inst] = instructions::<2>(function, root.dispatch)?;
        let InstructionData::Unary {
            opcode: Opcode::Ireduce,
            arg,
        } = function.dfg.insts[reduce]
        else {
            return Err(invalid());
        };
        let index = result(function, reduce, types::I32)?;
        if !same(function, arg, params[1]) {
            return Err(invalid());
        }
        let InstructionData::BranchTable {
            opcode: Opcode::BrTable,
            arg,
            table,
        } = function.dfg.insts[table_inst]
        else {
            return Err(invalid());
        };
        if !same(function, arg, index) {
            return Err(invalid());
        }
        let table = &function.dfg.jump_tables[table];
        if table.as_slice().len() != self.expected {
            return Err(invalid());
        }
        no_args(function, table.default_block(), root.unknown)?;
        let [zero, unknown_exit] = instructions::<2>(function, root.unknown)?;
        let zero = result(function, zero, types::I32)?;
        if !constant(function, zero, types::I32, 0) {
            return Err(invalid());
        }
        jump(function, unknown_exit, fallback, &[params[1], zero])?;
        let cfg = ControlFlowGraph::with_function(function);
        single_predecessor(&cfg, root.dispatch, branch)?;
        let mut unknown_predecessors = cfg.pred_iter(root.unknown);
        if cfg.pred_iter(root.unknown).count() != 2
            || unknown_predecessors
                .any(|predecessor| predecessor.inst != branch && predecessor.inst != table_inst)
        {
            return Err(invalid());
        }
        for (pc, ((point, &header), &target)) in self
            .points
            .iter()
            .zip(headers)
            .zip(table.as_slice())
            .enumerate()
        {
            no_args(function, target, point.trampoline)?;
            single_predecessor(&cfg, point.trampoline, table_inst)?;
            let [zero, trampoline_jump] = instructions::<2>(function, point.trampoline)?;
            let zero = result(function, zero, types::I32)?;
            if !constant(function, zero, types::I32, 0) {
                return Err(invalid());
            }
            jump(function, trampoline_jump, header, &[zero])?;
            let count: [Value; 1] = function
                .dfg
                .block_params(header)
                .try_into()
                .map_err(|_| invalid())?;
            if function.dfg.value_type(count[0]) != types::I32 {
                return Err(invalid());
            }
            let [pc_inst, compare_inst, budget_branch] = instructions::<3>(function, header)?;
            let source_pc = result(function, pc_inst, types::I64)?;
            if !constant(function, source_pc, types::I64, pc as u64)
                || !compare(
                    function,
                    compare_inst,
                    IntCC::UnsignedGreaterThanOrEqual,
                    count[0],
                    params[2],
                )
            {
                return Err(invalid());
            }
            let condition = result(function, compare_inst, types::I8)?;
            let arms = conditional(function, budget_branch, condition)?;
            destination(function, arms[0], exhausted, &[source_pc, count[0]])?;
            no_args(function, arms[1], point.body)?;
            if !function.dfg.block_params(point.body).is_empty() {
                return Err(invalid());
            }
            single_predecessor(&cfg, point.body, budget_branch)?;
        }
        Ok(())
    }
}

impl Paths {
    fn owner(&self, block: Block) -> Option<usize> {
        let id = block.as_u32();
        if let Ok(pc) = self
            .points
            .binary_search_by_key(&id, |point| point.body.as_u32())
        {
            return Some(pc);
        }
        let pc = self.points.partition_point(|point| point.region[1] <= id);
        self.points
            .get(pc)
            .filter(|point| point.region[0] <= id)
            .map(|_| pc)
    }

    pub fn owns_inst(&self, function: &Function, pc: usize, inst: Inst) -> bool {
        function
            .layout
            .inst_block(inst)
            .is_some_and(|block| self.owner(block) == Some(pc))
    }

    pub fn owns_value(&self, function: &Function, pc: usize, value: Value) -> bool {
        match function.dfg.value_def(function.dfg.resolve_aliases(value)) {
            ValueDef::Result(inst, _) => self.owns_inst(function, pc, inst),
            ValueDef::Param(block, _) => self.owner(block) == Some(pc),
            ValueDef::Union(_, _) => false,
        }
    }

    pub fn verify_regions(
        &self,
        function: &Function,
        graph: &super::flow::FlowGraph,
        headers: &[Block],
        root: Root,
        handlers: &[Block; 4],
    ) -> Result<(), JitError> {
        use cranelift_codegen::dominator_tree::DominatorTree;
        let rejected = || JitError::Compilation("invalid source region data flow".into());
        if self.overflowed
            || self.expected != self.points.len()
            || self.expected != headers.len()
            || self.expected != graph.nodes.len()
            || headers
                .windows(2)
                .any(|pair| pair[0].as_u32() >= pair[1].as_u32())
            || self.points.windows(2).any(|pair| {
                pair[0].body.as_u32() >= pair[1].body.as_u32()
                    || pair[0].trampoline.as_u32() >= pair[1].trampoline.as_u32()
            })
        {
            return Err(rejected());
        }
        let mut frontier = root.unknown.as_u32().checked_add(1).ok_or_else(rejected)?;
        for point in &self.points {
            if point.region[0] != frontier || point.region[1] < frontier {
                return Err(rejected());
            }
            frontier = point.region[1];
        }
        if frontier as usize != function.dfg.num_blocks() {
            return Err(rejected());
        }
        let entry = function.layout.entry_block().ok_or_else(rejected)?;
        let mut effects = Vec::new_in(self.points.allocator().clone());
        effects
            .try_reserve_exact(function.dfg.num_blocks())
            .map_err(|_| JitError::ResourceLimit("source path verification"))?;
        effects.resize(function.dfg.num_blocks(), 0u8);
        let cfg = ControlFlowGraph::with_function(function);
        let dom = DominatorTree::with_function(function, &cfg);
        for block in function.layout.blocks() {
            let Some(pc) = self.owner(block) else {
                if ![entry, root.dispatch, root.unknown].contains(&block)
                    && !handlers.contains(&block)
                    && headers
                        .binary_search_by_key(&block.as_u32(), |block| block.as_u32())
                        .is_err()
                    && self
                        .points
                        .binary_search_by_key(&block.as_u32(), |point| point.trampoline.as_u32())
                        .is_err()
                {
                    return Err(rejected());
                }
                continue;
            };
            if !dom.is_reachable(block)
                || !dom.dominates(self.points[pc].body, block, &function.layout)
                || effects[block.as_u32() as usize] & 1 != 0
            {
                return Err(rejected());
            }
            let count: [Value; 1] = function
                .dfg
                .block_params(headers[pc])
                .try_into()
                .map_err(|_| rejected())?;
            let mut written = effects[block.as_u32() as usize] & 2 != 0;
            effects[block.as_u32() as usize] |= 1;
            let terminal = function.layout.last_inst(block).ok_or_else(rejected)?;
            if !matches!(
                function.dfg.insts[terminal].opcode(),
                Opcode::Jump | Opcode::Brif
            ) {
                return Err(rejected());
            }
            for inst in function.layout.block_insts(block) {
                written |= function.dfg.insts[inst].opcode().can_store();
            }
            for branch in function.dfg.insts[terminal]
                .branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables)
            {
                let target = branch.block(&function.dfg.value_lists);
                if let Some(owner) = self.owner(target) {
                    let first = function.layout.first_inst(target).ok_or_else(rejected)?;
                    if owner != pc
                        || terminal.as_u32() >= first.as_u32()
                        || effects[target.as_u32() as usize] & 1 != 0
                    {
                        return Err(rejected());
                    }
                    effects[target.as_u32() as usize] |= u8::from(written) << 1;
                } else if let Ok(next) =
                    headers.binary_search_by_key(&target.as_u32(), |block| block.as_u32())
                {
                    let mut args = branch.args(&function.dfg.value_lists);
                    if !graph.permits_edge(pc, next)
                        || !matches!(args.next(), Some(BlockArg::Value(value))
                            if super::shape::increment(function, value, count[0]))
                        || args.next().is_some()
                    {
                        return Err(rejected());
                    }
                } else if let Some(reason) = handlers.iter().position(|&block| block == target) {
                    use super::exits::Kind;
                    let kind = match reason {
                        0 => Kind::Interpreter,
                        1 => Kind::Guard,
                        3 => Kind::Panic,
                        _ => return Err(rejected()),
                    };
                    let state = graph.nodes[pc]
                        .exit
                        .state(kind, written)
                        .ok_or_else(rejected)?;
                    let mut args = branch.args(&function.dfg.value_lists);
                    if !matches!(args.next(), Some(BlockArg::Value(value))
                        if constant(function, value, types::I64, u64::from(state.resume_pc)))
                        || !matches!(args.next(), Some(BlockArg::Value(value)) if same(function, value, count[0]))
                        || args.next().is_some()
                    {
                        return Err(rejected());
                    }
                } else {
                    return Err(rejected());
                }
            }
        }
        Ok(())
    }
}

fn instructions<const N: usize>(function: &Function, block: Block) -> Result<[Inst; N], JitError> {
    let mut instructions = function.layout.block_insts(block);
    let mut result = [Inst::from_u32(0); N];
    for slot in &mut result {
        *slot = instructions.next().ok_or_else(invalid)?;
    }
    if instructions.next().is_some() {
        return Err(invalid());
    }
    Ok(result)
}
fn result(
    function: &Function,
    inst: Inst,
    ty: cranelift_codegen::ir::Type,
) -> Result<Value, JitError> {
    let [value] = function.dfg.inst_results(inst) else {
        return Err(invalid());
    };
    if function.dfg.value_type(*value) != ty {
        return Err(invalid());
    }
    Ok(*value)
}
fn same(function: &Function, left: Value, right: Value) -> bool {
    function.dfg.resolve_aliases(left) == function.dfg.resolve_aliases(right)
}
fn constant(
    function: &Function,
    value: Value,
    ty: cranelift_codegen::ir::Type,
    expected: u64,
) -> bool {
    let value = function.dfg.resolve_aliases(value);
    let ValueDef::Result(inst, 0) = function.dfg.value_def(value) else {
        return false;
    };
    function.dfg.value_type(value) == ty
        && matches!(function.dfg.insts[inst], InstructionData::UnaryImm {
        opcode: Opcode::Iconst, imm } if imm.bits() as u64 == expected)
}
fn compare(function: &Function, inst: Inst, expected: IntCC, left: Value, right: Value) -> bool {
    matches!(function.dfg.insts[inst], InstructionData::IntCompare {
        opcode: Opcode::Icmp, cond, args } if cond == expected && same(function, args[0], left) && same(function, args[1], right))
}
fn conditional(
    function: &Function,
    inst: Inst,
    condition: Value,
) -> Result<[BlockCall; 2], JitError> {
    match function.dfg.insts[inst] {
        InstructionData::Brif {
            opcode: Opcode::Brif,
            arg,
            blocks,
        } if same(function, arg, condition) => Ok(blocks),
        _ => Err(invalid()),
    }
}
fn no_args(function: &Function, branch: BlockCall, block: Block) -> Result<(), JitError> {
    destination(function, branch, block, &[])
}
fn destination(
    function: &Function,
    branch: BlockCall,
    block: Block,
    values: &[Value],
) -> Result<(), JitError> {
    if branch.block(&function.dfg.value_lists) != block {
        return Err(invalid());
    }
    let mut args = branch.args(&function.dfg.value_lists);
    for &value in values {
        if !matches!(args.next(), Some(BlockArg::Value(arg)) if same(function, arg, value)) {
            return Err(invalid());
        }
    }
    if args.next().is_some() {
        return Err(invalid());
    }
    Ok(())
}
fn jump(function: &Function, inst: Inst, block: Block, values: &[Value]) -> Result<(), JitError> {
    match function.dfg.insts[inst] {
        InstructionData::Jump {
            opcode: Opcode::Jump,
            destination: branch,
        } => destination(function, branch, block, values),
        _ => Err(invalid()),
    }
}
fn single_predecessor(cfg: &ControlFlowGraph, block: Block, inst: Inst) -> Result<(), JitError> {
    let mut predecessors = cfg.pred_iter(block);
    if !predecessors
        .next()
        .is_some_and(|predecessor| predecessor.inst == inst)
        || predecessors.next().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Fault {
    EntryLength,
    EntryPolarity,
    EntrySource,
    DispatchSource,
    DispatchIndex,
    TableTarget,
    TableDefault,
    InitialCount,
    TrampolineTarget,
    UnknownPc,
    UnknownCount,
    BudgetPc,
    BudgetCondition,
    BudgetOperands,
    BudgetTarget,
    BudgetCount,
    BudgetBody,
    ExtraBodyEntry,
    HeaderStore,
    Missing,
    Growth,
    Root,
}

#[cfg(test)]
impl Paths {
    pub fn corrupt(
        &mut self,
        function: &mut Function,
        headers: &[Block],
        root: &mut Root,
        fallback: Block,
        fault: Fault,
    ) {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::MemFlagsData,
        };
        use Fault::*;
        let entry = function.layout.entry_block().unwrap();
        let parameters: [Value; 5] = function.dfg.block_params(entry).try_into().unwrap();
        let entry_insts = instructions::<3>(function, entry).unwrap();
        let dispatch = instructions::<2>(function, root.dispatch).unwrap();
        let unknown = instructions::<2>(function, root.unknown).unwrap();
        let trampoline = instructions::<2>(function, self.points[0].trampoline).unwrap();
        let header = instructions::<3>(function, headers[0]).unwrap();
        let count = function.dfg.block_params(headers[0])[0];
        match fault {
            EntryLength | InitialCount | UnknownCount | BudgetPc => {
                let inst = match fault {
                    EntryLength => entry_insts[0],
                    InitialCount => trampoline[0],
                    UnknownCount => unknown[0],
                    BudgetPc => header[0],
                    _ => unreachable!(),
                };
                let InstructionData::UnaryImm { imm, .. } = &mut function.dfg.insts[inst] else {
                    unreachable!()
                };
                *imm = 1.into();
            }
            EntryPolarity | EntrySource | BudgetCondition | BudgetOperands => {
                let inst = if matches!(fault, EntryPolarity | EntrySource) {
                    entry_insts[1]
                } else {
                    header[1]
                };
                let InstructionData::IntCompare { cond, args, .. } = &mut function.dfg.insts[inst]
                else {
                    unreachable!()
                };
                match fault {
                    EntryPolarity => *cond = IntCC::SignedLessThan,
                    EntrySource => args[0] = parameters[0],
                    BudgetCondition => *cond = IntCC::UnsignedLessThan,
                    BudgetOperands => args.swap(0, 1),
                    _ => unreachable!(),
                }
            }
            DispatchSource => {
                let InstructionData::Unary { arg, .. } = &mut function.dfg.insts[dispatch[0]]
                else {
                    unreachable!()
                };
                *arg = parameters[0];
            }
            DispatchIndex => {
                let InstructionData::BranchTable { arg, .. } = &mut function.dfg.insts[dispatch[1]]
                else {
                    unreachable!()
                };
                *arg = parameters[2];
            }
            TableTarget | TableDefault => {
                let InstructionData::BranchTable { table, .. } = function.dfg.insts[dispatch[1]]
                else {
                    unreachable!()
                };
                if fault == TableTarget {
                    function.dfg.jump_tables[table].as_mut_slice().swap(0, 1);
                } else {
                    let wrong = function.dfg.jump_tables[table].as_slice()[0];
                    *function.dfg.jump_tables[table].default_block_mut() = wrong;
                }
            }
            TrampolineTarget | UnknownPc | ExtraBodyEntry => {
                let (inst, target, values): (Inst, Block, std::vec::Vec<Value>) = match fault {
                    TrampolineTarget => (
                        trampoline[1],
                        headers[1],
                        vec![result(function, trampoline[0], types::I32).unwrap()],
                    ),
                    UnknownPc => (
                        unknown[1],
                        fallback,
                        vec![
                            parameters[0],
                            result(function, unknown[0], types::I32).unwrap(),
                        ],
                    ),
                    ExtraBodyEntry => (
                        function
                            .layout
                            .last_inst(self.points.last().unwrap().body)
                            .unwrap(),
                        self.points[0].body,
                        vec![],
                    ),
                    _ => unreachable!(),
                };
                let destination = BlockCall::new(
                    target,
                    values.into_iter().map(BlockArg::Value),
                    &mut function.dfg.value_lists,
                );
                function.dfg.insts[inst] = InstructionData::Jump {
                    opcode: Opcode::Jump,
                    destination,
                };
            }
            BudgetTarget | BudgetCount | BudgetBody => {
                let InstructionData::Brif { blocks, .. } = function.dfg.insts[header[2]] else {
                    unreachable!()
                };
                let ordinal = usize::from(fault == BudgetBody);
                let target = if fault == BudgetTarget {
                    fallback
                } else if fault == BudgetBody {
                    self.points[1].body
                } else {
                    blocks[0].block(&function.dfg.value_lists)
                };
                let values = if ordinal == 0 {
                    vec![
                        result(function, header[0], types::I64).unwrap(),
                        if fault == BudgetCount {
                            parameters[2]
                        } else {
                            count
                        },
                    ]
                } else {
                    vec![]
                };
                let replacement = BlockCall::new(
                    target,
                    values.into_iter().map(BlockArg::Value),
                    &mut function.dfg.value_lists,
                );
                let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[header[2]]
                else {
                    unreachable!()
                };
                blocks[ordinal] = replacement;
            }
            HeaderStore => {
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(header[2]);
                cursor
                    .ins()
                    .store(MemFlagsData::new(), parameters[1], parameters[0], 0);
            }
            Missing => self.points.clear(),
            Growth => self.record(self.points[0]),
            Root => root.dispatch = root.unknown,
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RegionFault {
    Gap,
    End,
    Missing,
    Fuel,
    Successor,
    Retry,
    Pc,
    Count,
    Kind,
    Root,
    Cycle,
    Cross,
    Unreachable,
}

#[cfg(test)]
impl Paths {
    pub fn corrupt_region(
        &mut self,
        function: &mut Function,
        headers: &[Block],
        root: Root,
        handlers: &[Block; 4],
        fault: RegionFault,
    ) {
        use cranelift_codegen::cursor::{Cursor, FuncCursor};
        use RegionFault::*;
        let entry = function.layout.entry_block().unwrap();
        let parameters: [Value; 5] = function.dfg.block_params(entry).try_into().unwrap();
        let first = self.points[0].body;
        let last_pc = self.points.len() - 1;
        let last = self.points[last_pc].body;
        let source_terminal = function.layout.last_inst(first).unwrap();
        let terminal = function.layout.last_inst(last).unwrap();
        match fault {
            Gap => self.points[0].region[0] += 1,
            End => self.points[last_pc].region[1] += 1,
            Missing => self.points[0].region = [0, 0],
            Fuel => {
                let InstructionData::Jump { destination, .. } = function.dfg.insts[source_terminal]
                else {
                    unreachable!()
                };
                let Some(BlockArg::Value(value)) =
                    destination.args(&function.dfg.value_lists).next()
                else {
                    unreachable!()
                };
                let inst = function.dfg.value_def(value).unwrap_inst();
                let InstructionData::Binary { opcode, .. } = &mut function.dfg.insts[inst] else {
                    unreachable!()
                };
                *opcode = Opcode::Isub;
            }
            Successor | Retry | Root | Cycle => {
                let count = function.dfg.block_params(headers[0])[0];
                let (target, args) = if fault == Successor {
                    let InstructionData::Jump { destination, .. } =
                        function.dfg.insts[source_terminal]
                    else {
                        unreachable!()
                    };
                    let Some(BlockArg::Value(value)) =
                        destination.args(&function.dfg.value_lists).next()
                    else {
                        unreachable!()
                    };
                    (headers[0], vec![value])
                } else if fault == Retry {
                    let mut cursor = FuncCursor::new(function);
                    cursor.goto_inst(source_terminal);
                    let pc = cursor.ins().iconst(types::I64, 0);
                    (handlers[1], vec![pc, count])
                } else if fault == Root {
                    (root.unknown, vec![])
                } else {
                    (first, vec![])
                };
                let destination = BlockCall::new(
                    target,
                    args.into_iter().map(BlockArg::Value),
                    &mut function.dfg.value_lists,
                );
                function.dfg.insts[source_terminal] = InstructionData::Jump {
                    opcode: Opcode::Jump,
                    destination,
                };
            }
            Pc | Count | Kind => {
                let InstructionData::Jump { destination, .. } = function.dfg.insts[terminal] else {
                    unreachable!()
                };
                let mut args: std::vec::Vec<_> = destination
                    .args(&function.dfg.value_lists)
                    .map(|arg| {
                        let BlockArg::Value(value) = arg else {
                            unreachable!()
                        };
                        value
                    })
                    .collect();
                if fault == Pc {
                    args[0] = parameters[1];
                }
                if fault == Count {
                    args[1] = parameters[2];
                }
                let target = if fault == Kind {
                    handlers[1]
                } else {
                    handlers[0]
                };
                let destination = BlockCall::new(
                    target,
                    args.into_iter().map(BlockArg::Value),
                    &mut function.dfg.value_lists,
                );
                function.dfg.insts[terminal] = InstructionData::Jump {
                    opcode: Opcode::Jump,
                    destination,
                };
            }
            Cross | Unreachable => {
                let block = function.dfg.make_block();
                function.layout.append_block(block);
                let mut cursor = FuncCursor::new(function);
                cursor.goto_bottom(block);
                cursor
                    .ins()
                    .jump(if fault == Cross { first } else { last }, &[]);
                self.points[last_pc].region[1] += 1;
                if fault == Cross {
                    let destination = BlockCall::new(block, [], &mut function.dfg.value_lists);
                    function.dfg.insts[source_terminal] = InstructionData::Jump {
                        opcode: Opcode::Jump,
                        destination,
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod region_tests {
    use super::*;
    use crate::{
        opcode::Operation,
        types::{RegisterIndex as R, VarCount},
    };
    use cranelift_codegen::ir::{AbiParam, MemFlagsData};
    use cranelift_frontend::FunctionBuilderContext;

    fn fixture(fault: Option<RegionFault>, quota: bool) -> Result<(), JitError> {
        fixture_with_write(fault, quota, true)
    }

    fn fixture_with_write(
        fault: Option<RegionFault>,
        quota: bool,
        write: bool,
    ) -> Result<(), JitError> {
        let source = Snapshot {
            operations: super::super::resources::owned(&[
                Operation::Move {
                    dest: R(0),
                    source: R(0),
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
        let ledger = source.operations.allocator().0.clone();
        let baseline = ledger.current();
        let graph = super::super::flow::FlowGraph::new(&source).unwrap();
        let mut paths = Paths::new(&source).unwrap();
        let mut function = Function::new();
        function.signature.params.extend(
            [types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new),
        );
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let parameters: [Value; 5] = builder.block_params(entry).try_into().unwrap();
        let headers = std::array::from_fn::<_, 2, _>(|_| {
            let block = builder.create_block();
            builder.append_block_param(block, types::I32);
            block
        });
        let handlers = std::array::from_fn::<_, 4, _>(|_| {
            let block = builder.create_block();
            builder.append_block_param(block, types::I64);
            builder.append_block_param(block, types::I32);
            block
        });
        for _ in &headers {
            paths.record(Point {
                trampoline: builder.create_block(),
                body: builder.create_block(),
                region: [0, 0],
            });
        }
        let root = paths.emit_entry(&mut builder, &headers, handlers[0]);
        for (pc, &header) in headers.iter().enumerate() {
            let start = builder.func.dfg.num_blocks() as u32;
            builder.switch_to_block(header);
            let count = builder.block_params(header)[0];
            let source_pc = builder.ins().iconst(types::I64, pc as i64);
            let limit = builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, count, parameters[2]);
            builder.ins().brif(
                limit,
                handlers[2],
                &[source_pc.into(), count.into()],
                paths.body(pc),
                &[],
            );
            builder.switch_to_block(paths.body(pc));
            if pc == 0 {
                let left = builder.create_block();
                let right = builder.create_block();
                let join = builder.create_block();
                let test = builder.ins().iconst(types::I8, 1);
                builder.ins().brif(test, left, &[], right, &[]);
                builder.switch_to_block(left);
                let tag = builder
                    .ins()
                    .iconst(types::I64, super::super::abi::INTEGER as i64);
                if write {
                    builder
                        .ins()
                        .store(MemFlagsData::new(), tag, parameters[0], 0);
                }
                builder.ins().jump(join, &[]);
                builder.switch_to_block(right);
                builder.ins().jump(join, &[]);
                builder.switch_to_block(join);
                let increment = builder.ins().iadd_imm_s(count, 1);
                builder.ins().jump(headers[1], &[increment.into()]);
            } else {
                builder
                    .ins()
                    .jump(handlers[0], &[source_pc.into(), count.into()]);
            }
            paths.region(pc, start, builder.func.dfg.num_blocks() as u32);
        }
        for &handler in &handlers {
            builder.switch_to_block(handler);
            builder.ins().return_(&[]);
        }
        builder.seal_all_blocks();
        builder.finalize(
            cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(cranelift_codegen::settings::Flags::new(
                    cranelift_codegen::settings::builder(),
                ))
                .unwrap()
                .frontend_config(),
        );
        if let Some(fault) = fault {
            if matches!(
                fault,
                RegionFault::Fuel
                    | RegionFault::Successor
                    | RegionFault::Retry
                    | RegionFault::Root
                    | RegionFault::Cycle
                    | RegionFault::Cross
            ) {
                let body = paths.points[0].body;
                let private = Block::from_u32(paths.points[0].region[1] - 1);
                paths.points[0].body = private;
                paths.corrupt_region(&mut function, &headers, root, &handlers, fault);
                paths.points[0].body = body;
            } else {
                paths.corrupt_region(&mut function, &headers, root, &handlers, fault);
            }
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        let before = ledger.current();
        if quota {
            ledger.set_limit(before);
        }
        let result = paths.verify_regions(&function, &graph, &headers, root, &handlers);
        assert_eq!(ledger.current(), before);
        if quota {
            assert!(matches!(
                result,
                Err(JitError::ResourceLimit("source path verification"))
            ));
            assert_eq!(ledger.refusals(), 1);
        }
        drop(paths);
        drop(graph);
        assert_eq!(ledger.current(), baseline);
        result
    }
    #[test]
    fn private_dag_and_completed_source_transition_are_admitted() {
        fixture(None, false).unwrap();
    }

    #[test]
    fn effect_free_guard_exit_is_admitted() {
        fixture_with_write(Some(RegionFault::Retry), false, false).unwrap();
    }
    #[test]
    fn workspace_quota_refusal_releases_storage() {
        assert!(matches!(
            fixture(None, true),
            Err(JitError::ResourceLimit("source path verification"))
        ));
    }
    macro_rules! rejected {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                let result = fixture(Some(RegionFault::$fault), false);
                assert!(matches!(result, Err(JitError::Compilation(ref message))
                    if message == "invalid source region data flow"), "{result:?}");
            }
        };
    }
    rejected!(gap_is_refused, Gap);
    rejected!(end_is_refused, End);
    rejected!(missing_is_refused, Missing);
    rejected!(fuel_is_refused, Fuel);
    rejected!(successor_is_refused, Successor);
    rejected!(retry_is_refused, Retry);
    rejected!(pc_is_refused, Pc);
    rejected!(count_is_refused, Count);
    rejected!(kind_is_refused, Kind);
    rejected!(root_is_refused, Root);
    rejected!(cycle_is_refused, Cycle);
    rejected!(cross_is_refused, Cross);
    rejected!(unreachable_is_refused, Unreachable);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        opcode::Operation,
        types::{RegisterIndex, VarCount},
    };
    use cranelift_codegen::ir::AbiParam;
    use cranelift_frontend::FunctionBuilderContext;

    fn snapshot(length: usize) -> Snapshot {
        let mut source = Snapshot {
            operations: super::super::resources::owned(&[]),
            constants: super::super::resources::owned(&[]),
            registers: 1,
            upvalues: 0,
            prototypes: 0,
        };
        source
            .operations
            .extend((0..length).map(|_| Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(0),
            }));
        source
    }

    fn fixture(length: usize, fault: Option<Fault>) -> Result<(), JitError> {
        let source = snapshot(length);
        let ledger = source.operations.allocator().0.clone();
        let baseline = ledger.current();
        let mut paths = Paths::new(&source).unwrap();
        let mut function = Function::new();
        function.signature.params.extend(
            [types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new),
        );
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let parameters: [Value; 5] = builder.block_params(entry).try_into().unwrap();
        let headers: std::vec::Vec<_> = (0..length)
            .map(|_| {
                let block = builder.create_block();
                builder.append_block_param(block, types::I32);
                block
            })
            .collect();
        let fallback = builder.create_block();
        let exhausted = builder.create_block();
        for block in [fallback, exhausted] {
            builder.append_block_param(block, types::I64);
            builder.append_block_param(block, types::I32);
        }
        for _ in 0..length {
            paths.record(Point {
                trampoline: builder.create_block(),
                body: builder.create_block(),
                region: [0, 0],
            });
        }
        let mut root = paths.emit_entry(&mut builder, &headers, fallback);
        for (pc, &header) in headers.iter().enumerate() {
            builder.switch_to_block(header);
            let count = builder.block_params(header)[0];
            let source_pc = builder.ins().iconst(types::I64, pc as i64);
            let condition =
                builder
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThanOrEqual, count, parameters[2]);
            builder.ins().brif(
                condition,
                exhausted,
                &[source_pc.into(), count.into()],
                paths.body(pc),
                &[],
            );
            builder.switch_to_block(paths.body(pc));
            builder
                .ins()
                .jump(fallback, &[parameters[1].into(), parameters[2].into()]);
        }
        for block in [fallback, exhausted] {
            builder.switch_to_block(block);
            builder.ins().return_(&[]);
        }
        builder.seal_all_blocks();
        builder.finalize(
            cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(cranelift_codegen::settings::Flags::new(
                    cranelift_codegen::settings::builder(),
                ))
                .unwrap()
                .frontend_config(),
        );
        if let Some(fault) = fault {
            paths.corrupt(&mut function, &headers, &mut root, fallback, fault);
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        let result = paths.verify(&function, &source, &headers, root, fallback, exhausted);
        drop(paths);
        assert_eq!(ledger.current(), baseline);
        result
    }

    #[test]
    fn dense_entry_and_budget_routes_match_source_counts() {
        for length in [1, 2, 7, 257] {
            fixture(length, None).unwrap();
        }
    }
    #[test]
    fn point_quota_refusal_and_successful_drop_release_storage() {
        let source = snapshot(2);
        let ledger = source.operations.allocator().0.clone();
        let baseline = ledger.current();
        ledger.set_limit(baseline);
        assert!(matches!(
            Paths::new(&source),
            Err(JitError::ResourceLimit("entry path verification"))
        ));
        assert_eq!(ledger.current(), baseline);
        assert_eq!(ledger.refusals(), 1);
        ledger.set_limit(baseline + 2 * std::mem::size_of::<Point>());
        let paths = Paths::new(&source).unwrap();
        assert_eq!(
            ledger.current(),
            baseline + 2 * std::mem::size_of::<Point>()
        );
        drop(paths);
        assert_eq!(ledger.current(), baseline);
    }
    macro_rules! rejected {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                let result = fixture(2, Some(Fault::$fault));
                assert!(matches!(result, Err(JitError::Compilation(ref message))
                    if message == "invalid entry or budget data flow"), "{result:?}");
            }
        };
    }
    rejected!(entry_length_is_refused, EntryLength);
    rejected!(entry_polarity_is_refused, EntryPolarity);
    rejected!(entry_source_is_refused, EntrySource);
    rejected!(dispatch_source_is_refused, DispatchSource);
    rejected!(dispatch_index_is_refused, DispatchIndex);
    rejected!(table_target_is_refused, TableTarget);
    rejected!(table_default_is_refused, TableDefault);
    rejected!(initial_count_is_refused, InitialCount);
    rejected!(trampoline_target_is_refused, TrampolineTarget);
    rejected!(unknown_pc_is_refused, UnknownPc);
    rejected!(unknown_count_is_refused, UnknownCount);
    rejected!(budget_pc_is_refused, BudgetPc);
    rejected!(budget_condition_is_refused, BudgetCondition);
    rejected!(budget_operands_is_refused, BudgetOperands);
    rejected!(budget_target_is_refused, BudgetTarget);
    rejected!(budget_count_is_refused, BudgetCount);
    rejected!(budget_body_is_refused, BudgetBody);
    rejected!(extra_body_entry_is_refused, ExtraBodyEntry);
    rejected!(header_store_is_refused, HeaderStore);
    rejected!(missing_is_refused, Missing);
    rejected!(growth_is_refused, Growth);
    rejected!(root_is_refused, Root);
}
