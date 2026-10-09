use super::{tests::assert_identical, tests::invoke, *};

#[test]
fn integer_reads_preserve_array_hash_bounds_and_numeric_aliases() {
    crate::Lua::empty().enter(|ctx| {
        let table = Table::from_parts(&ctx, RawTable::with_capacity(&ctx, 8, 0), None);
        let marker = Value::Table(Table::new(&ctx));
        for key in [i64::MIN, -1, 0, 1, 4, 8, 9, 256, 1 << 32, i64::MAX] {
            table.set_raw(&ctx, Value::Integer(key), marker).unwrap();
        }
        table
            .set_raw(&ctx, Value::Number(2.0), Value::Number(-0.0))
            .unwrap();
        table.set_raw(&ctx, Value::Integer(4), Value::Nil).unwrap();
        for key in [
            i64::MIN,
            -2,
            -1,
            0,
            1,
            2,
            3,
            4,
            7,
            8,
            9,
            255,
            256,
            1 << 32,
            i64::MAX,
        ] {
            let expected = if key == 2 {
                Value::Number(-0.0)
            } else if [i64::MIN, -1, 0, 1, 8, 9, 256, 1 << 32, i64::MAX].contains(&key) {
                marker
            } else {
                Value::Nil
            };
            assert_identical(table.get_raw(&ctx, Value::Integer(key)), expected);
        }
    });
}

#[test]
fn integer_reads_upgrade_live_weak_values_and_drop_dead_entries() {
    let mut lua = crate::Lua::empty();
    let (table, live) = lua.enter(|ctx| {
        let table = Table::from_parts(&ctx, RawTable::with_capacity(&ctx, 8, 0), None);
        let meta = Table::new(&ctx);
        meta.set_field(ctx, "__mode", "v");
        let live = Table::new(&ctx);
        table.set(ctx, 1, live).unwrap();
        table.set(ctx, 2, Table::new(&ctx)).unwrap();
        table.set_metatable(ctx, Some(meta));
        table.set(ctx, 3, 42).unwrap();
        assert_identical(table.get_raw(&ctx, Value::Integer(1)), Value::Table(live));
        assert!(matches!(
            table.get_raw(&ctx, Value::Integer(2)),
            Value::Table(_)
        ));
        (ctx.stash(table), ctx.stash(live))
    });
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        let table = ctx.fetch(&table);
        for key in [1, 2, 3, 4, i64::MAX] {
            let expected = match key {
                1 => Value::Table(ctx.fetch(&live)),
                3 => Value::Integer(42),
                _ => Value::Nil,
            };
            assert_identical(table.get_raw(&ctx, Value::Integer(key)), expected);
        }
    });
}

#[test]
fn integer_helper_reads_keep_pending_keys_aliases_and_miss_semantics() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let table = Table::from_parts(&ctx, RawTable::with_capacity(&ctx, 8, 0), None);
        let meta = Table::new(&ctx);
        meta.set_field(ctx, "__index", Table::new(&ctx));
        table.set_metatable(ctx, Some(meta));
        table.set(ctx, 1, 42).unwrap();
        table.set_readonly(&ctx, true);
        table.set_intercept_all_writes(&ctx, true);
        for key in [
            Value::Integer(1),
            Value::Integer(2),
            Value::Number(1.0),
            Value::Number(1.5),
        ] {
            for destination in [0, 1, 2] {
                let mut values = [Value::Table(table), Value::Integer(7), Value::Integer(99)];
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                    let mut slots = [
                        Slot::from_value(Value::Table(table)),
                        Slot::from_value(key),
                        Slot::from_value(Value::Integer(99)),
                    ];
                    let mut frame = Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: Counts::default(),
                        table_cache: Default::default(),
                        slot_count: slots.len(),
                        panic: None,
                        projection: None,
                    };
                    let expected = table.get_raw(&ctx, key);
                    let before = frame.registers.stack_frame[destination];
                    let before_slot = slots[destination];
                    let result = invoke::<{ abi::HELPER_GET_TABLE }>(
                        &mut frame,
                        &mut slots,
                        destination as u32,
                        0,
                        1,
                        7,
                    );
                    assert_eq!(frame.count.calls, 1);
                    assert_eq!(frame.count.table_writes, 0);
                    if expected.is_nil() {
                        assert_eq!(result, abi::HELPER_DECLINED);
                        assert_eq!(
                            (
                                frame.count.completed,
                                frame.count.declined,
                                frame.count.table_reads
                            ),
                            (0, 1, 0)
                        );
                        assert_identical(frame.registers.stack_frame[destination], before);
                        assert_eq!(
                            (slots[destination].tag, slots[destination].bits),
                            (before_slot.tag, before_slot.bits)
                        );
                        assert_eq!(*frame.registers.pc, 7);
                    } else {
                        assert_eq!(result, abi::HELPER_COMPLETED);
                        assert_eq!(
                            (
                                frame.count.completed,
                                frame.count.declined,
                                frame.count.table_reads
                            ),
                            (1, 0, 1)
                        );
                        assert_identical(frame.registers.stack_frame[destination], expected);
                        assert_eq!(*frame.registers.pc, 8);
                    }
                    assert!(frame.panic.is_none());
                });
            }
        }
    });
}
