use super::*;
use closed_table_tests::{identical, siblings};
use table_write_tests::{counts, scope};

#[test]
fn new_tables_refresh_only_the_destination_and_preserve_allocation_accounting() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return {}").unwrap();
        let old = crate::Table::new(&ctx);
        old.set_readonly(&ctx, true);
        let kinds = [
            Value::Nil,
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"old")),
            Value::Table(old),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for count in [1, 7, 256] {
            for dest in [0, count - 1] {
                for initial in kinds {
                    for (array, map) in [(0, 0), (1, 0), (0, 1), (4, 7), (17, 2)] {
                        let mut values: Vec<_> =
                            (0..count).map(|i| kinds[i % kinds.len()]).collect();
                        values[dest] = initial;
                        let before = values.clone();
                        let start = ctx.metrics().total_allocation();
                        let expected = crate::Table::from_parts(
                            &ctx,
                            crate::table::RawTable::with_capacity(&ctx, array, map),
                            None,
                        );
                        let allocated = ctx.metrics().total_allocation() - start;
                        let (c, pc, panic) =
                            scope(ctx, closure, &mut values, |host, slots| unsafe {
                                let saved: Vec<_> =
                                    (0..count).map(|i| slots.add(i).read()).collect();
                                let start = ctx.metrics().total_allocation();
                                assert_eq!(
                                    call::<{ abi::HELPER_NEW_TABLE }>(
                                        host,
                                        slots,
                                        dest as u32,
                                        array as u32,
                                        map as u32,
                                        17
                                    ),
                                    abi::HELPER_COMPLETED
                                );
                                assert_eq!(ctx.metrics().total_allocation() - start, allocated);
                                siblings(slots, &saved, Some(dest));
                                let destination = slots.add(dest).read();
                                assert_eq!(destination.tag, REFERENCE);
                                assert!(destination.pointer.is_null());
                            });
                        assert_eq!(counts(c), [1, 1, 0, 0, 0, 0, 0, 1]);
                        assert_eq!(pc, 18);
                        assert!(!panic);
                        let Value::Table(table) = values[dest] else {
                            panic!("missing new table")
                        };
                        assert_ne!(table, expected);
                        assert_ne!(table, old);
                        assert_eq!(table.length(&ctx), 0);
                        assert!(table.metatable().is_none());
                        assert!(!table.is_readonly());
                        assert!(!table.intercepts_all_writes());
                        for key in [
                            Value::Integer(1),
                            Value::Integer(19),
                            Value::String(ctx.intern(b"key")),
                        ] {
                            let before = ctx.metrics().total_allocation();
                            expected.set_raw(&ctx, key, Value::Integer(42)).unwrap();
                            let growth = ctx.metrics().total_allocation() - before;
                            let before = ctx.metrics().total_allocation();
                            table.set_raw(&ctx, key, Value::Integer(42)).unwrap();
                            assert_eq!(ctx.metrics().total_allocation() - before, growth);
                            identical(table.get_raw(&ctx, key), expected.get_raw(&ctx, key));
                        }
                        for (index, (actual, expected)) in
                            values.into_iter().zip(before).enumerate()
                        {
                            if index != dest {
                                identical(actual, expected);
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn new_tables_preserve_retirement_and_refresh_after_generic_helpers() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return {}").unwrap();
        let mut values = [Value::Integer(7), Value::Number(-0.0), Value::Boolean(true)];
        let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
            assert_eq!(
                materialize_terminal(host, slots, 0, INTEGER, 42),
                abi::HELPER_COMPLETED
            );
            assert_eq!(
                materialize_terminal(host, slots, 1, NUMBER, 7.5f64.to_bits()),
                abi::HELPER_COMPLETED
            );
            let saved: Vec<_> = (0..3).map(|i| slots.add(i).read()).collect();
            assert_eq!(
                call::<{ abi::HELPER_NEW_TABLE }>(host, slots, 0, 0, 0, 7),
                abi::HELPER_COMPLETED
            );
            siblings(slots, &saved, Some(0));
            assert_eq!(slots.add(1).read().tag, NIL);
            assert_eq!(
                call::<{ abi::HELPER_MOVE }>(host, slots, 2, 1, 0, 8),
                abi::HELPER_COMPLETED
            );
            assert_eq!(load(slots.add(1).read()).unwrap().bits, 7.5f64.to_bits());
            assert_eq!(load(slots.add(2).read()).unwrap().bits, 7.5f64.to_bits());
            let saved: Vec<_> = (0..3).map(|i| slots.add(i).read()).collect();
            assert_eq!(
                call::<{ abi::HELPER_NEW_TABLE }>(host, slots, 0, 2, 3, 9),
                abi::HELPER_COMPLETED
            );
            siblings(slots, &saved, Some(0));
        });
        assert_eq!(counts(c), [3, 3, 0, 0, 0, 0, 0, 2]);
        assert_eq!(pc, 10);
        assert!(!panic);
        assert!(matches!(values[0], Value::Table(_)));
        identical(values[1], Value::Number(7.5));
        identical(values[2], Value::Number(7.5));
    });
}

#[test]
fn new_table_bounds_keep_fallback_allocation_and_panic_semantics() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return {}").unwrap();
        for (count, dest) in [(0, 0), (3, 3), (3, u32::MAX)] {
            let mut values = vec![Value::Integer(7); count];
            let start = ctx.metrics().total_allocation();
            let _expected = crate::Table::from_parts(
                &ctx,
                crate::table::RawTable::with_capacity(&ctx, 2, 3),
                None,
            );
            let allocated = ctx.metrics().total_allocation() - start;
            let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
                let start = ctx.metrics().total_allocation();
                if count != 0 {
                    assert!(store(
                        slots.read(),
                        Slot {
                            tag: INTEGER,
                            bits: 41
                        }
                    ));
                }
                assert_eq!(
                    call::<{ abi::HELPER_NEW_TABLE }>(host, slots, dest, 2, 3, 42),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    call::<{ abi::HELPER_NEW_TABLE }>(host, slots, 0, 0, 0, 44),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    materialize(host, slots, 0, INTEGER, 99),
                    abi::HELPER_PANICKED
                );
                assert_eq!(ctx.metrics().total_allocation() - start, allocated);
            });
            assert_eq!(counts(c), [1, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(pc, 43);
            assert!(panic);
            if count != 0 {
                identical(values[0], Value::Integer(41));
            }
        }
    });
}

#[test]
fn new_table_constructor_unwind_preserves_destination_and_stops_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return {}").unwrap();
        for allocated in [false, true] {
            let mut values = [Value::Number(-0.0), Value::Integer(7)];
            let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
                assert!(store(
                    slots.add(1).read(),
                    Slot {
                        tag: INTEGER,
                        bits: 41
                    }
                ));
                let session = &mut *host.cast::<Session<'_, '_, '_, '_, '_>>();
                assert_eq!(
                    new_table_with(session, slots, 0, 42, || {
                        if allocated {
                            let _ = crate::Table::new(&ctx);
                        }
                        panic!("constructor unwind witness")
                    }),
                    Some(abi::HELPER_PANICKED)
                );
                assert_eq!(
                    call::<{ abi::HELPER_NEW_TABLE }>(host, slots, 0, 0, 0, 44),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    materialize(host, slots, 1, INTEGER, 99),
                    abi::HELPER_PANICKED
                );
            });
            assert_eq!(counts(c), [1, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(pc, 43);
            assert!(panic);
            identical(values[0], Value::Number(-0.0));
            identical(values[1], Value::Integer(41));
        }
    });
}

#[test]
fn new_table_destinations_are_visible_through_open_upvalue_aliases() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV.key").unwrap();
        for dest in 0..3 {
            let mut values = [
                Value::Integer(7),
                Value::Table(crate::Table::new(&ctx)),
                Value::Number(-0.0),
                Value::Boolean(true),
            ];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let cell =
                    registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(dest as u8));
                closure.set_upvalue(&ctx, 0, cell);
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 4,
                    panic: None,
                    projection: None,
                };
                with_session(&mut frame, |host, slots| unsafe {
                    let saved: Vec<_> = (0..4).map(|i| slots.add(i).read()).collect();
                    assert_eq!(
                        call::<{ abi::HELPER_NEW_TABLE }>(host, slots, dest, 2, 3, 7),
                        abi::HELPER_COMPLETED
                    );
                    siblings(slots, &saved, Some(dest as usize));
                    assert_eq!(
                        call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, 3, 0, 0, 8),
                        abi::HELPER_COMPLETED
                    );
                });
                assert_eq!(counts(frame.count), [2, 2, 0, 0, 0, 1, 0, 1]);
                assert!(frame.panic.is_none());
            });
            assert_eq!(pc, 9);
            assert!(matches!(values[dest as usize], Value::Table(_)));
            identical(values[dest as usize], values[3]);
        }
    });
}
