use cranelift_codegen::ir::{
    condcodes::IntCC, types, Function, InstBuilder, InstructionData, MemFlagsData, Opcode, Type,
};
use cranelift_frontend::FunctionBuilder;

use super::{native::VERSION, *};
use crate::{jit::JitError, opcode::Operation};

const STEPS: usize = 192;
const VALUES: usize = 96;

#[derive(Clone, Copy)]
enum Expression {
    Constant(Type, i64),
    Load(Type, u8, i32),
    Binary(Opcode, u8, u8),
    Compare(IntCC, u8, u8),
    Extend(u8),
    Select(u8, u8, u8),
}

#[derive(Clone, Copy)]
enum Step {
    Value(u8, Expression),
    Guard(u8),
    Store(u8, u8, i32),
    Return(u8),
}

struct Program {
    steps: [Step; STEPS],
    length: usize,
    values: u8,
}

fn invalid() -> JitError {
    JitError::Compilation("invalid scalar projection flow".into())
}

impl Program {
    fn push(&mut self, step: Step) {
        assert!(self.length < STEPS);
        self.steps[self.length] = step;
        self.length += 1;
    }

    fn value(&mut self, expression: Expression) -> u8 {
        assert!(usize::from(self.values) < VALUES);
        let value = self.values;
        self.values += 1;
        self.push(Step::Value(value, expression));
        value
    }

    fn constant(&mut self, bits: i64) -> u8 {
        self.value(Expression::Constant(types::I64, bits))
    }

    fn load(&mut self, base: u8, offset: i32) -> u8 {
        self.value(Expression::Load(types::I64, base, offset))
    }

    fn binary(&mut self, opcode: Opcode, left: u8, right: u8) -> u8 {
        self.value(Expression::Binary(opcode, left, right))
    }

    fn compare(&mut self, condition: IntCC, left: u8, right: u8) -> u8 {
        self.value(Expression::Compare(condition, left, right))
    }

    fn guard(&mut self, condition: IntCC, left: u8, right: u8) {
        let value = self.compare(condition, left, right);
        self.push(Step::Guard(value));
    }

    fn new(operation: Operation) -> Result<Self, JitError> {
        let (write, register, binding) = match operation {
            Operation::GetUpValue { dest, source } => (false, dest.0, source.0),
            Operation::SetUpValue { dest, source } => (true, source.0, dest.0),
            _ => return Err(invalid()),
        };
        let mut p = Self {
            steps: [Step::Guard(0); STEPS],
            length: 0,
            values: 1,
        };
        let zero = p.constant(0);
        let one = p.constant(1);
        let two = p.constant(2);
        let three = p.constant(3);
        let four = p.constant(4);
        let five = p.constant(5);
        let detached_marker = p.constant(DETACHED as i64);
        let limit = p.constant(LIMIT as i64);
        let expected_version = p.constant(VERSION as i64);
        let binding = p.constant(i64::from(binding));
        let register = p.constant(i64::from(register));
        p.guard(IntCC::NotEqual, 0, zero);
        let version = p.load(0, 0);
        p.guard(IntCC::Equal, version, expected_version);
        let binding_count = p.load(0, 16);
        p.guard(IntCC::UnsignedLessThanOrEqual, binding_count, limit);
        p.guard(IntCC::UnsignedLessThan, binding, binding_count);
        let cell_count = p.load(0, 32);
        p.guard(IntCC::UnsignedLessThanOrEqual, cell_count, limit);
        let slot_count = p.load(0, 48);
        p.guard(IntCC::UnsignedLessThanOrEqual, slot_count, limit);
        p.guard(IntCC::UnsignedLessThan, register, slot_count);
        let bindings = p.load(0, 8);
        p.guard(IntCC::NotEqual, bindings, zero);
        let cells = p.load(0, 24);
        p.guard(IntCC::NotEqual, cells, zero);
        let slots = p.load(0, 40);
        p.guard(IntCC::NotEqual, slots, zero);
        let counts = p.load(0, 56);
        p.guard(IntCC::NotEqual, counts, zero);
        let shift = p.constant(2);
        let binding_offset = p.binary(Opcode::Ishl, binding, shift);
        let binding_pointer = p.binary(Opcode::Iadd, bindings, binding_offset);
        let index = p.value(Expression::Load(types::I32, binding_pointer, 0));
        let index = p.value(Expression::Extend(index));
        p.guard(IntCC::UnsignedLessThan, index, cell_count);
        let cell_offset = p.binary(Opcode::Ishl, index, five);
        let cell = p.binary(Opcode::Iadd, cells, cell_offset);
        let linked_register = p.load(cell, 16);
        let detached = p.compare(IntCC::Equal, linked_register, detached_marker);
        let linked = p.compare(IntCC::UnsignedLessThan, linked_register, slot_count);
        let valid_link = p.binary(Opcode::Bor, detached, linked);
        p.push(Step::Guard(valid_link));
        let linked_offset = p.binary(Opcode::Ishl, linked_register, four);
        let linked_pointer = p.binary(Opcode::Iadd, slots, linked_offset);
        let value_pointer = p.value(Expression::Select(detached, cell, linked_pointer));
        let register_offset = p.binary(Opcode::Ishl, register, four);
        let register_pointer = p.binary(Opcode::Iadd, slots, register_offset);
        let (source, destination) = if write {
            (register_pointer, value_pointer)
        } else {
            (value_pointer, register_pointer)
        };
        let tag = p.load(source, 0);
        let bits = p.load(source, 8);
        p.guard(IntCC::UnsignedLessThanOrEqual, tag, three);
        let numeric = p.compare(IntCC::UnsignedGreaterThanOrEqual, tag, two);
        let nil = p.compare(IntCC::Equal, tag, zero);
        let empty = p.compare(IntCC::Equal, bits, zero);
        let nil = p.binary(Opcode::Band, nil, empty);
        let boolean = p.compare(IntCC::Equal, tag, one);
        let boolean_bits = p.compare(IntCC::UnsignedLessThanOrEqual, bits, one);
        let boolean = p.binary(Opcode::Band, boolean, boolean_bits);
        let valid_payload = p.binary(Opcode::Bor, nil, boolean);
        let valid_payload = p.binary(Opcode::Bor, numeric, valid_payload);
        p.push(Step::Guard(valid_payload));
        let counter_offset = if write { 4 } else { 0 };
        let counter = p.value(Expression::Load(types::I32, counts, counter_offset));
        let maximum = p.value(Expression::Constant(types::I32, i64::from(u32::MAX)));
        p.guard(IntCC::UnsignedLessThan, counter, maximum);
        let increment = p.value(Expression::Constant(types::I32, 1));
        let counter = p.binary(Opcode::Iadd, counter, increment);
        p.push(Step::Store(tag, destination, 0));
        p.push(Step::Store(bits, destination, 8));
        if write {
            p.push(Step::Store(one, cell, 24));
        }
        p.push(Step::Store(counter, counts, counter_offset));
        let completed = p.value(Expression::Constant(
            types::I32,
            i64::from(abi::HELPER_COMPLETED),
        ));
        p.push(Step::Return(completed));
        Ok(p)
    }

    fn emit(&self, builder: &mut FunctionBuilder<'_>) {
        let entry = builder.create_block();
        let declined = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let mut values = [None; VALUES];
        values[0] = Some(builder.block_params(entry)[0]);
        for &step in &self.steps[..self.length] {
            let get = |id: u8| values[usize::from(id)].unwrap();
            match step {
                Step::Value(id, expression) => {
                    let result = match expression {
                        Expression::Constant(ty, bits) => builder.ins().iconst(ty, bits),
                        Expression::Load(ty, base, offset) => {
                            builder
                                .ins()
                                .load(ty, MemFlagsData::new(), get(base), offset)
                        }
                        Expression::Binary(opcode, a, b) => match opcode {
                            Opcode::Iadd => builder.ins().iadd(get(a), get(b)),
                            Opcode::Ishl => builder.ins().ishl(get(a), get(b)),
                            Opcode::Band => builder.ins().band(get(a), get(b)),
                            Opcode::Bor => builder.ins().bor(get(a), get(b)),
                            _ => unreachable!(),
                        },
                        Expression::Compare(condition, a, b) => {
                            builder.ins().icmp(condition, get(a), get(b))
                        }
                        Expression::Extend(a) => builder.ins().uextend(types::I64, get(a)),
                        Expression::Select(condition, a, b) => {
                            builder.ins().select(get(condition), get(a), get(b))
                        }
                    };
                    values[usize::from(id)] = Some(result);
                }
                Step::Guard(condition) => {
                    let next = builder.create_block();
                    builder.ins().brif(get(condition), next, &[], declined, &[]);
                    builder.switch_to_block(next);
                }
                Step::Store(value, base, offset) => {
                    builder
                        .ins()
                        .store(MemFlagsData::new(), get(value), get(base), offset);
                }
                Step::Return(value) => {
                    builder.ins().return_(&[get(value)]);
                }
            }
        }
        builder.switch_to_block(declined);
        let declined = builder
            .ins()
            .iconst(types::I32, i64::from(abi::HELPER_DECLINED));
        builder.ins().return_(&[declined]);
    }

    fn verify(&self, function: &Function) -> Result<(), JitError> {
        let entry = function.layout.entry_block().ok_or_else(invalid)?;
        let declined = function.layout.blocks().last().ok_or_else(invalid)?;
        if function.signature.params.len() != 1
            || function.signature.params[0].value_type != types::I64
            || function.signature.returns.len() != 1
            || function.signature.returns[0].value_type != types::I32
            || function.dfg.block_params(entry).len() != 1
            || function.dfg.value_type(function.dfg.block_params(entry)[0]) != types::I64
            || function
                .layout
                .blocks()
                .skip(1)
                .any(|block| !function.dfg.block_params(block).is_empty())
        {
            return Err(invalid());
        }
        let mut instructions = function
            .layout
            .blocks()
            .flat_map(|block| function.layout.block_insts(block));
        let mut values = [None; VALUES];
        values[0] = Some(function.dfg.block_params(entry)[0]);
        let mut block = entry;
        for &step in &self.steps[..self.length] {
            let inst = instructions.next().ok_or_else(invalid)?;
            if function.layout.inst_block(inst) != Some(block) {
                return Err(invalid());
            }
            let get = |id: u8| values[usize::from(id)].unwrap();
            let data = function.dfg.insts[inst];
            let args = function.dfg.inst_args(inst);
            match step {
                Step::Value(id, expression) => {
                    let (valid, expected_type) = match expression {
                        Expression::Constant(ty, bits) => (
                            matches!(data, InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if if ty == types::I32 { imm.bits() as u32 == bits as u32 } else { imm.bits() == bits }),
                            ty,
                        ),
                        Expression::Load(ty, base, offset) => (
                            matches!(data, InstructionData::Load { opcode: Opcode::Load, arg, offset: actual, flags } if arg == get(base) && i32::from(actual) == offset && function.dfg.mem_flags[flags] == MemFlagsData::new()),
                            ty,
                        ),
                        Expression::Binary(opcode, a, b) => (
                            data.opcode() == opcode && args == [get(a), get(b)],
                            function.dfg.value_type(get(a)),
                        ),
                        Expression::Compare(condition, a, b) => (
                            matches!(data, InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args } if cond == condition && args == [get(a), get(b)]),
                            types::I8,
                        ),
                        Expression::Extend(a) => (
                            data.opcode() == Opcode::Uextend && args == [get(a)],
                            types::I64,
                        ),
                        Expression::Select(condition, a, b) => (
                            data.opcode() == Opcode::Select
                                && args == [get(condition), get(a), get(b)],
                            function.dfg.value_type(get(a)),
                        ),
                    };
                    let [result] = function.dfg.inst_results(inst) else {
                        return Err(invalid());
                    };
                    if !valid || function.dfg.value_type(*result) != expected_type {
                        return Err(invalid());
                    }
                    values[usize::from(id)] = Some(*result);
                }
                Step::Guard(condition) => {
                    let InstructionData::Brif {
                        opcode: Opcode::Brif,
                        arg,
                        blocks,
                    } = data
                    else {
                        return Err(invalid());
                    };
                    let next = function.layout.next_block(block).ok_or_else(invalid)?;
                    if arg != get(condition)
                        || next == declined
                        || blocks[0].block(&function.dfg.value_lists) != next
                        || blocks[1].block(&function.dfg.value_lists) != declined
                        || blocks
                            .iter()
                            .any(|branch| branch.args(&function.dfg.value_lists).len() != 0)
                    {
                        return Err(invalid());
                    }
                    block = next;
                }
                Step::Store(value, base, offset) => {
                    if !matches!(data, InstructionData::Store { opcode: Opcode::Store, args, offset: actual, flags } if args == [get(value), get(base)] && i32::from(actual) == offset && function.dfg.mem_flags[flags] == MemFlagsData::new())
                    {
                        return Err(invalid());
                    }
                }
                Step::Return(value) => {
                    if data.opcode() != Opcode::Return
                        || args != [get(value)]
                        || !function.dfg.inst_results(inst).is_empty()
                    {
                        return Err(invalid());
                    }
                }
            }
        }
        let constant = instructions.next().ok_or_else(invalid)?;
        let returns = instructions.next().ok_or_else(invalid)?;
        if function.layout.next_block(block) != Some(declined)
            || function.layout.inst_block(constant) != Some(declined)
            || function.layout.inst_block(returns) != Some(declined)
            || !matches!(function.dfg.insts[constant], InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if imm.bits() == i64::from(abi::HELPER_DECLINED))
            || function.dfg.inst_results(constant).len() != 1
            || function
                .dfg
                .value_type(function.dfg.inst_results(constant)[0])
                != types::I32
            || function.dfg.insts[returns].opcode() != Opcode::Return
            || function.dfg.inst_args(returns) != function.dfg.inst_results(constant)
            || instructions.next().is_some()
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RegisterIndex as R, UpValueIndex as U};
    use cranelift_codegen::{
        ir::{AbiParam, Signature},
        settings,
    };
    use cranelift_frontend::FunctionBuilderContext;

    fn fixture(operation: Operation) -> Function {
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        let mut signature = Signature::new(isa.default_call_conv());
        signature.params.push(AbiParam::new(types::I64));
        signature.returns.push(AbiParam::new(types::I32));
        let mut function = Function::new();
        function.signature = signature;
        let mut context = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            Program::new(operation).unwrap().emit(&mut builder);
            builder.seal_all_blocks();
            builder.finalize(isa.frontend_config());
        }
        cranelift_codegen::verify_function(&function, isa.flags()).unwrap();
        function
    }

    #[test]
    fn source_bound_get_and_set_ir_cover_index_boundaries() {
        for index in [0, 1, 127, 255] {
            for operation in [
                Operation::GetUpValue {
                    dest: R(index),
                    source: U(255 - index),
                },
                Operation::SetUpValue {
                    dest: U(index),
                    source: R(255 - index),
                },
            ] {
                let program = Program::new(operation).unwrap();
                let function = fixture(operation);
                program.verify(&function).unwrap();
                assert!(program.length < STEPS);
                assert!(usize::from(program.values) < VALUES);
                assert!(function.layout.blocks().count() < 32);
                assert!(program.length + 2 < 128);
            }
        }
    }

    #[test]
    fn every_emitted_instruction_rejects_an_operand_or_control_flow_mutation() {
        use cranelift_codegen::{
            cursor::{Cursor, FuncCursor},
            ir::ValueList,
        };
        for operation in [
            Operation::GetUpValue {
                dest: R(3),
                source: U(7),
            },
            Operation::SetUpValue {
                dest: U(7),
                source: R(3),
            },
        ] {
            let original = fixture(operation);
            let program = Program::new(operation).unwrap();
            for inst in original
                .layout
                .blocks()
                .flat_map(|block| original.layout.block_insts(block))
            {
                let mut corrupted = original.clone();
                match &mut corrupted.dfg.insts[inst] {
                    InstructionData::UnaryImm { imm, .. } => *imm = (imm.bits() ^ 1).into(),
                    InstructionData::Load { offset, .. }
                    | InstructionData::Store { offset, .. } => {
                        *offset = (i32::from(*offset) + 1).into()
                    }
                    InstructionData::Binary { opcode, .. } => {
                        *opcode = match *opcode {
                            Opcode::Iadd => Opcode::Isub,
                            Opcode::Ishl => Opcode::Ushr,
                            Opcode::Band => Opcode::Bor,
                            Opcode::Bor => Opcode::Band,
                            _ => panic!("unexpected binary opcode"),
                        };
                    }
                    InstructionData::IntCompare { cond, .. } => {
                        *cond = if *cond == IntCC::Equal {
                            IntCC::NotEqual
                        } else {
                            IntCC::Equal
                        };
                    }
                    InstructionData::Unary {
                        opcode: opcode @ Opcode::Uextend,
                        ..
                    } => *opcode = Opcode::Sextend,
                    InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args,
                    } => args.swap(1, 2),
                    InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                    InstructionData::MultiAry {
                        opcode: Opcode::Return,
                        ..
                    } => {
                        let value = {
                            let mut cursor = FuncCursor::new(&mut corrupted);
                            cursor.goto_inst(inst);
                            cursor.ins().iconst(types::I32, 99)
                        };
                        let list = ValueList::from_slice(&[value], &mut corrupted.dfg.value_lists);
                        let InstructionData::MultiAry { args, .. } = &mut corrupted.dfg.insts[inst]
                        else {
                            unreachable!();
                        };
                        *args = list;
                    }
                    _ => panic!("uncovered projection instruction"),
                }
                cranelift_codegen::verify_function(
                    &corrupted,
                    &settings::Flags::new(settings::builder()),
                )
                .unwrap();
                assert!(
                    program.verify(&corrupted).is_err(),
                    "accepted mutation at {inst}"
                );
            }
        }
    }

    #[test]
    fn mismatched_sources_and_extra_effects_do_not_pass_the_projection_grammar() {
        use cranelift_codegen::cursor::{Cursor, FuncCursor};
        let operation = Operation::GetUpValue {
            dest: R(3),
            source: U(7),
        };
        let mut function = fixture(operation);
        for wrong in [
            Operation::GetUpValue {
                dest: R(2),
                source: U(7),
            },
            Operation::GetUpValue {
                dest: R(3),
                source: U(6),
            },
            Operation::SetUpValue {
                dest: U(7),
                source: R(3),
            },
        ] {
            assert!(Program::new(wrong).unwrap().verify(&function).is_err());
        }
        assert!(Program::new(Operation::Move {
            dest: R(0),
            source: R(1)
        })
        .is_err());
        let pointer = function
            .dfg
            .block_params(function.layout.entry_block().unwrap())[0];
        let load = function
            .layout
            .blocks()
            .flat_map(|block| function.layout.block_insts(block))
            .find(|inst| function.dfg.insts[*inst].opcode() == Opcode::Load)
            .unwrap();
        let mut cursor = FuncCursor::new(&mut function);
        cursor.goto_inst(load);
        cursor.ins().store(MemFlagsData::new(), pointer, pointer, 0);
        cranelift_codegen::verify_function(&function, &settings::Flags::new(settings::builder()))
            .unwrap();
        assert!(Program::new(operation).unwrap().verify(&function).is_err());
    }

    #[cfg(not(miri))]
    #[test]
    fn generated_projection_code_preserves_aliases_payloads_fallback_and_refresh() {
        let get = Operation::GetUpValue {
            dest: R(0),
            source: U(1),
        };
        let set = Operation::SetUpValue {
            dest: U(0),
            source: R(0),
        };
        let get_function = fixture(get);
        let set_function = fixture(set);
        Program::new(get).unwrap().verify(&get_function).unwrap();
        Program::new(set).unwrap().verify(&set_function).unwrap();
        crate::jit::backend::projection_probe::with_projection_probe(get_function, |get_pointer| {
            crate::jit::backend::projection_probe::with_projection_probe(
                set_function,
                |set_pointer| {
                    type Entry = unsafe extern "C" fn(*mut std::ffi::c_void) -> u32;
                    let get = unsafe { std::mem::transmute::<*const u8, Entry>(get_pointer) };
                    let set = unsafe { std::mem::transmute::<*const u8, Entry>(set_pointer) };
                    let mut lua = crate::Lua::empty();
                    lua.enter(|ctx| {
                        let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
                        let mut canonical = [Value::Integer(42), Value::Integer(11)];
                        let mut pc = 0;
                        LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            |mut registers| {
                                let open = registers.projection_open_at(ctx, 1);
                                let mut scratch = [
                                    Slot::from_value(Value::Integer(42)),
                                    Slot::from_value(Value::Integer(11)),
                                ];
                                let mut projection =
                                    Projection::<2>::new(&registers, &[cell, cell], &scratch)
                                        .unwrap();
                                projection
                                    .with_native(&mut scratch, |session| {
                                        unsafe {
                                            assert_eq!(
                                                session.invoke_scalar(set),
                                                abi::HELPER_COMPLETED
                                            );
                                            assert_eq!(
                                                session.invoke_scalar(get),
                                                abi::HELPER_COMPLETED
                                            );
                                        }
                                        assert_eq!(session.slot(0).unwrap().bits, 42);
                                        session.flush(ctx, &mut registers).unwrap();
                                        assert!(matches!(
                                            cell.get(),
                                            UpValueState::Closed(Value::Integer(42))
                                        ));
                                        cell.set(&ctx, open.get());
                                        session.refresh(&registers, &[cell, cell]).unwrap();
                                        unsafe {
                                            assert_eq!(
                                                session.invoke_scalar(get),
                                                abi::HELPER_COMPLETED
                                            );
                                        }
                                        assert_eq!(session.slot(0).unwrap().bits, 11);
                                        for bits in [(-0.0f64).to_bits(), 0x7ff8_0000_0000_1234] {
                                            session
                                                .store(
                                                    0,
                                                    Slot {
                                                        tag: abi::NUMBER,
                                                        bits,
                                                    },
                                                )
                                                .unwrap();
                                            unsafe {
                                                assert_eq!(
                                                    session.invoke_scalar(set),
                                                    abi::HELPER_COMPLETED
                                                );
                                                assert_eq!(
                                                    session.invoke_scalar(get),
                                                    abi::HELPER_COMPLETED
                                                );
                                            }
                                            assert_eq!(session.slot(0).unwrap().bits, bits);
                                            assert_eq!(session.slot(1).unwrap().bits, bits);
                                            session.flush(ctx, &mut registers).unwrap();
                                            session.refresh(&registers, &[cell, cell]).unwrap();
                                        }
                                        session
                                            .store(
                                                1,
                                                Slot {
                                                    tag: abi::REFERENCE,
                                                    bits: 0,
                                                },
                                            )
                                            .unwrap();
                                        let counts = session.counts();
                                        unsafe {
                                            assert_eq!(
                                                session.invoke_scalar(get),
                                                abi::HELPER_DECLINED
                                            );
                                        }
                                        assert_eq!(session.counts(), counts);
                                        session
                                            .store(
                                                0,
                                                Slot {
                                                    tag: abi::BOOLEAN,
                                                    bits: 2,
                                                },
                                            )
                                            .unwrap();
                                        unsafe {
                                            assert_eq!(
                                                session.invoke_scalar(set),
                                                abi::HELPER_DECLINED
                                            );
                                        }
                                        assert_eq!(session.counts(), counts);
                                        assert_eq!(session.slot(1).unwrap().tag, abi::REFERENCE);
                                        session
                                            .store(1, Slot::from_value(registers.stack_frame[1]))
                                            .unwrap();
                                        session.flush(ctx, &mut registers).unwrap();
                                        assert_eq!(
                                            session.counts(),
                                            Counts {
                                                reads: 4,
                                                writes: 3
                                            }
                                        );
                                    })
                                    .unwrap();
                            },
                        );
                    });
                },
            )
            .unwrap();
        })
        .unwrap();
    }
}
