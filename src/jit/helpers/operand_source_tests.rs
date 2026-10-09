use super::{tests::assert_identical, tests::invoke, *};

fn constant(closure: Closure<'_>, bytes: &[u8]) -> u32 {
    abi::CONSTANT_OPERAND
        | closure
            .prototype()
            .constants
            .iter()
            .position(|value| {
                matches!(value, crate::Constant::String(value) if value.as_bytes() == bytes)
            })
            .unwrap() as u32
}

fn table_call(
    frame: &mut Frame<'_, '_, '_, '_>,
    slots: &mut [Slot],
    upvalue: bool,
    write: bool,
    a: u32,
    b: u32,
    c: u32,
) -> u32 {
    match (upvalue, write) {
        (false, false) => invoke::<{ abi::HELPER_GET_TABLE }>(frame, slots, a, b, c, 17),
        (false, true) => invoke::<{ abi::HELPER_SET_TABLE }>(frame, slots, a, b, c, 17),
        (true, false) => invoke::<{ abi::HELPER_GET_UP_TABLE }>(frame, slots, a, b, c, 17),
        (true, true) => invoke::<{ abi::HELPER_SET_UP_TABLE }>(frame, slots, a, b, c, 17),
    }
}

#[test]
fn table_receivers_preserve_live_tags_and_canonical_identity() {
    crate::Lua::empty().enter(|ctx| {
        let table = Table::new(&ctx);
        let closure = Closure::load(ctx, None, b"return _ENV").unwrap();
        let values = [
            Value::Nil,
            Value::Boolean(true),
            Value::Integer(7),
            Value::Number(-0.0),
            Value::String(ctx.intern(b"receiver")),
            Value::Table(table),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        let tags = [
            abi::NIL,
            abi::BOOLEAN,
            abi::INTEGER,
            abi::NUMBER,
            abi::REFERENCE,
        ];
        for value in values {
            for tag in tags {
                for upvalue in [false, true] {
                    for write in [false, true] {
                        table
                            .set_raw(&ctx, Value::Integer(7), Value::Integer(41))
                            .unwrap();
                        let mut canonical = [value, Value::Integer(99), Value::Nil, Value::Nil];
                        let mut pc = 0;
                        LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            |mut registers| {
                                let cell = registers
                                    .open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
                                closure.set_upvalue(&ctx, 0, cell);
                                let mut slots = [
                                    Slot {
                                        tag,
                                        bits: u64::MAX,
                                    },
                                    Slot::from_value(Value::Integer(7)),
                                    Slot::from_value(Value::Number(-0.0)),
                                    Slot::from_value(Value::Nil),
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
                                let completed =
                                    tag == abi::REFERENCE && matches!(value, Value::Table(_));
                                let result = if write {
                                    table_call(&mut frame, &mut slots, upvalue, true, 0, 1, 2)
                                } else {
                                    table_call(&mut frame, &mut slots, upvalue, false, 3, 0, 1)
                                };
                                assert_eq!(
                                    result,
                                    if completed {
                                        abi::HELPER_COMPLETED
                                    } else {
                                        abi::HELPER_DECLINED
                                    }
                                );
                                assert_eq!(*frame.registers.pc, if completed { 18 } else { 17 });
                                assert_eq!(
                                    (
                                        frame.count.calls,
                                        frame.count.completed,
                                        frame.count.declined
                                    ),
                                    (1, u64::from(completed), u64::from(!completed))
                                );
                                assert_eq!(frame.count.table_reads, u64::from(completed && !write));
                                assert_eq!(frame.count.table_writes, u64::from(completed && write));
                                assert_eq!(
                                    frame.count.upvalue_reads,
                                    u64::from(completed && upvalue)
                                );
                                assert_identical(frame.registers.stack_frame[0], value);
                                assert_identical(
                                    frame.registers.stack_frame[3],
                                    if completed && !write {
                                        Value::Integer(41)
                                    } else {
                                        Value::Nil
                                    },
                                );
                                assert_identical(
                                    table.get_raw(&ctx, Value::Integer(7)),
                                    if completed && write {
                                        Value::Number(-0.0)
                                    } else {
                                        Value::Integer(41)
                                    },
                                );
                                assert_eq!(slots[0].tag, tag);
                                assert_eq!(slots[0].bits, u64::MAX);
                                assert!(frame.panic.is_none());
                            },
                        );
                    }
                }
            }
        }
    });
}

#[test]
fn non_table_receivers_still_validate_all_operands_before_declining() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return _ENV").unwrap();
        for upvalue in [false, true] {
            for bad_source in 0..3 {
                let mut canonical = [Value::Nil, Value::Integer(99), Value::Nil];
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                    let cell = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
                    closure.set_upvalue(&ctx, 0, cell);
                    let mut slots = [
                        Slot::from_value(Value::Integer(1)),
                        Slot::from_value(Value::Integer(7)),
                        Slot::from_value(Value::Number(-0.0)),
                    ];
                    let mut frame = Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: Counts::default(),
                        table_cache: Default::default(),
                        slot_count: 3,
                        panic: None,
                        projection: None,
                    };
                    let result = match bad_source {
                        0 => table_call(&mut frame, &mut slots, upvalue, false, 2, 0, 3),
                        1 => table_call(&mut frame, &mut slots, upvalue, true, 0, 3, 2),
                        _ => table_call(&mut frame, &mut slots, upvalue, true, 0, 1, 3),
                    };
                    assert_eq!(result, abi::HELPER_PANICKED);
                    assert_eq!(*frame.registers.pc, 18);
                    assert_identical(frame.registers.stack_frame[0], Value::Integer(1));
                    assert_identical(frame.registers.stack_frame[1], Value::Integer(7));
                    assert_identical(frame.registers.stack_frame[2], Value::Number(-0.0));
                    assert_eq!(
                        (
                            frame.count.calls,
                            frame.count.completed,
                            frame.count.declined
                        ),
                        (1, 0, 0)
                    );
                    assert_eq!(
                        (
                            frame.count.table_reads,
                            frame.count.table_writes,
                            frame.count.upvalue_reads
                        ),
                        (0, 0, 0)
                    );
                    assert!(frame.panic.is_some());
                });
            }
        }
    });
}

#[test]
fn table_sources_preserve_constants_pending_scalars_references_and_aliases() {
    crate::Lua::empty().enter(|ctx| {
        for upvalue in [false, true] {
            for key_constant in [false, true] {
                for value_constant in [false, true] {
                    for reference in [false, true] {
                        let table = Table::new(&ctx);
                        let closure = Closure::load_with_env(
                            ctx,
                            None,
                            b"return _ENV, 'key', 'value'",
                            table,
                        )
                        .unwrap();
                        let key_index = constant(closure, b"key");
                        let value_index = constant(closure, b"value");
                        let key = if key_constant {
                            closure.prototype().constants
                                [(key_index & !abi::CONSTANT_OPERAND) as usize]
                                .into()
                        } else {
                            Value::Integer(7)
                        };
                        let value = if value_constant {
                            closure.prototype().constants
                                [(value_index & !abi::CONSTANT_OPERAND) as usize]
                                .into()
                        } else if reference {
                            Value::Table(Table::new(&ctx))
                        } else {
                            Value::Number(-0.0)
                        };
                        let mut canonical = [
                            Value::Table(table),
                            Value::Integer(99),
                            Value::Nil,
                            Value::Nil,
                        ];
                        if matches!(value, Value::Table(_)) {
                            canonical[2] = value;
                        }
                        let mut pc = 0;
                        LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            |mut registers| {
                                let mut slots = [
                                    Slot::from_value(Value::Table(table)),
                                    Slot::from_value(key),
                                    Slot::from_value(value),
                                    Slot::from_value(Value::Nil),
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
                                let b = if key_constant { key_index } else { 1 };
                                let c = if value_constant { value_index } else { 2 };
                                assert_eq!(
                                    table_call(&mut frame, &mut slots, upvalue, true, 0, b, c),
                                    abi::HELPER_COMPLETED
                                );
                                assert_identical(table.get_raw(&ctx, key), value);
                                assert!(table.get_raw(&ctx, Value::Integer(99)).is_nil());
                                assert_eq!(*frame.registers.pc, 18);
                                assert_eq!(
                                    table_call(&mut frame, &mut slots, upvalue, false, 1, 0, b),
                                    abi::HELPER_COMPLETED
                                );
                                assert_identical(frame.registers.stack_frame[1], value);
                                assert_identical(
                                    slots[1].value(frame.registers.stack_frame[1]),
                                    value,
                                );
                                frame.registers.stack_frame[1] = key;
                                slots[1] = Slot::from_value(key);
                                assert_eq!(
                                    table_call(&mut frame, &mut slots, upvalue, false, 0, 0, b),
                                    abi::HELPER_COMPLETED
                                );
                                assert_identical(frame.registers.stack_frame[0], value);
                                assert_eq!(*frame.registers.pc, 18);
                                assert_eq!(
                                    (
                                        frame.count.calls,
                                        frame.count.completed,
                                        frame.count.declined
                                    ),
                                    (3, 3, 0)
                                );
                                assert_eq!(
                                    (
                                        frame.count.table_reads,
                                        frame.count.table_writes,
                                        frame.count.upvalue_reads
                                    ),
                                    (2, 1, if upvalue { 3 } else { 0 })
                                );
                                assert!(frame.panic.is_none());
                            },
                        );
                    }
                }
            }
        }
    });
}

#[test]
fn table_source_declines_preserve_slots_effects_and_counters() {
    crate::Lua::empty().enter(|ctx| {
        for upvalue in [false, true] {
            for key_constant in [false, true] {
                for value_constant in [false, true] {
                    for readonly in [false, true] {
                        let table = Table::new(&ctx);
                        let meta = Table::new(&ctx);
                        meta.set_field(ctx, "__index", Table::new(&ctx));
                        if !readonly {
                            meta.set_field(ctx, "__newindex", Table::new(&ctx));
                        }
                        table.set_metatable(ctx, Some(meta));
                        table.set_readonly(&ctx, readonly);
                        let closure = Closure::load_with_env(
                            ctx,
                            None,
                            b"return _ENV, 'key', 'value'",
                            table,
                        )
                        .unwrap();
                        let b = if key_constant {
                            constant(closure, b"key")
                        } else {
                            1
                        };
                        let c = if value_constant {
                            constant(closure, b"value")
                        } else {
                            2
                        };
                        let mut canonical = [Value::Table(table), Value::Integer(99), Value::Nil];
                        let mut pc = 0;
                        LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            |mut registers| {
                                let mut slots = [
                                    Slot::from_value(Value::Table(table)),
                                    Slot::from_value(Value::Integer(7)),
                                    Slot::from_value(Value::Number(-0.0)),
                                ];
                                let before = slots.map(|slot| (slot.tag, slot.bits));
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
                                assert_eq!(
                                    table_call(&mut frame, &mut slots, upvalue, true, 0, b, c),
                                    abi::HELPER_DECLINED
                                );
                                assert_eq!(*frame.registers.pc, 17);
                                assert_eq!(
                                    table_call(&mut frame, &mut slots, upvalue, false, 1, 0, b),
                                    abi::HELPER_DECLINED
                                );
                                assert_eq!(*frame.registers.pc, 17);
                                assert_eq!(before, slots.map(|slot| (slot.tag, slot.bits)));
                                assert_identical(
                                    frame.registers.stack_frame[1],
                                    Value::Integer(99),
                                );
                                assert_identical(frame.registers.stack_frame[2], Value::Nil);
                                assert!(table.get_raw(&ctx, Value::Integer(7)).is_nil());
                                let constant_key: Value<'_> = closure.prototype().constants
                                    [(constant(closure, b"key") & !abi::CONSTANT_OPERAND) as usize]
                                    .into();
                                assert!(table.get_raw(&ctx, constant_key).is_nil());
                                assert_eq!(
                                    (
                                        frame.count.calls,
                                        frame.count.completed,
                                        frame.count.declined
                                    ),
                                    (2, 0, 2)
                                );
                                assert_eq!(
                                    (
                                        frame.count.table_reads,
                                        frame.count.table_writes,
                                        frame.count.upvalue_reads
                                    ),
                                    (0, 0, 0)
                                );
                                assert!(frame.panic.is_none());
                            },
                        );
                    }
                }
            }
        }
    });
}

#[test]
fn malformed_table_sources_preserve_bounds_panics_and_materialization() {
    crate::Lua::empty().enter(|ctx| {
        for upvalue in [false, true] {
            for operand in [
                8,
                1 << 24,
                abi::CONSTANT_OPERAND | 65535,
                abi::CONSTANT_OPERAND | (1 << 24),
            ] {
                for bad_source in 0..3 {
                    let table = Table::new(&ctx);
                    let closure =
                        Closure::load_with_env(ctx, None, b"return _ENV, 'key', 'value'", table)
                            .unwrap();
                    let mut canonical = [Value::Table(table), Value::Integer(99), Value::Nil];
                    let mut pc = 0;
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                        let mut slots = [
                            Slot::from_value(Value::Table(table)),
                            Slot::from_value(Value::Integer(7)),
                            Slot::from_value(Value::Number(-0.0)),
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
                        let result = match bad_source {
                            0 => table_call(&mut frame, &mut slots, upvalue, false, 2, 0, operand),
                            1 => table_call(&mut frame, &mut slots, upvalue, true, 0, operand, 2),
                            _ => table_call(&mut frame, &mut slots, upvalue, true, 0, 1, operand),
                        };
                        assert_eq!(result, abi::HELPER_PANICKED);
                        assert_eq!(*frame.registers.pc, 18);
                        assert_identical(frame.registers.stack_frame[1], Value::Integer(7));
                        assert_identical(frame.registers.stack_frame[2], Value::Number(-0.0));
                        assert!(table.get_raw(&ctx, Value::Integer(7)).is_nil());
                        assert_eq!(
                            (
                                frame.count.calls,
                                frame.count.completed,
                                frame.count.declined
                            ),
                            (1, 0, 0)
                        );
                        assert_eq!(
                            (
                                frame.count.table_reads,
                                frame.count.table_writes,
                                frame.count.upvalue_reads
                            ),
                            (0, 0, 0)
                        );
                        assert!(frame.panic.is_some());
                    });
                }
            }
        }
    });
}
