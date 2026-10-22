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
