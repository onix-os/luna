use allocator_api2::vec::Vec;
use cranelift_codegen::ir::{
    condcodes::IntCC, types, AbiParam, Block, BlockArg, FuncRef, Function, Inst, InstructionData,
    Opcode, Type, Value, ValueDef,
};

use super::{abi, ir::Snapshot, preds::Predecessors, resources::BudgetAllocator, JitError};
use crate::opcode::{Operation, RCIndex};

#[derive(Clone, Copy)]
pub(super) struct Record {
    pub pc: usize,
    pub call: Inst,
    pub completed: Inst,
    pub success: Inst,
    pub declined: Inst,
}

pub(super) struct Calls {
    pub(super) prepared_keys: bool,
    #[cfg(test)]
    pub(super) wide_prepared: bool,
    records: Vec<Record, BudgetAllocator>,
    expected: Option<usize>,
    overflowed: bool,
    rooted_moves: bool,
}

pub(super) struct Boundary<'a> {
    pub slots: Value,
    pub host: Value,
    pub blocks: &'a [Block],
    pub imports: &'a [(u32, FuncRef)],
    pub fallback: Block,
    pub panicked: Block,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Fault {
    Symbol,
    Pointer,
    Operand,
    ConstantFlag,
    PreparedLow,
    PreparedHigh,
    SourcePc,
    Signature,
    CompletedTest,
    SuccessTarget,
    SuccessCount,
    PanicTest,
    ExitTarget,
    ExitPc,
    ExitCount,
    Missing,
    ProgramCounter,
    Growth,
    ExtraCall,
    ExtraStore,
}

fn invalid() -> JitError {
    JitError::Compilation("invalid helper call data flow".into())
}
fn refused() -> JitError {
    JitError::ResourceLimit("helper call verification")
}

#[cfg(test)]
impl Calls {
    pub(super) fn read_cache_clobbers(&self, snapshot: &Snapshot, inst: Inst) -> u32 {
        let Some(record) = self.records.iter().find(|record| record.call == inst) else {
            return u32::MAX;
        };
        match snapshot.operations.get(record.pc) {
            Some(
                Operation::GetTable { dest, .. }
                | Operation::GetUpTable { dest, .. }
                | Operation::GetUpValue { dest, .. },
            ) if dest.0 < 16 => 3 << (2 * dest.0),
            _ => u32::MAX,
        }
    }
}

#[cfg(test)]
mod read_effect_tests {
    use super::*;
    use crate::types::{RegisterIndex as R, UpValueIndex as U};

    #[test]
    fn getter_effects_bound_destinations_and_keep_dynamic_writes_opaque() {
        for dest in 0..=255 {
            let snapshot = Snapshot {
                prepared_keys: None,
                operations: super::super::resources::owned(&[
                    Operation::GetTable {
                        dest: R(dest),
                        table: R(1),
                        key: R(2).into(),
                    },
                    Operation::GetUpTable {
                        dest: R(dest),
                        table: U(0),
                        key: R(2).into(),
                    },
                    Operation::GetUpValue {
                        dest: R(dest),
                        source: U(0),
                    },
                    Operation::SetUpValue {
                        dest: U(0),
                        source: R(dest),
                    },
                    Operation::SetTable {
                        table: R(1),
                        key: R(2).into(),
                        value: R(dest).into(),
                    },
                    Operation::Return {
                        start: R(0),
                        count: crate::types::VarCount::constant(0),
                    },
                ]),
                constants: super::super::resources::owned(&[]),
                registers: 256,
                upvalues: 1,
                prototypes: 0,
            };
            snapshot.verify().unwrap();
            let mut calls = Calls::new(&snapshot).unwrap();
            for pc in 0..snapshot.operations.len() - 1 {
                let inst = Inst::from_u32(pc as u32);
                calls.record(Record {
                    pc,
                    call: inst,
                    completed: inst,
                    success: inst,
                    declined: inst,
                });
                let expected = if pc < 3 && dest < 16 {
                    3 << (2 * dest)
                } else {
                    u32::MAX
                };
                assert_eq!(calls.read_cache_clobbers(&snapshot, inst), expected);
            }
            assert_eq!(
                calls.read_cache_clobbers(&snapshot, Inst::from_u32(900)),
                u32::MAX
            );
            calls.records[0].pc = usize::MAX;
            assert_eq!(
                calls.read_cache_clobbers(&snapshot, Inst::from_u32(0)),
                u32::MAX
            );
        }
    }
}

impl Calls {
    pub(super) fn wide_prepared(&self) -> bool {
        #[cfg(test)]
        {
            self.wide_prepared
        }
        #[cfg(not(test))]
        {
            false
        }
    }
    #[cfg(test)]
    pub fn corrupt_ownership(&mut self) {
        self.records[0].pc += 1;
    }
    pub fn verify_ownership(
        &self,
        function: &Function,
        paths: &super::entry_flow::Paths,
    ) -> Result<(), JitError> {
        for record in &self.records {
            if [
                record.call,
                record.completed,
                record.success,
                record.declined,
            ]
            .into_iter()
            .any(|inst| !paths.owns_inst(function, record.pc, inst))
            {
                return Err(JitError::Compilation(
                    "invalid semantic source ownership".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn new(snapshot: &Snapshot) -> Result<Self, JitError> {
        let mut count = 0usize;
        for &op in &snapshot.operations {
            if expected(op, snapshot).is_some() {
                count = count.checked_add(1).ok_or_else(refused)?;
            }
        }
        let mut records = Vec::new_in(snapshot.operations.allocator().clone());
        records.try_reserve_exact(count).map_err(|_| refused())?;
        Ok(Self {
            prepared_keys: false,
            #[cfg(test)]
            wide_prepared: false,
            records,
            expected: Some(count),
            overflowed: false,
            rooted_moves: false,
        })
    }

    pub fn rooted(snapshot: &Snapshot) -> Result<Self, JitError> {
        if !rooted_moves_admitted(snapshot) {
            return Err(invalid());
        }
        Ok(Self {
            prepared_keys: false,
            #[cfg(test)]
            wide_prepared: false,
            records: Vec::new_in(snapshot.operations.allocator().clone()),
            expected: Some(0),
            overflowed: false,
            rooted_moves: true,
        })
    }

    #[cfg(test)]
    pub fn empty(allocator: BudgetAllocator) -> Self {
        Self {
            prepared_keys: false,
            wide_prepared: false,
            records: Vec::new_in(allocator),
            expected: None,
            overflowed: false,
            rooted_moves: false,
        }
    }

    pub fn record(&mut self, record: Record) {
        if self.records.len() == self.records.capacity() {
            self.overflowed = true;
            return;
        }
        self.records.push(record);
    }

    #[cfg(test)]
    pub fn corrupt(&mut self, function: &mut Function, fault: Fault, imports: &[(u32, FuncRef)]) {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::InstBuilder,
        };
        use Fault::*;
        let record = if fault == ConstantFlag {
            *self
                .records
                .iter()
                .find(|record| {
                    let InstructionData::Call { func_ref, .. } = function.dfg.insts[record.call]
                    else {
                        return false;
                    };
                    imports.iter().any(|(kind, target)| {
                        matches!(*kind, abi::HELPER_SET_TABLE | abi::HELPER_SET_UP_TABLE)
                            && *target == func_ref
                    })
                })
                .expect("missing constant-key helper")
        } else {
            self.records[0]
        };
        match fault {
            Missing => {
                self.records.clear();
                return;
            }
            ProgramCounter => {
                self.records[0].pc += 1;
                return;
            }
            Growth => {
                self.record(record);
                return;
            }
            _ => {}
        }
        let InstructionData::Call { func_ref, .. } = function.dfg.insts[record.call] else {
            unreachable!()
        };
        let args = function.dfg.inst_args(record.call).to_vec();
        let wide = self.wide_prepared() && args.len() == 4;
        match fault {
            Symbol => {
                let wrong = imports
                    .iter()
                    .map(|(_, function)| *function)
                    .find(|function| *function != func_ref)
                    .unwrap();
                let InstructionData::Call { func_ref, .. } = &mut function.dfg.insts[record.call]
                else {
                    unreachable!()
                };
                *func_ref = wrong;
            }
            Pointer | Operand | ConstantFlag | SourcePc | PreparedLow | PreparedHigh => {
                let mut values = args.clone();
                if fault == Pointer {
                    values.swap(0, 1);
                } else {
                    let mut cursor = FuncCursor::new(function);
                    cursor.goto_inst(record.call);
                    let index = if wide {
                        if matches!(fault, PreparedLow | PreparedHigh) {
                            3
                        } else {
                            2
                        }
                    } else if fault == PreparedLow {
                        3
                    } else if fault == PreparedHigh {
                        4
                    } else if fault == Operand {
                        2
                    } else if fault == ConstantFlag {
                        3
                    } else {
                        5
                    };
                    let ty = if wide { types::I64 } else { types::I32 };
                    let wrong = if wide {
                        literal(cursor.func, args[index], ty).unwrap()
                            ^ match fault {
                                PreparedHigh | SourcePc => 1 << 32,
                                Operand => 1 << 24,
                                _ => 1,
                            }
                    } else if matches!(fault, PreparedLow | PreparedHigh) {
                        literal(cursor.func, args[index], types::I32).unwrap() ^ 1
                    } else if fault == ConstantFlag {
                        literal(cursor.func, args[index], types::I32).unwrap()
                            ^ u64::from(abi::CONSTANT_OPERAND)
                    } else {
                        123
                    };
                    values[index] = cursor.ins().iconst(ty, wrong as i64);
                }
                let list = cranelift_codegen::ir::ValueList::from_slice(
                    &values,
                    &mut function.dfg.value_lists,
                );
                let InstructionData::Call { args, .. } = &mut function.dfg.insts[record.call]
                else {
                    unreachable!()
                };
                *args = list;
            }
            Signature => {
                use cranelift_codegen::isa::CallConv;
                let signature = function.dfg.ext_funcs[func_ref].signature;
                let convention = &mut function.dfg.signatures[signature].call_conv;
                *convention = if *convention == CallConv::Fast {
                    CallConv::SystemV
                } else {
                    CallConv::Fast
                };
            }
            CompletedTest | PanicTest => {
                let point = if fault == CompletedTest {
                    record.completed
                } else {
                    record.declined
                };
                let InstructionData::Brif { arg, .. } = function.dfg.insts[point] else {
                    unreachable!()
                };
                let inst = function.dfg.value_def(arg).unwrap_inst();
                let InstructionData::IntCompare { cond, .. } = &mut function.dfg.insts[inst] else {
                    unreachable!()
                };
                *cond = IntCC::NotEqual;
            }
            SuccessTarget | SuccessCount => {
                let InstructionData::Jump { destination, .. } = function.dfg.insts[record.success]
                else {
                    unreachable!()
                };
                let BlockArg::Value(value) =
                    destination.args(&function.dfg.value_lists).next().unwrap()
                else {
                    unreachable!()
                };
                if fault == SuccessCount {
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
                        [value.into()],
                        &mut function.dfg.value_lists,
                    );
                    let InstructionData::Jump { destination, .. } =
                        &mut function.dfg.insts[record.success]
                    else {
                        unreachable!()
                    };
                    *destination = call;
                }
            }
            ExitTarget => {
                let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[record.declined]
                else {
                    unreachable!()
                };
                blocks.swap(0, 1);
            }
            ExitPc | ExitCount => {
                let InstructionData::Brif { blocks, .. } = function.dfg.insts[record.declined]
                else {
                    unreachable!()
                };
                let status = function.dfg.inst_results(record.call)[0];
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(record.declined);
                let wrong = if fault == ExitPc {
                    cursor.ins().iconst(types::I64, 123)
                } else {
                    status
                };
                let dfg = &mut cursor.func.dfg;
                for (index, block) in blocks.into_iter().enumerate() {
                    let mut arguments = block.args(&dfg.value_lists);
                    let mut values = [arguments.next().unwrap(), arguments.next().unwrap()];
                    assert!(arguments.next().is_none());
                    drop(arguments);
                    values[if fault == ExitPc { 0 } else { 1 }] = wrong.into();
                    let call = cranelift_codegen::ir::BlockCall::new(
                        block.block(&dfg.value_lists),
                        values,
                        &mut dfg.value_lists,
                    );
                    let InstructionData::Brif { blocks, .. } = &mut dfg.insts[record.declined]
                    else {
                        unreachable!()
                    };
                    blocks[index] = call;
                }
            }
            ExtraCall | ExtraStore => {
                let mut cursor = FuncCursor::new(function);
                cursor.goto_inst(record.completed);
                if fault == ExtraCall {
                    cursor.ins().call(func_ref, &args);
                } else {
                    let value = cursor.ins().iconst(types::I64, 0);
                    cursor.ins().store(
                        cranelift_codegen::ir::MemFlagsData::new(),
                        value,
                        args[1],
                        0,
                    );
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn verify(
        &self,
        function: &Function,
        snapshot: &Snapshot,
        boundary: Boundary<'_>,
    ) -> Result<(), JitError> {
        if (self.rooted_moves && !rooted_moves_admitted(snapshot))
            || self.overflowed
            || self
                .expected
                .is_some_and(|count| count != self.records.len())
            || boundary.blocks.len() != snapshot.operations.len()
            || self
                .records
                .windows(2)
                .any(|pair| pair[0].call.as_u32() >= pair[1].call.as_u32())
        {
            return Err(invalid());
        }
        let mut found = 0usize;
        for block in function.layout.blocks() {
            for inst in function.layout.block_insts(block) {
                if function.dfg.insts[inst].opcode().is_call() {
                    if function.dfg.insts[inst].opcode() != Opcode::Call
                        || self
                            .records
                            .binary_search_by_key(&inst.as_u32(), |record| record.call.as_u32())
                            .is_err()
                    {
                        return Err(invalid());
                    }
                    found += 1;
                }
            }
        }
        if found != self.records.len() {
            return Err(invalid());
        }
        let cfg = if !self.records.is_empty() {
            Predecessors::new(function, self.records.allocator().clone())?
        } else {
            Predecessors::empty(self.records.allocator().clone())
        };
        let mut records = self.records.iter();
        if self.wide_prepared() && !self.prepared_keys {
            return Err(invalid());
        }
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            if self.rooted_moves && matches!(op, Operation::Move { .. }) {
                continue;
            }
            let Some((kind, operands)) = expected(op, snapshot) else {
                continue;
            };
            let operands = if self.prepared_keys && kind == abi::HELPER_GET_UP_TABLE {
                let Operation::GetUpTable {
                    dest,
                    table,
                    key: RCIndex::Constant(index),
                } = op
                else {
                    return Err(invalid());
                };
                let keys = snapshot.prepared_keys.as_ref().ok_or_else(invalid)?;
                let hash = keys.hash(index.0);
                [
                    u32::from(dest.0) * 65536 + u32::from(table.0) * 256 + u32::from(index.0),
                    hash as u32,
                    (hash >> 32) as u32,
                ]
            } else {
                operands
            };
            let record = records.next().ok_or_else(invalid)?;
            if record.pc != pc {
                return Err(invalid());
            }
            let InstructionData::Call {
                opcode: Opcode::Call,
                func_ref,
                ..
            } = function.dfg.insts[record.call]
            else {
                return Err(invalid());
            };
            if boundary
                .imports
                .iter()
                .find(|(key, _)| *key == kind)
                .map(|(_, function)| *function)
                != Some(func_ref)
            {
                return Err(invalid());
            }
            let signature = &function.dfg.signatures[function.dfg.ext_funcs[func_ref].signature];
            let wide = self.wide_prepared() && kind == abi::HELPER_GET_UP_TABLE;
            let parameters: &[Type] = if wide {
                &[types::I64, types::I64, types::I64, types::I64]
            } else {
                &[
                    types::I64,
                    types::I64,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                ]
            };
            if signature.call_conv != function.signature.call_conv
                || signature
                    .params
                    .iter()
                    .copied()
                    .ne(parameters.iter().copied().map(AbiParam::new))
                || signature.returns.len() != 1
                || signature.returns[0] != AbiParam::new(types::I32)
            {
                return Err(invalid());
            }
            let args = function.dfg.inst_args(record.call);
            let results = function.dfg.inst_results(record.call);
            if args.len() != parameters.len()
                || results.len() != 1
                || function.dfg.value_type(results[0]) != types::I32
                || !same(function, args[0], boundary.host)
                || !same(function, args[1], boundary.slots)
                || function.dfg.value_type(args[0]) != types::I64
                || function.dfg.value_type(args[1]) != types::I64
            {
                return Err(invalid());
            }
            if wide {
                let expected = [
                    u64::from(operands[0]) | ((pc as u64) << 32),
                    u64::from(operands[1]) | (u64::from(operands[2]) << 32),
                ];
                for (&value, expected) in args[2..].iter().zip(expected) {
                    if literal(function, value, types::I64) != Some(expected) {
                        return Err(invalid());
                    }
                }
            } else {
                for (value, expected) in args[2..]
                    .iter()
                    .copied()
                    .zip(operands.into_iter().chain([pc as u32]))
                {
                    if literal(function, value, types::I32) != Some(u64::from(expected)) {
                        return Err(invalid());
                    }
                }
            }
            let InstructionData::Brif { arg, blocks, .. } = function.dfg.insts[record.completed]
            else {
                return Err(invalid());
            };
            let call_block = function
                .layout
                .inst_block(record.call)
                .ok_or_else(invalid)?;
            if !status_test(function, arg, results[0], abi::HELPER_COMPLETED)
                || function.layout.last_inst(call_block) != Some(record.completed)
            {
                return Err(invalid());
            }
            no_stores(function, call_block)?;
            let arms = blocks.map(|block| block.block(&function.dfg.value_lists));
            for (branch, block) in blocks.into_iter().zip(arms) {
                if branch.args(&function.dfg.value_lists).next().is_some()
                    || !single_predecessor(&cfg, block, record.completed)
                {
                    return Err(invalid());
                }
                no_stores(function, block)?;
            }
            let count = *function
                .dfg
                .block_params(boundary.blocks[pc])
                .first()
                .ok_or_else(invalid)?;
            let InstructionData::Jump { destination, .. } = function.dfg.insts[record.success]
            else {
                return Err(invalid());
            };
            if function.layout.last_inst(arms[0]) != Some(record.success)
                || boundary.blocks.get(pc + 1).copied()
                    != Some(destination.block(&function.dfg.value_lists))
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
            let InstructionData::Brif { arg, blocks, .. } = function.dfg.insts[record.declined]
            else {
                return Err(invalid());
            };
            if function.layout.last_inst(arms[1]) != Some(record.declined)
                || !status_test(function, arg, results[0], abi::HELPER_PANICKED)
            {
                return Err(invalid());
            }
            for (branch, target) in blocks
                .into_iter()
                .zip([boundary.panicked, boundary.fallback])
            {
                let mut args = branch.args(&function.dfg.value_lists);
                let Some(BlockArg::Value(exit_pc)) = args.next() else {
                    return Err(invalid());
                };
                let Some(BlockArg::Value(exit_count)) = args.next() else {
                    return Err(invalid());
                };
                if branch.block(&function.dfg.value_lists) != target
                    || literal(function, exit_pc, types::I64) != Some(pc as u64)
                    || !same(function, exit_count, count)
                    || args.next().is_some()
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
}

pub(super) fn rooted_moves_admitted(snapshot: &Snapshot) -> bool {
    snapshot.verify().is_ok()
        && snapshot
            .operations
            .iter()
            .all(|&op| expected(op, snapshot).is_none_or(|(kind, _)| kind == abi::HELPER_MOVE))
}

fn expected(op: Operation, snapshot: &Snapshot) -> Option<(u32, [u32; 3])> {
    use Operation::*;
    let rc = |operand| match operand {
        RCIndex::Register(index) => u32::from(index.0),
        RCIndex::Constant(index) => abi::CONSTANT_OPERAND | u32::from(index.0),
    };
    Some(match op {
        Move { dest, source } => (
            abi::HELPER_MOVE,
            [u32::from(dest.0), u32::from(source.0), 0],
        ),
        LoadConstant { dest, constant }
            if snapshot
                .constants
                .get(usize::from(constant.0))
                .is_some_and(|slot| slot.tag == abi::REFERENCE) =>
        {
            (
                abi::HELPER_CONSTANT,
                [u32::from(dest.0), u32::from(constant.0), 0],
            )
        }
        NewTable {
            dest,
            array_size,
            map_size,
        } => (
            abi::HELPER_NEW_TABLE,
            [
                u32::from(dest.0),
                u32::from(array_size),
                u32::from(map_size),
            ],
        ),
        GetTable { dest, table, key } => (
            abi::HELPER_GET_TABLE,
            [u32::from(dest.0), u32::from(table.0), rc(key)],
        ),
        SetTable { table, key, value } => (
            abi::HELPER_SET_TABLE,
            [u32::from(table.0), rc(key), rc(value)],
        ),
        GetUpTable { dest, table, key } => (
            abi::HELPER_GET_UP_TABLE,
            [u32::from(dest.0), u32::from(table.0), rc(key)],
        ),
        SetUpTable { table, key, value } => (
            abi::HELPER_SET_UP_TABLE,
            [u32::from(table.0), rc(key), rc(value)],
        ),
        GetUpValue { dest, source } => (
            abi::HELPER_GET_UPVALUE,
            [u32::from(dest.0), u32::from(source.0), 0],
        ),
        SetUpValue { dest, source } => (
            abi::HELPER_SET_UPVALUE,
            [u32::from(dest.0), u32::from(source.0), 0],
        ),
        SetList { base, count } if !count.is_variable() => (
            abi::HELPER_SET_LIST,
            [
                u32::from(base.0),
                u32::from(count.to_constant().unwrap()),
                0,
            ],
        ),
        _ => return None,
    })
}

fn same(function: &Function, left: Value, right: Value) -> bool {
    function.dfg.resolve_aliases(left) == function.dfg.resolve_aliases(right)
}
fn literal(function: &Function, value: Value, ty: Type) -> Option<u64> {
    let value = function.dfg.resolve_aliases(value);
    if function.dfg.value_type(value) != ty {
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
fn status_test(function: &Function, value: Value, status: Value, expected: u32) -> bool {
    let value = function.dfg.resolve_aliases(value);
    if function.dfg.value_type(value) != types::I8 {
        return false;
    }
    let ValueDef::Result(inst, 0) = function.dfg.value_def(value) else {
        return false;
    };
    matches!(function.dfg.insts[inst], InstructionData::IntCompare { opcode: Opcode::Icmp, cond: IntCC::Equal, args } if same(function, args[0], status) && literal(function, args[1], types::I32) == Some(u64::from(expected)))
}
fn single_predecessor(cfg: &Predecessors, block: Block, inst: Inst) -> bool {
    let mut predecessors = cfg.pred_iter(block);
    predecessors
        .next()
        .is_some_and(|predecessor| predecessor.inst == inst)
        && predecessors.next().is_none()
}
fn no_stores(function: &Function, block: Block) -> Result<(), JitError> {
    if function
        .layout
        .block_insts(block)
        .any(|inst| function.dfg.insts[inst].opcode().can_store())
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn read_effects_only(snapshot: &Snapshot) -> bool {
    snapshot.operations.iter().all(|&op| {
        expected(op, snapshot).is_none_or(|(kind, _)| {
            matches!(
                kind,
                abi::HELPER_MOVE
                    | abi::HELPER_CONSTANT
                    | abi::HELPER_GET_TABLE
                    | abi::HELPER_GET_UP_TABLE
                    | abi::HELPER_GET_UPVALUE
            )
        })
    })
}
