use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

fn storage<'gc>(ctx: Context<'gc>, length: usize) -> Gc<'gc, RefLock<StackVec<'gc>>> {
    let mut values = StackVec::new_in(MetricsAlloc::new(&ctx));
    values.extend((0..length).map(|index| Value::Integer(index as i64)));
    Gc::new(&ctx, RefLock::new(values))
}

fn integer(value: Value<'_>) -> usize {
    let Value::Integer(value) = value else {
        panic!("expected integer")
    };
    value.try_into().unwrap()
}

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::String(a), Value::String(b)) => assert!(Gc::ptr_eq(a.into_inner(), b.into_inner())),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        (a, b) => panic!("value mismatch: {a:?} != {b:?}"),
    }
}

#[test]
fn yielded_and_retained_values_preserve_bits_and_reference_identity() {
    crate::Lua::empty().enter(|ctx| {
        let fixtures = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            Value::String(crate::String::from_slice(&ctx, b"root")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(
                crate::Closure::load(ctx, None, b"return 42")
                    .unwrap()
                    .into(),
            ),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 42i64)),
        ];
        for value in fixtures {
            for end in [1, 2] {
                let values = storage(ctx, 3);
                values.borrow_mut(&ctx).fill(value);
                let mut stack = Stack::new(ctx, values, 1);
                {
                    let mut drain = stack.drain(..end);
                    identical(drain.next().unwrap(), value);
                }
                assert_eq!(values.borrow().len(), 3 - end);
                for retained in values.borrow().iter().copied() {
                    identical(retained, value);
                }
            }
        }
    });
}

#[test]
fn every_small_range_matches_vec_drain_in_both_directions() {
    crate::Lua::empty().enter(|ctx| {
        for length in 0..=6 {
            for bottom in 0..=length {
                for start in 0..=length - bottom {
                    for end in start..=length - bottom {
                        for mode in 0..6 {
                            let values = storage(ctx, length);
                            let capacity = values.borrow().capacity();
                            let mut stack = Stack::new(ctx, values, bottom);
                            let mut model = (0..length).collect::<std::vec::Vec<_>>();
                            {
                                let mut actual = stack.drain(start..end);
                                let mut expected = model.drain(bottom + start..bottom + end);
                                assert!(values.try_borrow().is_err());
                                let reads = match mode {
                                    0 => 0,
                                    4 | 5 => 1,
                                    _ => end - start + 2,
                                };
                                for index in 0..reads {
                                    assert_eq!(actual.len(), expected.len());
                                    assert_eq!(actual.size_hint(), expected.size_hint());
                                    let back =
                                        mode == 2 || mode == 5 || (mode == 3 && index % 2 == 1);
                                    let a = if back {
                                        actual.next_back()
                                    } else {
                                        actual.next()
                                    };
                                    let b = if back {
                                        expected.next_back()
                                    } else {
                                        expected.next()
                                    };
                                    assert_eq!(a.map(integer), b);
                                }
                            }
                            assert_eq!(values.borrow().capacity(), capacity);
                            let actual = values
                                .borrow()
                                .iter()
                                .copied()
                                .map(integer)
                                .collect::<std::vec::Vec<_>>();
                            assert_eq!(
                                actual, model,
                                "length={length} bottom={bottom} range={start}..{end} mode={mode}"
                            );
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn panicking_with_a_partial_drain_removes_the_whole_range() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in [0, 2] {
            for (start, end) in [(0, 0), (0, 2), (1, 3), (2, 4), (4, 4)] {
                for back in [false, true] {
                    let values = storage(ctx, bottom + 4);
                    let mut stack = Stack::new(ctx, values, bottom);
                    let panic = catch_unwind(AssertUnwindSafe(|| {
                        let mut drain = stack.drain(start..end);
                        if back {
                            drain.next_back();
                        } else {
                            drain.next();
                        }
                        std::panic::panic_any(0x5678_u64);
                    }))
                    .unwrap_err();
                    assert_eq!(*panic.downcast::<u64>().unwrap(), 0x5678);
                    let expected = (0..bottom + start)
                        .chain(bottom + end..bottom + 4)
                        .collect::<std::vec::Vec<_>>();
                    let actual = values
                        .borrow()
                        .iter()
                        .copied()
                        .map(integer)
                        .collect::<std::vec::Vec<_>>();
                    assert_eq!(actual, expected);
                    stack.push_back(Value::Integer(99));
                    assert_eq!(stack.pop_back().map(integer), Some(99));
                }
            }
        }
    });
}

#[test]
fn bound_variants_preserve_prefix_and_invalid_ranges_do_not_mutate() {
    crate::Lua::empty().enter(|ctx| {
        let bounds = [
            (Bound::Unbounded, Bound::Unbounded),
            (Bound::Included(1), Bound::Included(2)),
            (Bound::Excluded(0), Bound::Excluded(3)),
            (Bound::Included(4), Bound::Unbounded),
            (Bound::Included(3), Bound::Excluded(1)),
            (Bound::Unbounded, Bound::Included(4)),
        ];
        for (start, end) in bounds {
            let values = storage(ctx, 6);
            let mut stack = Stack::new(ctx, values, 2);
            let from = match start {
                Bound::Unbounded => 0,
                Bound::Included(n) => n,
                Bound::Excluded(n) => n + 1,
            };
            let to = match end {
                Bound::Unbounded => 4,
                Bound::Included(n) => n + 1,
                Bound::Excluded(n) => n,
            };
            let valid = from <= to && to <= 4;
            let result = catch_unwind(AssertUnwindSafe(|| {
                drop(stack.drain((start, end)));
            }));
            assert_eq!(result.is_ok(), valid);
            let expected = if valid {
                (0..2 + from).chain(2 + to..6).collect::<std::vec::Vec<_>>()
            } else {
                (0..6).collect()
            };
            let actual = values
                .borrow()
                .iter()
                .copied()
                .map(integer)
                .collect::<std::vec::Vec<_>>();
            assert_eq!(actual, expected);
        }
    });
}
