use super::*;
use crate::{
    jit::resources,
    types::{RegisterIndex as R, VarCount},
    Lua, Table,
};

pub(super) fn source() -> Snapshot {
    Snapshot {
        prepared_keys: None,
        operations: resources::owned(&[
            Operation::Add {
                dest: R(1),
                left: R(1).into(),
                right: R(2).into(),
            },
            Operation::NumericForLoop {
                base: R(3),
                jump: -2,
            },
            Operation::Return {
                start: R(1),
                count: VarCount::constant(1),
            },
        ]),
        constants: resources::owned(&[]),
        registers: 7,
        upvalues: 0,
        prototypes: 0,
    }
}

#[test]
fn plan_rejects_invalid_aliases_shapes_and_multiple_candidates() {
    let mut source = source();
    assert!(Plan::recognize(&source).is_some());
    for register in 3..7 {
        source.operations[0] = Operation::Add {
            dest: R(1),
            left: R(register).into(),
            right: R(2).into(),
        };
        assert!(Plan::recognize(&source).is_none());
    }
    source.operations[0] = Operation::Add {
        dest: R(1),
        left: R(1).into(),
        right: R(2).into(),
    };
    source.operations[1] = Operation::NumericForLoop {
        base: R(3),
        jump: -1,
    };
    assert!(Plan::recognize(&source).is_none());
    source.operations[1] = Operation::NumericForLoop {
        base: R(3),
        jump: -2,
    };
    source.registers = 6;
    assert!(Plan::recognize(&source).is_none());
    source.registers = 7;
    let extra = [source.operations[0], source.operations[1]];
    source.operations.extend(extra);
    assert!(Plan::recognize(&source).is_none());
}

#[test]
fn admission_reads_current_values_without_changing_references_or_scalar_bits() {
    Lua::empty().enter(|ctx| {
        let plan = Plan::recognize(&source()).unwrap();
        let table = Table::new(&ctx);
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Integer(17),
            Value::Number(-0.0),
            Value::Number(f64::NAN),
            Value::Table(table),
        ];
        for left in values {
            for right in values {
                let mut frame = [
                    Value::Number(-0.0),
                    left,
                    right,
                    Value::Integer(0),
                    Value::Integer(3),
                    Value::Integer(1),
                    Value::Table(table),
                ];
                let before = frame.map(|v| {
                    let s = abi::Slot::from_value(v);
                    (s.tag, s.bits)
                });
                let nonnumeric = !matches!(left, Value::Integer(_) | Value::Number(_))
                    || !matches!(right, Value::Integer(_) | Value::Number(_));
                for width in 0..frame.len() {
                    let old = match plan
                        .operands
                        .map(|operand| operand.numeric(&frame[..width]))
                    {
                        [Some(left), Some(right)] => !(left && right),
                        _ => false,
                    };
                    assert_eq!(plan.refuses(&frame[..width], 0, 64), old);
                }
                for budget in [0, 1, 2, 64, u32::MAX] {
                    for pc in [0, 1, 2, usize::MAX] {
                        assert_eq!(
                            plan.refuses(&frame, pc, budget),
                            budget != 0 && pc < 2 && nonnumeric
                        );
                    }
                }
                assert_eq!(
                    frame.map(|v| {
                        let s = abi::Slot::from_value(v);
                        (s.tag, s.bits)
                    }),
                    before
                );
                assert!(matches!(frame[6],Value::Table(value) if value==table));
                frame[1] = Value::Integer(1);
                frame[2] = Value::Number(1.5);
                assert!(!plan.refuses(&frame, 0, 64));
                assert!(!plan.refuses(&frame, 1, 64));
                assert!(!plan.refuses(&frame[..2], 0, 64));
            }
        }
    });
}

pub(super) fn accumulator(initial: abi::Slot) -> Snapshot {
    use crate::types::{ConstantIndex16 as C16, ConstantIndex8 as C};
    Snapshot {
        prepared_keys: None,
        operations: resources::owned(&[
            Operation::LoadConstant {
                dest: R(0),
                constant: C16(0),
            },
            Operation::LoadConstant {
                dest: R(1),
                constant: C16(1),
            },
            Operation::LoadConstant {
                dest: R(2),
                constant: C16(2),
            },
            Operation::LoadConstant {
                dest: R(3),
                constant: C16(1),
            },
            Operation::NumericForPrep {
                base: R(1),
                jump: 1,
            },
            Operation::Add {
                dest: R(0),
                left: R(0).into(),
                right: C(3).into(),
            },
            Operation::NumericForLoop {
                base: R(1),
                jump: -2,
            },
            Operation::Return {
                start: R(0),
                count: VarCount::constant(1),
            },
        ]),
        constants: resources::owned(&[
            initial,
            abi::Slot {
                tag: abi::INTEGER,
                bits: 1,
            },
            abi::Slot {
                tag: abi::INTEGER,
                bits: 4,
            },
            abi::Slot {
                tag: abi::NUMBER,
                bits: 0.5f64.to_bits(),
            },
        ]),
        registers: 5,
        upvalues: 0,
        prototypes: 0,
    }
}

#[test]
fn numeric_carry_profitability_keeps_all_arithmetic_orders_and_initial_tags() {
    use crate::types::ConstantIndex8 as C;
    for initial in [
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Number(-0.0),
        Value::Number(f64::NAN),
        Value::Number(f64::INFINITY),
        Value::Nil,
        Value::Boolean(true),
    ] {
        let mut source = accumulator(abi::Slot::from_value(initial));
        for reverse in [false, true] {
            for kind in 0..4 {
                let (left, right) = if reverse {
                    (C(3).into(), R(0).into())
                } else {
                    (R(0).into(), C(3).into())
                };
                source.operations[5] = match kind {
                    0 => Operation::Add {
                        dest: R(0),
                        left,
                        right,
                    },
                    1 => Operation::Sub {
                        dest: R(0),
                        left,
                        right,
                    },
                    2 => Operation::Mul {
                        dest: R(0),
                        left,
                        right,
                    },
                    _ => Operation::Div {
                        dest: R(0),
                        left,
                        right,
                    },
                };
                source.verify().unwrap();
                assert_eq!(
                    Plan::recognize(&source).is_none(),
                    matches!(initial, Value::Integer(_) | Value::Number(_))
                );
            }
        }
    }
}

#[test]
fn numeric_carry_profitability_rejects_unknown_writes_calls_and_control_flow() {
    use crate::types::ConstantIndex16 as C16;
    let initial = abi::Slot::from_value(Value::Number(0.0));
    for operation in [
        Operation::Move {
            dest: R(0),
            source: R(4),
        },
        Operation::LoadConstant {
            dest: R(0),
            constant: C16(4),
        },
        Operation::LoadBool {
            dest: R(0),
            value: true,
            skip_next: false,
        },
        Operation::LoadBool {
            dest: R(4),
            value: true,
            skip_next: true,
        },
        Operation::Jump {
            offset: 0,
            close_upvalues: crate::types::Opt254::none(),
        },
        Operation::NumericForPrep {
            base: R(1),
            jump: 4,
        },
    ] {
        let mut source = accumulator(initial);
        source.constants.push(abi::Slot::from_value(Value::Nil));
        source.operations[1] = operation;
        source.verify().unwrap();
        assert!(Plan::recognize(&source).is_some());
    }
    let mut source = accumulator(initial);
    source.operations[0] = Operation::Call {
        func: R(0),
        args: VarCount::constant(0),
        returns: VarCount::constant(1),
    };
    source.verify().unwrap();
    assert!(Plan::recognize(&source).is_some());
    let mut source = accumulator(initial);
    source.operations[4] = Operation::Move {
        dest: R(4),
        source: R(1),
    };
    source.verify().unwrap();
    assert!(Plan::recognize(&source).is_some());
    let mut source = accumulator(abi::Slot::from_value(Value::Nil));
    source.operations.insert(
        4,
        Operation::LoadConstant {
            dest: R(0),
            constant: C16(1),
        },
    );
    source.verify().unwrap();
    assert!(Plan::recognize(&source).is_none());
}

#[test]
fn loop_refusal_requires_checked_continuing_integer_controls() {
    let plan = Plan::recognize(&source()).unwrap();
    let controls = [-2, -1, 0, 1, 2, i64::MIN, i64::MAX];
    for index in controls {
        for limit in controls {
            for step in controls {
                let frame = [
                    Value::Nil,
                    Value::Nil,
                    Value::Nil,
                    Value::Integer(index),
                    Value::Integer(limit),
                    Value::Integer(step),
                    Value::Nil,
                ];
                let next = i128::from(index) + i128::from(step);
                let expected = step != 0
                    && (i128::from(i64::MIN)..=i128::from(i64::MAX)).contains(&next)
                    && if step > 0 {
                        next <= i128::from(limit)
                    } else {
                        next >= i128::from(limit)
                    };
                assert_eq!(plan.refuses(&frame, 1, 64), expected);
                assert!(plan.refuses(&frame, 0, 64));
            }
        }
    }
    for lane in 3..6 {
        for value in [Value::Nil, Value::Boolean(true), Value::Number(1.0)] {
            let mut frame = [
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Integer(0),
                Value::Integer(3),
                Value::Integer(1),
                Value::Nil,
            ];
            frame[lane] = value;
            assert!(!plan.refuses(&frame, 1, 64));
            assert!(plan.refuses(&frame, 0, 64));
        }
    }
}

#[test]
fn register_and_constant_operands_keep_numeric_classification_separate() {
    use crate::types::ConstantIndex8 as C;
    for layout in 0..4 {
        for a in [
            abi::Slot {
                tag: abi::NIL,
                bits: 0,
            },
            abi::Slot {
                tag: abi::NUMBER,
                bits: 0,
            },
        ] {
            for b in [
                abi::Slot {
                    tag: abi::BOOLEAN,
                    bits: 1,
                },
                abi::Slot {
                    tag: abi::INTEGER,
                    bits: 1,
                },
            ] {
                let mut source = source();
                source.constants = resources::owned(&[a, b]);
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
                for kind in 0..4 {
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
                    let frame = [
                        Value::Nil,
                        Value::Integer(1),
                        Value::Number(0.0),
                        Value::Integer(0),
                        Value::Integer(3),
                        Value::Integer(1),
                        Value::Nil,
                    ];
                    let numeric_a = layout & 1 == 0 || a.tag == abi::NUMBER;
                    let numeric_b = layout & 2 == 0 || b.tag == abi::INTEGER;
                    if let Some(plan) = Plan::recognize(&source) {
                        assert_eq!(plan.refuses(&frame, 0, 64), !(numeric_a && numeric_b));
                        assert_eq!(plan.refuses(&frame, 1, 64), !(numeric_a && numeric_b));
                    } else {
                        assert!(numeric_a && numeric_b);
                    }
                }
            }
        }
    }
}
