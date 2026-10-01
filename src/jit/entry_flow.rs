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
