use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

fn storage<'gc>(ctx: Context<'gc>, length: usize) -> Gc<'gc, RefLock<StackVec<'gc>>> {
    let mut values = StackVec::new_in(MetricsAlloc::new(&ctx));
    values.extend((0..length).map(|i| Value::Integer(i as i64)));
    Gc::new(&ctx, RefLock::new(values))
}

fn integers(values: Gc<'_, RefLock<StackVec<'_>>>) -> std::vec::Vec<i64> {
    values
        .borrow()
        .iter()
        .map(|value| match value {
            Value::Integer(value) => *value,
            _ => panic!("expected integer"),
        })
        .collect()
}

#[test]
fn overflowing_get_cannot_read_the_prefix() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in 1..=4 {
            let values = storage(ctx, 6);
            let stack = Stack::new(ctx, values, bottom);
            for index in [usize::MAX, usize::MAX - bottom + 1] {
                assert!(stack.get(index).is_nil(), "bottom={bottom} index={index}");
            }
            assert_eq!(integers(values), (0..6).collect::<std::vec::Vec<_>>());
        }
    });
}

#[test]
fn overflowing_set_cannot_write_the_prefix() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in 1..=4 {
            for index in [usize::MAX, usize::MAX - bottom + 1] {
                let values = storage(ctx, 6);
                let mut stack = Stack::new(ctx, values, bottom);
                assert!(catch_unwind(AssertUnwindSafe(|| {
                    stack.set(index, Value::Nil);
                }))
                .is_err());
                assert_eq!(integers(values), (0..6).collect::<std::vec::Vec<_>>());
            }
        }
    });
}

#[test]
fn overflowing_remove_cannot_remove_the_prefix() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in 1..=4 {
            let values = storage(ctx, 6);
            let mut stack = Stack::new(ctx, values, bottom);
            for index in [usize::MAX, usize::MAX - bottom + 1] {
                assert!(stack.remove(index).is_none());
            }
            assert_eq!(integers(values), (0..6).collect::<std::vec::Vec<_>>());
        }
    });
}

#[test]
fn overflowing_substack_cannot_rebase_into_the_prefix() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in 1..=4 {
            let values = storage(ctx, 6);
            let mut stack = Stack::new(ctx, values, bottom);
            assert!(catch_unwind(AssertUnwindSafe(|| {
                stack.sub_stack(usize::MAX - bottom + 1).clear();
            }))
            .is_err());
            assert_eq!(integers(values), (0..6).collect::<std::vec::Vec<_>>());
        }
    });
}

#[test]
fn overflowing_resize_cannot_truncate_the_prefix() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in 1..=4 {
            let values = storage(ctx, 6);
            let mut stack = Stack::new(ctx, values, bottom);
            assert!(catch_unwind(AssertUnwindSafe(|| {
                stack.resize(usize::MAX - bottom + 1);
            }))
            .is_err());
            assert_eq!(integers(values), (0..6).collect::<std::vec::Vec<_>>());
        }
    });
}

#[test]
fn ordinary_indexed_operations_match_a_suffix_vector() {
    crate::Lua::empty().enter(|ctx| {
        for length in 0..=6 {
            for bottom in 0..=length {
                for index in 0..=length + 2 {
                    let values = storage(ctx, length);
                    let mut stack = Stack::new(ctx, values, bottom);
                    let mut expected = (0..length as i64).collect::<std::vec::Vec<_>>();
                    let valid = index < length - bottom;
                    match stack.get(index) {
                        Value::Integer(value) if valid => {
                            assert_eq!(value, expected[bottom + index]);
                        }
                        Value::Nil if !valid => {}
                        value => panic!("unexpected value {value:?}"),
                    }
                    assert_eq!(
                        catch_unwind(AssertUnwindSafe(|| {
                            stack.set(index, Value::Integer(42));
                        }))
                        .is_ok(),
                        valid,
                    );
                    if valid {
                        expected[bottom + index] = 42;
                    }
                    assert_eq!(integers(values), expected);
                    assert_eq!(stack.remove(index).is_some(), valid);
                    if valid {
                        expected.remove(bottom + index);
                    }
                    assert_eq!(integers(values), expected);
                }
                for offset in 0..=length - bottom {
                    let values = storage(ctx, length);
                    let mut stack = Stack::new(ctx, values, bottom);
                    stack.sub_stack(offset).resize(0);
                    assert_eq!(
                        integers(values),
                        (0..(bottom + offset) as i64).collect::<std::vec::Vec<_>>()
                    );
                }
            }
        }
    });
}

#[test]
fn all_extreme_drain_bounds_match_a_wide_integer_oracle() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in 0..=6 {
            let bounds = [
                Bound::Unbounded,
                Bound::Included(0),
                Bound::Excluded(0),
                Bound::Included(1),
                Bound::Excluded(1),
                Bound::Included(6 - bottom),
                Bound::Excluded(6 - bottom),
                Bound::Included(usize::MAX),
                Bound::Excluded(usize::MAX),
                Bound::Included(usize::MAX - bottom),
                Bound::Excluded(usize::MAX - bottom),
            ];
            for start in bounds {
                for end in bounds {
                    let values = storage(ctx, 6);
                    let mut stack = Stack::new(ctx, values, bottom);
                    let from = match start {
                        Bound::Unbounded => 0,
                        Bound::Included(i) => i as u128,
                        Bound::Excluded(i) => i as u128 + 1,
                    };
                    let to = match end {
                        Bound::Unbounded => (6 - bottom) as u128,
                        Bound::Included(i) => i as u128 + 1,
                        Bound::Excluded(i) => i as u128,
                    };
                    let valid = from <= to && to <= (6 - bottom) as u128;
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        drop(stack.drain((start, end)));
                    }));
                    assert_eq!(result.is_ok(), valid, "bottom={bottom} {start:?} {end:?}");
                    let mut expected = (0..6).collect::<std::vec::Vec<i64>>();
                    if valid {
                        expected.drain(bottom + from as usize..bottom + to as usize);
                    }
                    assert_eq!(integers(values), expected);
                }
            }
        }
    });
}
