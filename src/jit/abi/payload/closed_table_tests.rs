use super::*;

fn key(closure: crate::Closure<'_>) -> u32 {
    abi::CONSTANT_OPERAND | closure.prototype().constants.iter().position(|value| {
        matches!(value, crate::Constant::String(value) if value.as_bytes() == b"key")
    }).unwrap() as u32
}

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    assert_eq!(
        std::mem::discriminant(&actual),
        std::mem::discriminant(&expected)
    );
    match (actual, expected) {
        (Value::String(a), Value::String(b)) => assert!(ottavino_gc_arena::Gc::ptr_eq(
            a.into_inner(),
            b.into_inner()
        )),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        (a, b) => {
            let (a, b) = (Slot::from_value(a), Slot::from_value(b));
            assert_eq!((a.tag, a.bits), (b.tag, b.bits));
        }
    }
}

unsafe fn siblings(slots: *mut Payload, saved: &[Payload], dest: Option<usize>) {
    for (index, previous) in saved.iter().copied().enumerate() {
        if Some(index) == dest {
            continue;
        }
        let current = unsafe { slots.add(index).read() };
        assert_eq!(
            (current.tag, current.pointer),
            (previous.tag, previous.pointer)
        );
        if let Some(value) = unsafe { load(previous) } {
            assert!(unsafe { store(previous, value) });
            let actual = unsafe { load(current) }.unwrap();
            assert_eq!((actual.tag, actual.bits), (value.tag, value.bits));
        }
    }
}

#[test]
fn closed_constant_table_reads_keep_sibling_pointers_live() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV.key").unwrap();
        let table = crate::Table::new(&ctx);
        closure.set_upvalue(
            &ctx,
            0,
            crate::closure::UpValue::new(
                &ctx,
                crate::closure::UpValueState::Closed(Value::Table(table)),
            ),
        );
        let key_index = key(closure);
        let key = Value::String(ctx.intern(b"key"));
        let kinds = [
            Value::Nil,
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"value")),
            Value::Table(table),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 42)),
        ];
        for count in [1, 7, 255, 256] {
            for dest in [0, count / 2, count - 1] {
                let mut values: Vec<_> = (0..count).map(|i| kinds[i % kinds.len()]).collect();
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                    let mut frame = helpers::Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: count,
                        panic: None,
                        projection: None,
                    };
                    for value in kinds {
                        table.set_readonly(&ctx, false);
                        table.set_raw(&ctx, key, value).unwrap();
                        table.set_readonly(&ctx, true);
                        with_session(&mut frame, |host, slots| unsafe {
                            let saved: Vec<_> = (0..count).map(|i| slots.add(i).read()).collect();
                            assert_eq!(
                                call::<{ abi::HELPER_GET_UP_TABLE }>(
                                    host,
                                    slots,
                                    dest as u32,
                                    0,
                                    key_index,
                                    17
                                ),
                                abi::HELPER_COMPLETED
                            );
                            siblings(slots, &saved, Some(dest));
                        });
                        identical(frame.registers.stack_frame[dest], value);
                        assert_eq!(*frame.registers.pc, 18);
                    }
                    assert_eq!(
                        (
                            frame.count.calls,
                            frame.count.completed,
                            frame.count.table_reads,
                            frame.count.upvalue_reads
                        ),
                        [kinds.len() as u64; 4].into()
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
                });
            }
        }
    });
}

#[test]
fn closed_reads_refresh_control_pointers_after_fallback_and_preserve_declines() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV.key").unwrap();
        let table = crate::Table::new(&ctx);
        let key_index = key(closure);
        let key = Value::String(ctx.intern(b"key"));
        table.set_raw(&ctx, key, Value::Integer(42)).unwrap();
        let mut values = [
            Value::Table(table),
            key,
            Value::Integer(7),
            Value::Boolean(true),
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
                projection: None,
            };
            with_session(&mut frame, |host, slots| unsafe {
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 2, 0, key_index, 1),
                    abi::HELPER_COMPLETED
                );
                cell.set(
                    &ctx,
                    crate::closure::UpValueState::Closed(Value::Table(table)),
                );
                for operand in [1, key_index] {
                    assert_eq!(
                        call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 2, 0, operand, 2),
                        abi::HELPER_COMPLETED
                    );
                }
                assert_eq!(
                    call::<{ abi::HELPER_MOVE }>(host, slots, 2, 3, 0, 3),
                    abi::HELPER_COMPLETED
                );
                let saved: Vec<_> = (0..4).map(|i| slots.add(i).read()).collect();
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 2, 0, key_index, 4),
                    abi::HELPER_COMPLETED
                );
                siblings(slots, &saved, Some(2));
                table.set_raw(&ctx, key, Value::Nil).unwrap();
                let meta = crate::Table::new(&ctx);
                meta.set_raw(
                    &ctx,
                    Value::String(ctx.intern(b"__index")),
                    Value::Boolean(true),
                )
                .unwrap();
                table.set_metatable(ctx, Some(meta));
                let saved: Vec<_> = (0..4).map(|i| slots.add(i).read()).collect();
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 2, 0, key_index, 5),
                    abi::HELPER_DECLINED
                );
                siblings(slots, &saved, None);
                assert_eq!(load(slots.add(2).read()).unwrap().bits, 42);
                cell.set(
                    &ctx,
                    crate::closure::UpValueState::Closed(Value::Integer(9)),
                );
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 2, 0, key_index, 5),
                    abi::HELPER_DECLINED
                );
                siblings(slots, &saved, None);
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 2, 0, u32::MAX, 6),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 2, 0, key_index, 7),
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
                (8, 5, 2, 4, 4)
            );
            assert!(frame.panic.is_some());
        });
        assert_eq!(pc, 7);
        assert!(matches!(values[2], Value::Integer(42)));
    });
}

#[test]
fn narrow_table_read_panics_preserve_direct_writes_and_stop_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV.key").unwrap();
        let table = crate::Table::new(&ctx);
        closure.set_upvalue(
            &ctx,
            0,
            crate::closure::UpValue::new(
                &ctx,
                crate::closure::UpValueState::Closed(Value::Table(table)),
            ),
        );
        let mut values = [Value::Nil, Value::Integer(7)];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 2,
                panic: None,
                projection: None,
            };
            with_session(&mut frame, |host, slots| unsafe {
                assert_eq!(
                    materialize(host, slots, 1, INTEGER, 41),
                    abi::HELPER_COMPLETED
                );
                let inner = table.into_inner();
                let _borrow = inner.borrow_mut(&ctx);
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 0, 0, key(closure), 5),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    call::<{ abi::HELPER_GET_UP_TABLE }>(host, slots, 0, 0, key(closure), 6),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    materialize(host, slots, 1, INTEGER, 99),
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
        assert_eq!(pc, 6);
        assert!(matches!(values[0], Value::Nil));
        assert!(matches!(values[1], Value::Integer(41)));
    });
}
