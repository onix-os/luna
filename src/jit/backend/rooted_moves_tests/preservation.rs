use super::*;
use crate::types::{ConstantIndex16, Opt254};

fn same_reference<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::String(a), Value::String(b)) => assert_eq!(a, b),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        _ => panic!("root reference kind changed"),
    }
}

fn operations(base: u8) -> Vec<Operation> {
    use Operation::*;
    let left = R(base).into();
    let right = R(base + 1).into();
    let mut result = vec![
        Move {
            dest: R(base),
            source: R(base + 1),
        },
        Move {
            dest: R(base + 1),
            source: R(base),
        },
        Move {
            dest: R(base),
            source: R(base),
        },
        LoadNil {
            dest: R(base),
            count: 4,
        },
        Not {
            dest: R(base),
            source: R(base + 1),
        },
        Jump {
            offset: -1,
            close_upvalues: Opt254::none(),
        },
        Jump {
            offset: 1,
            close_upvalues: Opt254::none(),
        },
        NumericForPrep {
            base: R(base),
            jump: 0,
        },
        NumericForPrep {
            base: R(base),
            jump: 1,
        },
        NumericForLoop {
            base: R(base),
            jump: -1,
        },
        NumericForLoop {
            base: R(base),
            jump: 1,
        },
        Call {
            func: R(base),
            args: VarCount::constant(1),
            returns: VarCount::constant(0),
        },
    ];
    for index in 0..5 {
        result.push(LoadConstant {
            dest: R(base),
            constant: ConstantIndex16(index),
        });
    }
    for value in [false, true] {
        for skip_next in [false, true] {
            result.push(LoadBool {
                dest: R(base),
                value,
                skip_next,
            });
        }
        result.extend([
            Test {
                value: R(base),
                is_true: value,
            },
            Eq {
                skip_if: value,
                left,
                right,
            },
            Less {
                skip_if: value,
                left,
                right,
            },
            LessEq {
                skip_if: value,
                left,
                right,
            },
        ]);
    }
    for dest in [R(base), R(base + 1), R(base + 3)] {
        result.extend([
            Add { dest, left, right },
            Sub { dest, left, right },
            Mul { dest, left, right },
            Div { dest, left, right },
        ]);
    }
    result
}

#[test]
fn rooted_transfers_and_scalar_operations_preserve_snapshots_across_exits() {
    let mut cases = 0usize;
    Lua::empty().enter(|ctx| {
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Integer(1),
            Value::Number(-0.0),
            Value::Number(0.5),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0x7ff8000000000055)),
            Value::Number(f64::from_bits(0x7ff0000000000055)),
            crate::String::from_static(&ctx, b"preserved").into(),
            Table::new(&ctx).into(),
            crate::Closure::load(ctx, None, b"return").unwrap().into(),
            crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            crate::Thread::new(ctx).into(),
            crate::UserData::new_static(&ctx, 42).into(),
        ];
        for width in [6usize, 17, 256] {
            let base = (width - 4) as u8;
            for op in operations(base) {
                let computed = match op {
                    Operation::Add { dest, .. }
                    | Operation::Sub { dest, .. }
                    | Operation::Mul { dest, .. }
                    | Operation::Div { dest, .. } => [Some(usize::from(dest.0)), None],
                    Operation::NumericForPrep { base, .. } => [Some(usize::from(base.0)), None],
                    Operation::NumericForLoop { base, .. } => {
                        [Some(usize::from(base.0)), Some(usize::from(base.0) + 3)]
                    }
                    _ => [None, None],
                };
                let source = Snapshot {
                    operations: owned(&[
                        op,
                        Operation::LoadNil { dest: R(base + 3), count: 1 },
                        Operation::Return { start: R(0), count: VarCount::constant(0) },
                    ]),
                    constants: owned(
                        &[values[0], values[1], values[3], values[6], values[9]]
                            .map(Slot::from_value),
                    ),
                    registers: width,
                    upvalues: 0,
                    prototypes: 0,
                };
                assert!(crate::jit::helper_flow::rooted_moves_admitted(&source));
                let code = compile_source(&source, Failure::None).unwrap();
                for (a, &left) in values.iter().enumerate() {
                    for (b, &right) in values.iter().enumerate() {
                        let mut canonical: Vec<_> = (0..width)
                            .map(|index| values[(index + a + b) % values.len()])
                            .collect();
                        canonical[usize::from(base)] = left;
                        canonical[usize::from(base) + 1] = right;
                        canonical[usize::from(base) + 2] =
                            Value::Integer(if (a + b) % 2 == 0 { 1 } else { -1 });
                        let mut roots = vec![Value::Nil; width];
                        let mut initial = vec![Slot::from_value(Value::Nil); width];
                        assert!(abi::roots::capture(&mut roots, &mut initial, &canonical));
                        for start in [0, 1, 2, 3, usize::MAX] {
                            for budget in [0, 1, 2, 3, 7, 64, 65, u32::MAX] {
                                let mut expected = initial.clone();
                                let reference = crate::jit::model::run_rooted(
                                    &source, &mut expected, start, budget,
                                );
                                let mut actual = initial.clone();
                                let mut file = abi::roots::call::preserved::Preserved::new(
                                    &mut roots, &mut actual,
                                ).unwrap();
                                let exit = unsafe {
                                    code.invoke_raw(file.native_slots(), start, budget, std::ptr::null_mut())
                                };
                                assert_eq!(
                                    (exit.pc, exit.instructions, exit.reason),
                                    (reference.pc, reference.instructions, reference.reason),
                                    "op={op:?} width={width} a={a} b={b} start={start} budget={budget}"
                                );
                                let mut published = vec![Value::Nil; width];
                                assert!(file.snapshot().publish(&mut published));
                                drop(file);
                                let snapshot = abi::roots::call::Snapshot::new(&roots, &mut actual)
                                    .expect("native rooted output must retain valid tags and roots");
                                let mut checked = vec![Value::Nil; width];
                                assert!(snapshot.publish(&mut checked));
                                for (actual, expected) in published.iter().zip(&checked) {
                                    let (a, b) = (Slot::from_value(*actual), Slot::from_value(*expected));
                                    assert_eq!((a.tag, a.bits), (b.tag, b.bits));
                                    if a.tag == abi::REFERENCE {
                                        same_reference(*actual, *expected);
                                    }
                                }
                                for (index, (actual, expected)) in
                                    actual.iter().zip(&expected).enumerate()
                                {
                                    assert_eq!(actual.tag, expected.tag);
                                    if actual.tag == abi::NUMBER
                                        && f64::from_bits(expected.bits).is_nan()
                                        && start == 0
                                        && exit.instructions != 0
                                        && computed.contains(&Some(index))
                                    {
                                        assert!(f64::from_bits(actual.bits).is_nan());
                                    } else {
                                        assert_eq!(actual.bits, expected.bits);
                                    }
                                    if actual.tag == abi::REFERENCE {
                                        same_reference(published[index], roots[expected.bits as usize]);
                                    }
                                }
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    });
    assert_eq!(cases, 1_421_880);
}
