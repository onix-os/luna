use super::*;
use crate::{table::RawTable, Lua};
use std::panic::{catch_unwind, AssertUnwindSafe};

fn array<'gc>(ctx: Context<'gc>, values: &[Value<'gc>]) -> Table<'gc> {
    let mut raw = RawTable::with_capacity(&ctx, values.len(), 0);
    raw.array_mut()[..values.len()].copy_from_slice(values);
    Table::from_parts(&ctx, raw, None)
}

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::String(a), Value::String(b)) => assert!(ottavino_gc_arena::Gc::ptr_eq(
            a.into_inner(),
            b.into_inner()
        )),
        (a, b) => {
            let (a, b) = (Slot::from_value(a), Slot::from_value(b));
            assert_eq!((a.tag, a.bits), (b.tag, b.bits));
        }
    }
}

#[test]
fn reads_and_writes_preserve_scalar_bits_and_reference_cells() {
    Lua::empty().enter(|ctx| {
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            Value::String(crate::String::from_slice(&ctx, b"retained")),
            Value::Table(Table::new(&ctx)),
        ];
        let table = array(ctx, &values);
        let mut counts = Counts::default();
        with_window::<_, 16>(
            ctx,
            table,
            1,
            values.len(),
            Access::Write,
            &mut counts,
            |window| {
                assert!(table.into_inner().try_borrow().is_err());
                for (index, value) in values.iter().copied().enumerate() {
                    let actual = window.read(index as i64 + 1);
                    if index >= 8 {
                        assert!(actual.is_none());
                    } else {
                        let actual = actual.unwrap();
                        let expected = Slot::from_value(value);
                        assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                    }
                }
                for index in 0..8 {
                    assert!(window.write(index as i64 + 1, Slot::from_value(values[7 - index])));
                }
                for index in 0..8 {
                    let actual = window.read(index as i64 + 1).unwrap();
                    let expected = Slot::from_value(values[7 - index]);
                    assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                }
            },
        )
        .unwrap();
        assert_eq!(
            counts,
            Counts {
                reads: 16,
                writes: 8
            }
        );
        for (index, value) in values.iter().copied().enumerate() {
            identical(
                table.get_value(ctx, index as i64 + 1),
                if index < 8 { values[7 - index] } else { value },
            );
        }
    });
}

#[test]
fn dirty_endpoints_and_unwinding_commit_once_and_release_the_lease() {
    Lua::empty().enter(|ctx| {
        let table = array(ctx, &[Value::Integer(7); 66]);
        let mut counts = Counts::default();
        let result = catch_unwind(AssertUnwindSafe(|| {
            with_window::<_, 64>(ctx, table, 2, 64, Access::Write, &mut counts, |window| {
                assert!(window.write(2, Slot::from_value(Value::Integer(11))));
                assert!(window.write(65, Slot::from_value(Value::Number(-0.0))));
                assert!(window.write(2, Slot::from_value(Value::Integer(13))));
                assert_eq!(window.dirty, 1 | (1 << 63));
                std::panic::panic_any(17u64);
            })
            .unwrap();
        }));
        assert_eq!(*result.unwrap_err().downcast::<u64>().unwrap(), 17);
        assert_eq!(
            counts,
            Counts {
                reads: 0,
                writes: 3
            }
        );
        for key in 1..=66 {
            identical(
                table.get_value(ctx, key),
                match key {
                    2 => Value::Integer(13),
                    65 => Value::Number(-0.0),
                    _ => Value::Integer(7),
                },
            );
        }
        table.set(ctx, 2, 19).unwrap();
        identical(table.get_value(ctx, 2), Value::Integer(19));
    });
}

#[test]
fn readonly_windows_allow_reads_but_never_dirty_writes() {
    Lua::empty().enter(|ctx| {
        let table = array(ctx, &[Value::Integer(7); 4]);
        table.set_readonly(&ctx, true);
        let mut counts = Counts::default();
        with_window::<_, 4>(ctx, table, 1, 4, Access::Read, &mut counts, |window| {
            assert!(table.into_inner().try_borrow().is_ok());
            assert!(table.into_inner().try_borrow_mut(&ctx).is_err());
            assert_eq!(window.read(4).unwrap().bits, 7);
            assert!(!window.write(4, Slot::from_value(Value::Integer(9))));
            assert_eq!(window.dirty, 0);
        })
        .unwrap();
        assert_eq!(
            counts,
            Counts {
                reads: 1,
                writes: 0
            }
        );
        let result = with_window::<_, 4>(ctx, table, 1, 4, Access::Write, &mut counts, |_| {
            panic!("readonly admission")
        });
        assert_eq!(result, Err(Refusal::ReadOnly));
        identical(table.get_value(ctx, 4), Value::Integer(7));
    });
}

#[test]
fn invalid_keys_and_descriptors_decline_before_counts_or_writes() {
    Lua::empty().enter(|ctx| {
        let table = array(ctx, &[Value::Integer(7); 4]);
        let mut counts = Counts::default();
        with_window::<_, 4>(ctx, table, 2, 2, Access::Write, &mut counts, |window| {
            for key in [i64::MIN, -1, 0, 1, 4, i64::MAX] {
                assert!(window.read(key).is_none());
                assert!(!window.write(key, Slot::from_value(Value::Integer(19))));
            }
            for slot in [
                Slot {
                    tag: abi::NIL,
                    bits: 1,
                },
                Slot {
                    tag: abi::BOOLEAN,
                    bits: 2,
                },
                Slot {
                    tag: abi::REFERENCE,
                    bits: 0,
                },
                Slot {
                    tag: u64::MAX,
                    bits: 0,
                },
            ] {
                assert!(!window.write(2, slot));
            }
            assert_eq!(window.dirty, 0);
        })
        .unwrap();
        assert_eq!(counts, Counts::default());
        for key in 1..=4 {
            identical(table.get_value(ctx, key), Value::Integer(7));
        }
    });
}

#[test]
fn admission_refuses_capacity_bounds_observers_weakness_and_borrow_conflicts() {
    Lua::empty().enter(|ctx| {
        let table = array(ctx, &[Value::Integer(7); 4]);
        let mut counts = Counts::default();
        let refused = |result: Result<(), Refusal>, expected| assert_eq!(result, Err(expected));
        refused(
            with_window::<_, 0>(ctx, table, 1, 1, Access::Write, &mut counts, |_| panic!()),
            Refusal::Capacity,
        );
        refused(
            with_window::<_, 65>(ctx, table, 1, 1, Access::Write, &mut counts, |_| panic!()),
            Refusal::Capacity,
        );
        for length in [0, 5] {
            refused(
                with_window::<_, 4>(
                    ctx,
                    table,
                    1,
                    length,
                    Access::Read,
                    &mut counts,
                    |_| panic!(),
                ),
                Refusal::Capacity,
            );
        }
        for first in [i64::MIN, -1, 0, 5, i64::MAX] {
            refused(
                with_window::<_, 4>(
                    ctx,
                    table,
                    first,
                    1,
                    Access::Read,
                    &mut counts,
                    |_| panic!(),
                ),
                Refusal::Bounds,
            );
        }
        refused(
            with_window::<_, 4>(ctx, table, 4, 2, Access::Read, &mut counts, |_| panic!()),
            Refusal::Bounds,
        );
        let inner = table.into_inner();
        {
            let _shared = inner.borrow();
            refused(
                with_window::<_, 4>(ctx, table, 1, 1, Access::Write, &mut counts, |_| panic!()),
                Refusal::Borrowed,
            );
        }
        {
            let _exclusive = inner.borrow_mut(&ctx);
            for access in [Access::Read, Access::Write] {
                refused(
                    with_window::<_, 4>(ctx, table, 1, 1, access, &mut counts, |_| panic!()),
                    Refusal::Borrowed,
                );
            }
        }
        table.set_metatable(ctx, Some(Table::new(&ctx)));
        refused(
            with_window::<_, 4>(ctx, table, 1, 1, Access::Read, &mut counts, |_| panic!()),
            Refusal::Observed,
        );
        table.set_metatable(ctx, None);
        table.set_intercept_all_writes(&ctx, true);
        refused(
            with_window::<_, 4>(ctx, table, 1, 1, Access::Read, &mut counts, |_| panic!()),
            Refusal::Observed,
        );
        table.set_intercept_all_writes(&ctx, false);
        inner.borrow_mut(&ctx).raw_table.make_values_weak(&ctx);
        refused(
            with_window::<_, 4>(ctx, table, 1, 1, Access::Read, &mut counts, |_| panic!()),
            Refusal::Weak,
        );
        assert_eq!(counts, Counts::default());
    });
}

#[test]
fn aliased_receivers_and_fresh_mutation_cannot_reuse_a_stale_window() {
    Lua::empty().enter(|ctx| {
        let table = array(ctx, &[Value::Integer(7); 4]);
        let mut counts = Counts::default();
        with_window::<_, 4>(ctx, table, 1, 4, Access::Write, &mut counts, |window| {
            let mut nested = Counts::default();
            assert_eq!(
                with_window::<_, 4>(ctx, table, 1, 4, Access::Read, &mut nested, |_| ()),
                Err(Refusal::Borrowed)
            );
            assert!(window.write(1, Slot::from_value(Value::Integer(13))));
        })
        .unwrap();
        table.set(ctx, 1, 17).unwrap();
        with_window::<_, 4>(ctx, table, 1, 4, Access::Read, &mut counts, |window| {
            assert_eq!(window.read(1).unwrap().bits, 17);
        })
        .unwrap();
        table.set_readonly(&ctx, true);
        assert_eq!(
            with_window::<_, 4>(ctx, table, 1, 4, Access::Write, &mut counts, |_| ()),
            Err(Refusal::ReadOnly)
        );
        assert_eq!(
            counts,
            Counts {
                reads: 1,
                writes: 1
            }
        );
    });
}

#[test]
fn canonical_reference_roots_survive_collection_after_window_release() {
    let mut lua = Lua::empty();
    let table = lua.enter(|ctx| {
        let child = Table::new(&ctx);
        child.set(ctx, "marker", 99).unwrap();
        let table = array(ctx, &[Value::Table(child), Value::Integer(7)]);
        with_window::<_, 2>(
            ctx,
            table,
            1,
            2,
            Access::Write,
            &mut Counts::default(),
            |window| {
                assert!(window.read(1).is_none());
                assert!(window.write(2, Slot::from_value(Value::Number(-0.0))));
            },
        )
        .unwrap();
        ctx.stash(table)
    });
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        let table = ctx.fetch(&table);
        let Value::Table(child) = table.get_value(ctx, 1) else {
            panic!("reference lost");
        };
        identical(child.get_value(ctx, "marker"), Value::Integer(99));
        identical(table.get_value(ctx, 2), Value::Number(-0.0));
    });
}
