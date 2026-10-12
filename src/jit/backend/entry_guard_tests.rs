use crate::{
    jit::{abi::Slot, backend, exits::Kind, ir::Snapshot, resources},
    opcode::{Operation, RCIndex},
    types::{ConstantIndex8 as C, RegisterIndex as R, VarCount},
    Closure, Constant, Lua, String, Table, Thread, UserData, Value,
};

fn arithmetic(kind: usize, dest: R, left: RCIndex, right: RCIndex) -> Operation {
    match kind {
        0 => Operation::Add { dest, left, right },
        1 => Operation::Sub { dest, left, right },
        2 => Operation::Mul { dest, left, right },
        _ => Operation::Div { dest, left, right },
    }
}

#[test]
fn arithmetic_entry_guards_preserve_values_and_budget_priority() {
    Lua::empty().enter(|ctx| {
        let text = String::from_static(&ctx, b"17");
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Integer(0),
            Value::Number(-0.0),
            Value::Number(1.5),
            Value::Number(f64::NAN),
            Value::Number(f64::INFINITY),
            Value::Number(f64::NEG_INFINITY),
            Value::String(text),
            Value::Table(Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(Thread::new(ctx)),
            Value::UserData(UserData::new_static(&ctx, 7)),
        ];
        let constants = [
            [Constant::Integer(i64::MAX), Constant::Number(-0.0)],
            [Constant::Nil, Constant::Boolean(true)],
            [Constant::String(text), Constant::Number(f64::NAN)],
            [Constant::Number(f64::INFINITY), Constant::String(text)],
        ];
        let mut declined = 0;
        let mut admitted = 0;
        for kind in 0..4 {
            for layout in 0..4 {
                let left = if layout & 1 == 0 {
                    R(0).into()
                } else {
                    C(0).into()
                };
                let right = if layout & 2 == 0 {
                    R(1).into()
                } else {
                    C(1).into()
                };
                for dest in [R(0), R(1), R(2)] {
                    let op = arithmetic(kind, dest, left, right);
                    for constants in constants {
                        let snapshot = Snapshot {
                            prepared_keys: None,
                            operations: resources::owned(&[
                                op,
                                Operation::Return {
                                    start: dest,
                                    count: VarCount::constant(1),
                                },
                            ]),
                            constants: resources::owned(&constants.map(Slot::from_constant)),
                            registers: 3,
                            upvalues: 0,
                            prototypes: 0,
                        };
                        snapshot.verify().unwrap();
                        let memory =
                            resources::MappingCounter::new(resources::Ledger::new(usize::MAX));
                        let code = backend::compile(&snapshot, memory, 1024 * 1024).unwrap();
                        for a in values {
                            for b in values {
                                let registers = [a, b, Value::Integer(987)];
                                let operand = |index| match index {
                                    RCIndex::Register(index) => registers[index.0 as usize],
                                    RCIndex::Constant(index) => constants[index.0 as usize].into(),
                                };
                                let numeric =
                                    |value| matches!(value, Value::Integer(_) | Value::Number(_));
                                let denied = !numeric(operand(left)) || !numeric(operand(right));
                                let before = registers.map(Slot::from_value);
                                for budget in [0, 1, 64] {
                                    let mut slots = before;
                                    let exit = code.invoke(&mut slots, 0, budget);
                                    if budget == 0 || denied {
                                        assert_eq!((exit.pc, exit.instructions), (0, 0));
                                        assert_eq!(
                                            exit.reason,
                                            if budget == 0 {
                                                Kind::Budget
                                            } else {
                                                Kind::Guard
                                            } as u32
                                        );
                                        assert_eq!(
                                            slots.map(|s| (s.tag, s.bits)),
                                            before.map(|s| (s.tag, s.bits))
                                        );
                                    } else {
                                        assert_eq!((exit.pc, exit.instructions), (1, 1));
                                    }
                                }
                                if denied {
                                    declined += 1;
                                } else {
                                    admitted += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(declined > 0 && admitted > 0);
    });
}
