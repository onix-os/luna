use super::*;

fn key(closure: crate::Closure<'_>) -> u32 {
    abi::CONSTANT_OPERAND
        | closure
            .prototype()
            .constants
            .iter()
            .position(|v| matches!(v, crate::Constant::String(s) if s.as_bytes() == b"key"))
            .unwrap() as u32
}

#[test]
fn local_table_destination_updates_current_frame_upvalue_aliases() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV.key").unwrap();
        let table = crate::Table::new(&ctx);
        for value in [Value::Integer(42), Value::Table(crate::Table::new(&ctx))] {
            table
                .set_raw(&ctx, Value::String(ctx.intern(b"key")), value)
                .unwrap();
            let mut values = [
                Value::Nil,
                Value::Table(table),
                Value::Boolean(true),
                Value::Number(-0.0),
            ];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let cell = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
                closure.set_upvalue(&ctx, 0, cell);
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 4,
                    panic: None,
                    read_hints: None,
                    projection: None,
                };
                with_session(&mut frame, |host, slots| unsafe {
                    let saved: Vec<_> = (0..4).map(|i| slots.add(i).read()).collect();
                    assert_eq!(
                        call::<{ abi::HELPER_GET_TABLE }>(host, slots, 0, 1, key(closure), 1),
                        abi::HELPER_COMPLETED
                    );
                    closed_table_tests::siblings(slots, &saved, Some(0));
                    assert_eq!(
                        call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, 2, 0, 0, 2),
                        abi::HELPER_COMPLETED
                    );
                });
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.table_reads,
                        frame.count.upvalue_reads
                    ),
                    (2, 2, 1, 1)
                );
                assert!(frame.panic.is_none());
            });
            assert_eq!(pc, 3);
            closed_table_tests::identical(values[0], value);
            closed_table_tests::identical(values[2], value);
        }
    });
}

#[test]
fn local_table_reads_cover_all_value_kinds_sizes_and_receiver_aliases() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... return t.key").unwrap();
        let table = crate::Table::new(&ctx);
        let key_value = Value::String(ctx.intern(b"key"));
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            key_value,
            Value::Table(table),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for count in [1, 7, 256] {
            for receiver in [0, count - 1] {
                for dest in [0, count - 1] {
                    for value in kinds {
                        table.set_readonly(&ctx, false);
                        table.set_raw(&ctx, key_value, value).unwrap();
                        table.set_readonly(&ctx, true);
                        let mut values: Vec<_> =
                            (0..count).map(|i| kinds[i % kinds.len()]).collect();
                        values[receiver] = Value::Table(table);
                        let mut pc = 0;
                        LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut values,
                            |mut registers| {
                                let mut frame = helpers::Frame {
                                    ctx,
                                    closure,
                                    registers: &mut registers,
                                    count: helpers::Counts::default(),
                                    slot_count: count,
                                    panic: None,
                                    read_hints: None,
                                    projection: None,
                                };
                                with_session(&mut frame, |host, slots| unsafe {
                                    let saved: Vec<_> =
                                        (0..count).map(|i| slots.add(i).read()).collect();
                                    assert_eq!(
                                        call::<{ abi::HELPER_GET_TABLE }>(
                                            host,
                                            slots,
                                            dest as u32,
                                            receiver as u32,
                                            key(closure),
                                            17
                                        ),
                                        abi::HELPER_COMPLETED
                                    );
                                    closed_table_tests::siblings(slots, &saved, Some(dest));
                                });
                                assert_eq!(
                                    (
                                        frame.count.calls,
                                        frame.count.completed,
                                        frame.count.table_reads,
                                        frame.count.upvalue_reads
                                    ),
                                    (1, 1, 1, 0)
                                );
                                assert_eq!(
                                    (
                                        frame.count.declined,
                                        frame.count.table_writes,
                                        frame.count.upvalue_writes,
                                        frame.count.allocations
                                    ),
                                    (0, 0, 0, 0)
                                );
                                assert!(frame.panic.is_none());
                            },
                        );
                        closed_table_tests::identical(values[dest], value);
                        assert_eq!(pc, 18);
                    }
                }
            }
        }
    });
}

#[test]
fn local_table_partial_refresh_preserves_retirement_and_full_fallbacks() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... return t.key").unwrap();
        let table = crate::Table::new(&ctx);
        let key_value = Value::String(ctx.intern(b"key"));
        table.set_raw(&ctx, key_value, Value::Integer(42)).unwrap();
        let mut values = [
            Value::Table(table),
            Value::Nil,
            key_value,
            Value::Integer(7),
            Value::Boolean(true),
            Value::Table(table),
        ];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 6,
                panic: None,
                read_hints: None,
                projection: None,
            };
            with_session(&mut frame, |host, slots| unsafe {
                assert_eq!(
                    materialize_terminal(host, slots, 3, NUMBER, 7.5f64.to_bits()),
                    abi::HELPER_COMPLETED
                );
                let saved: Vec<_> = (0..6).map(|i| slots.add(i).read()).collect();
                assert_eq!(
                    call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, key(closure), 1),
                    abi::HELPER_COMPLETED
                );
                closed_table_tests::siblings(slots, &saved, Some(1));
                assert_eq!(slots.add(3).read().tag, NIL);
                assert!(slots.add(3).read().pointer.is_null());
                assert_eq!(
                    call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, 2, 2),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(load(slots.add(3).read()).unwrap().bits, 7.5f64.to_bits());
                assert_eq!(
                    materialize_terminal(host, slots, 0, INTEGER, 99),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, key(closure), 3),
                    abi::HELPER_DECLINED
                );
                assert_eq!(load(slots.read()).unwrap().bits, 99);
                assert_eq!(
                    call::<{ abi::HELPER_MOVE }>(host, slots, 0, 5, 0, 4),
                    abi::HELPER_COMPLETED
                );
                let saved: Vec<_> = (0..6).map(|i| slots.add(i).read()).collect();
                table.set_raw(&ctx, key_value, Value::Nil).unwrap();
                let meta = crate::Table::new(&ctx);
                meta.set_raw(
                    &ctx,
                    Value::String(ctx.intern(b"__index")),
                    Value::Boolean(true),
                )
                .unwrap();
                table.set_metatable(ctx, Some(meta));
                assert_eq!(
                    call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, key(closure), 5),
                    abi::HELPER_DECLINED
                );
                closed_table_tests::siblings(slots, &saved, None);
                assert_eq!(load(slots.add(1).read()).unwrap().bits, 42);
                table.set_metatable(ctx, None);
                assert_eq!(
                    call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, key(closure), 6),
                    abi::HELPER_COMPLETED
                );
                closed_table_tests::siblings(slots, &saved, Some(1));
            });
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.completed,
                    frame.count.declined,
                    frame.count.table_reads,
                    frame.count.upvalue_reads
                ),
                (6, 4, 2, 3, 0)
            );
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 7);
        assert!(matches!(values[0], Value::Table(t) if t == table));
        assert!(matches!(values[1], Value::Nil));
        assert!(matches!(values[3], Value::Number(n) if n == 7.5));
    });
}

#[test]
fn local_table_invalid_operands_and_borrow_panics_stop_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... return t.key").unwrap();
        let table = crate::Table::new(&ctx);
        table
            .set_raw(&ctx, Value::String(ctx.intern(b"key")), Value::Integer(42))
            .unwrap();
        for case in 0..4 {
            let mut values = [Value::Table(table), Value::Nil, Value::Integer(7)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 3,
                    panic: None,
                    read_hints: None,
                    projection: None,
                };
                with_session(&mut frame, |host, slots| unsafe {
                    assert!(store(
                        slots.add(2).read(),
                        Slot {
                            tag: INTEGER,
                            bits: 41
                        }
                    ));
                    let inner = table.into_inner();
                    let _borrow = (case == 3).then(|| inner.borrow_mut(&ctx));
                    let (dest, receiver, operand) = match case {
                        0 => (u32::MAX, 0, key(closure)),
                        1 => (1, u32::MAX, key(closure)),
                        2 => (1, 0, u32::MAX),
                        _ => (1, 0, key(closure)),
                    };
                    assert_eq!(
                        call::<{ abi::HELPER_GET_TABLE }>(host, slots, dest, receiver, operand, 42),
                        abi::HELPER_PANICKED
                    );
                    assert_eq!(
                        call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, key(closure), 43),
                        abi::HELPER_PANICKED
                    );
                    assert_eq!(
                        materialize(host, slots, 2, INTEGER, 99),
                        abi::HELPER_PANICKED
                    );
                });
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.declined,
                        frame.count.table_reads,
                        frame.count.upvalue_reads
                    ),
                    (1, 0, 0, 0, 0)
                );
                assert!(frame.panic.is_some());
            });
            assert_eq!(pc, 43);
            assert!(matches!(values[1], Value::Nil));
            assert!(matches!(values[2], Value::Integer(41)));
        }
    });
}

#[test]
fn local_constant_table_preserves_live_sibling_field_pointers() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... return t.key").unwrap();
        let key = abi::CONSTANT_OPERAND
            | closure
                .prototype()
                .constants
                .iter()
                .position(|v| matches!(v, crate::Constant::String(s) if s.as_bytes() == b"key"))
                .unwrap() as u32;
        let table = crate::Table::new(&ctx);
        table
            .set_raw(&ctx, Value::String(ctx.intern(b"key")), Value::Integer(42))
            .unwrap();
        for dest in [0, 1, 2] {
            let mut values = [
                Value::Table(table),
                Value::Boolean(true),
                Value::Number(-0.0),
                Value::Integer(7),
            ];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 4,
                    panic: None,
                    read_hints: None,
                    projection: None,
                };
                with_session(&mut frame, |host, slots| unsafe {
                    let saved: Vec<_> = (0..4).map(|i| slots.add(i).read()).collect();
                    assert_eq!(
                        call::<{ abi::HELPER_GET_TABLE }>(host, slots, dest, 0, key, 17),
                        abi::HELPER_COMPLETED
                    );
                    for (index, previous) in saved.into_iter().enumerate() {
                        if index == dest as usize {
                            continue;
                        }
                        let current = slots.add(index).read();
                        assert_eq!(
                            (current.tag, current.pointer),
                            (previous.tag, previous.pointer)
                        );
                        if let Some(value) = load(previous) {
                            assert!(store(previous, value));
                            assert_eq!(load(current).unwrap().bits, value.bits);
                        }
                    }
                    assert_eq!(load(slots.add(dest as usize).read()).unwrap().bits, 42);
                });
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.table_reads,
                        frame.count.upvalue_reads
                    ),
                    (1, 1, 1, 0)
                );
                assert!(frame.panic.is_none());
            });
            assert_eq!(pc, 18);
            assert!(matches!(values[dest as usize], Value::Integer(42)));
        }
    });
}
