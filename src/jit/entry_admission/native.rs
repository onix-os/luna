use super::*;
use crate::{
    jit::{backend, exits::Kind, resources},
    Lua, Table, Value,
};

#[test]
fn every_refusal_matches_actual_native_zero_or_one_instruction_exit() {
    Lua::empty().enter(|ctx| {
        let source = super::tests::source();
        let plan = Plan::recognize(&source).unwrap();
        let memory = resources::MappingCounter::new(resources::Ledger::new(4 * 1024 * 1024));
        let code = backend::compile(&source, memory, 4 * 1024 * 1024).unwrap();
        let table = Table::new(&ctx);
        let values = [
            Value::Nil,
            Value::Integer(1),
            Value::Number(f64::NAN),
            Value::Table(table),
        ];
        let mut refused = 0;
        let mut accepted = 0;
        for left in values {
            for right in values {
                for index in [-1, 0, 1, i64::MIN, i64::MAX] {
                    for step in [-1, 0, 1, i64::MIN, i64::MAX] {
                        let frame = [
                            Value::Nil,
                            left,
                            right,
                            Value::Integer(index),
                            Value::Integer(3),
                            Value::Integer(step),
                            Value::Table(table),
                        ];
                        for pc in [0, 1] {
                            for budget in [0, 1, 2, 64] {
                                if plan.refuses(&frame, pc, budget) {
                                    let mut slots = frame.map(abi::Slot::from_value);
                                    let exit = code.invoke(&mut slots, pc, budget);
                                    assert_eq!((exit.pc, exit.instructions), (0, pc as u32));
                                    assert_eq!(
                                        exit.reason,
                                        if pc == 1 && budget == 1 {
                                            Kind::Budget
                                        } else {
                                            Kind::Guard
                                        } as u32
                                    );
                                    refused += 1;
                                } else {
                                    accepted += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(refused > 0 && accepted > 0);
    });
}

#[test]
fn every_arithmetic_and_constant_layout_matches_native_guard_priority() {
    use crate::types::{ConstantIndex8 as C, RegisterIndex as R};
    for kind in 0..4 {
        for layout in 0..4 {
            for constants in [
                [
                    abi::Slot {
                        tag: abi::NIL,
                        bits: 0,
                    },
                    abi::Slot {
                        tag: abi::INTEGER,
                        bits: 1,
                    },
                ],
                [
                    abi::Slot {
                        tag: abi::NUMBER,
                        bits: 1.0f64.to_bits(),
                    },
                    abi::Slot {
                        tag: abi::BOOLEAN,
                        bits: 1,
                    },
                ],
                [
                    abi::Slot {
                        tag: abi::INTEGER,
                        bits: 1,
                    },
                    abi::Slot {
                        tag: abi::NUMBER,
                        bits: 1.0f64.to_bits(),
                    },
                ],
            ] {
                let mut source = super::tests::source();
                source.constants = resources::owned(&constants);
                let left = if layout & 1 == 0 {
                    R(1).into()
                } else {
                    C(0).into()
                };
                let right = if layout & 2 == 0 {
                    R(2).into()
                } else {
                    C(1).into()
                };
                source.operations[0] = match kind {
                    0 => Operation::Add {
                        dest: R(1),
                        left,
                        right,
                    },
                    1 => Operation::Sub {
                        dest: R(1),
                        left,
                        right,
                    },
                    2 => Operation::Mul {
                        dest: R(1),
                        left,
                        right,
                    },
                    _ => Operation::Div {
                        dest: R(1),
                        left,
                        right,
                    },
                };
                let Some(plan) = Plan::recognize(&source) else {
                    continue;
                };
                let memory =
                    resources::MappingCounter::new(resources::Ledger::new(4 * 1024 * 1024));
                let code = backend::compile(&source, memory, 4 * 1024 * 1024).unwrap();
                let frame = [
                    Value::Nil,
                    Value::Nil,
                    Value::Nil,
                    Value::Integer(0),
                    Value::Integer(3),
                    Value::Integer(1),
                    Value::Nil,
                ];
                for pc in [0, 1] {
                    for budget in [1, 2, 64] {
                        assert!(plan.refuses(&frame, pc, budget));
                        let mut slots = frame.map(abi::Slot::from_value);
                        let exit = code.invoke(&mut slots, pc, budget);
                        assert_eq!((exit.pc, exit.instructions), (0, pc as u32));
                        assert_eq!(
                            exit.reason,
                            if pc == 1 && budget == 1 {
                                Kind::Budget
                            } else {
                                Kind::Guard
                            } as u32
                        );
                    }
                }
            }
        }
    }
}
