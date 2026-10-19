use super::*;
use closed_table_tests::{identical, siblings};

pub(super) fn scope<'gc>(
    ctx: crate::Context<'gc>,
    closure: crate::Closure<'gc>,
    values: &mut [Value<'gc>],
    body: impl FnOnce(*mut (), *mut Payload),
) -> (helpers::Counts, usize, bool) {
    let count = values.len();
    let mut pc = 0;
    let (counts, panic) =
        LuaRegisters::with_test_native_frame(ctx, &mut pc, values, |mut registers, mut _list| {
            let mut frame = helpers::Frame {
                list: Some(_list.reborrow()),
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: count,
                panic: None,
                projection: None,
            };
            with_session(&mut frame, body);
            (frame.count, frame.panic.is_some())
        });
    (counts, pc, panic)
}

pub(super) fn counts(counts: helpers::Counts) -> [u64; 8] {
    [
        counts.calls,
        counts.completed,
        counts.declined,
        counts.table_reads,
        counts.table_writes,
        counts.upvalue_reads,
        counts.upvalue_writes,
        counts.allocations,
    ]
}

fn constant(closure: crate::Closure<'_>, key: bool) -> u32 {
    abi::CONSTANT_OPERAND
        | closure
            .prototype()
            .constants
            .iter()
            .position(|v| {
                if key {
                    matches!(v, crate::Constant::String(s) if s.as_bytes() == b"key")
                } else {
                    matches!(v, crate::Constant::Integer(42))
                }
            })
            .unwrap() as u32
}

#[test]
fn table_writes_preserve_every_value_and_live_sibling_descriptor() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... t.key=42").unwrap();
        let table = crate::Table::new(&ctx);
        let key = Value::String(ctx.intern(b"key"));
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            key,
            Value::Table(table),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for count in [7, 256] {
            for value in kinds {
                for constant_key in [false, true] {
                    let mut values: Vec<_> = (0..count).map(|i| kinds[i % kinds.len()]).collect();
                    values[0] = Value::Table(table);
                    values[1] = key;
                    values[2] = value;
                    let before = values.clone();
                    let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
                        let saved: Vec<_> = (0..count).map(|i| slots.add(i).read()).collect();
                        let operand = if constant_key {
                            constant(closure, true)
                        } else {
                            1
                        };
                        assert_eq!(
                            call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, operand, 2, 17),
                            abi::HELPER_COMPLETED
                        );
                        siblings(slots, &saved, None);
                    });
                    assert_eq!(counts(c), [1, 1, 0, 0, 1, 0, 0, 0]);
                    assert_eq!(pc, 18);
                    assert!(!panic);
                    identical(table.get_raw(&ctx, key), value);
                    for (a, b) in values.into_iter().zip(before) {
                        identical(a, b);
                    }
                }
            }
        }
    });
}

#[test]
fn table_writes_preserve_aliases_constants_and_retired_values() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... t.key=42").unwrap();
        let table = crate::Table::new(&ctx);
        for key in [0, 1, 2, constant(closure, true)] {
            for value in [0, 1, 2, constant(closure, false)] {
                for retired in [false, true] {
                    let mut values = [
                        Value::Table(table),
                        Value::Integer(7),
                        Value::Number(-0.0),
                        Value::Boolean(true),
                    ];
                    let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
                        assert!(store(
                            slots.add(1).read(),
                            Slot {
                                tag: INTEGER,
                                bits: 81
                            }
                        ));
                        if retired {
                            assert_eq!(
                                materialize_terminal(host, slots, 1, INTEGER, 82),
                                abi::HELPER_COMPLETED
                            );
                        }
                        let saved: Vec<_> = (0..4).map(|i| slots.add(i).read()).collect();
                        assert_eq!(
                            call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, key, value, 19),
                            abi::HELPER_COMPLETED
                        );
                        siblings(slots, &saved, None);
                    });
                    let operand = |index: u32| {
                        if index & abi::CONSTANT_OPERAND != 0 {
                            closure.prototype().constants[(index & !abi::CONSTANT_OPERAND) as usize]
                                .into()
                        } else {
                            values[index as usize]
                        }
                    };
                    identical(table.get_raw(&ctx, operand(key)), operand(value));
                    assert_eq!(counts(c), [1, 1, 0, 0, 1, 0, 0, 0]);
                    assert_eq!(pc, 20);
                    assert!(!panic);
                    identical(values[1], Value::Integer(if retired { 82 } else { 81 }));
                }
            }
        }
    });
}

#[test]
fn table_writes_preserve_readonly_metatable_and_interception_guards() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for mask in 0..32 {
            let readonly = mask & 1 != 0;
            let metatable = mask & 2 != 0;
            let newindex = mask & 4 != 0;
            let intercept = mask & 8 != 0;
            let existing = mask & 16 != 0;
            for key in [Value::Integer(1), Value::Nil, Value::Number(f64::NAN)] {
                let table = crate::Table::new(&ctx);
                if existing {
                    table.set(ctx, 1, 7).unwrap();
                }
                if metatable {
                    let mt = crate::Table::new(&ctx);
                    if newindex {
                        mt.set(ctx, crate::meta_ops::MetaMethod::NewIndex, true)
                            .unwrap();
                    }
                    table.set_metatable(ctx, Some(mt));
                }
                table.set_intercept_all_writes(&ctx, intercept);
                table.set_readonly(&ctx, readonly);
                let valid_key = matches!(key, Value::Integer(_));
                let completed =
                    valid_key && !readonly && !(metatable && newindex && (!existing || intercept));
                let mut values = [
                    Value::Table(table),
                    key,
                    Value::Integer(42),
                    Value::Number(-0.0),
                ];
                let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
                    let saved: Vec<_> = (0..4).map(|i| slots.add(i).read()).collect();
                    assert_eq!(
                        call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, 1, 2, 17),
                        if completed {
                            abi::HELPER_COMPLETED
                        } else {
                            abi::HELPER_DECLINED
                        }
                    );
                    siblings(slots, &saved, None);
                });
                assert_eq!(
                    counts(c),
                    [
                        1,
                        u64::from(completed),
                        u64::from(!completed),
                        0,
                        u64::from(completed),
                        0,
                        0,
                        0
                    ]
                );
                assert_eq!(pc, if completed { 18 } else { 17 });
                assert!(!panic);
                identical(
                    table.get_raw(&ctx, Value::Integer(1)),
                    if completed {
                        Value::Integer(42)
                    } else if existing {
                        Value::Integer(7)
                    } else {
                        Value::Nil
                    },
                );
            }
        }
    });
}

#[test]
fn table_write_panics_preserve_prior_writes_and_stop_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... t.key=42").unwrap();
        for case in 0..6 {
            let table = crate::Table::new(&ctx);
            let mut values = [Value::Table(table), Value::Integer(1), Value::Integer(7)];
            let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
                assert!(store(
                    slots.add(2).read(),
                    Slot {
                        tag: INTEGER,
                        bits: 41
                    }
                ));
                let inner = table.into_inner();
                let _borrow = (case == 5).then(|| inner.borrow_mut(&ctx));
                let (a, b, c) = match case {
                    0 => (u32::MAX, 1, 2),
                    1 => (0, 3, 2),
                    2 => (0, 1, 3),
                    3 => (0, u32::MAX, 2),
                    4 => (0, 1, u32::MAX),
                    _ => (0, 1, 2),
                };
                assert_eq!(
                    call::<{ abi::HELPER_SET_TABLE }>(host, slots, a, b, c, 42),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, 1, 2, 44),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    materialize(host, slots, 2, INTEGER, 99),
                    abi::HELPER_PANICKED
                );
            });
            assert_eq!(counts(c), [1, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(pc, 43);
            assert!(panic);
            identical(values[2], Value::Integer(41));
            identical(table.get_raw(&ctx, Value::Integer(1)), Value::Nil);
        }
    });
}

#[test]
fn table_writes_preserve_non_table_receivers_and_current_frame_captures() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV.key").unwrap();
        for receiver in [
            Value::Nil,
            Value::Boolean(true),
            Value::Integer(7),
            Value::Number(-0.0),
            Value::String(ctx.intern(b"receiver")),
            Value::Function(closure.into()),
        ] {
            let mut values = [receiver, Value::Integer(1), Value::Integer(42)];
            let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
                let saved: Vec<_> = (0..3).map(|i| slots.add(i).read()).collect();
                assert_eq!(
                    call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, 1, 2, 7),
                    abi::HELPER_DECLINED
                );
                siblings(slots, &saved, None);
            });
            assert_eq!(counts(c), [1, 0, 1, 0, 0, 0, 0, 0]);
            assert_eq!(pc, 7);
            assert!(!panic);
            identical(values[0], receiver);
        }
        let table = crate::Table::new(&ctx);
        let mut singleton = [Value::Table(table)];
        let (c, pc, panic) = scope(ctx, closure, &mut singleton, |host, slots| unsafe {
            assert_eq!(
                call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, 0, 0, 7),
                abi::HELPER_COMPLETED
            );
        });
        assert_eq!(counts(c), [1, 1, 0, 0, 1, 0, 0, 0]);
        assert_eq!(pc, 8);
        assert!(!panic);
        identical(
            table.get_raw(&ctx, Value::Table(table)),
            Value::Table(table),
        );
        let mut values = [
            Value::Table(table),
            Value::Integer(1),
            Value::Integer(7),
            Value::Nil,
        ];
        let mut pc = 0;
        LuaRegisters::with_test_native_frame(
            ctx,
            &mut pc,
            &mut values,
            |mut registers, mut _list| {
                let cell = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(2));
                closure.set_upvalue(&ctx, 0, cell);
                let mut frame = helpers::Frame {
                    list: Some(_list.reborrow()),
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 4,
                    panic: None,
                    projection: None,
                };
                with_session(&mut frame, |host, slots| unsafe {
                    assert!(store(
                        slots.add(2).read(),
                        Slot {
                            tag: INTEGER,
                            bits: 82
                        }
                    ));
                    assert_eq!(
                        call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, 1, 2, 7),
                        abi::HELPER_COMPLETED
                    );
                    assert_eq!(
                        call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, 3, 0, 0, 8),
                        abi::HELPER_COMPLETED
                    );
                    assert_eq!(load(slots.add(3).read()).unwrap().bits, 82);
                    assert_eq!(
                        call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, 1, 3, 9),
                        abi::HELPER_COMPLETED
                    );
                    assert_eq!(load(slots.add(2).read()).unwrap().bits, 82);
                });
                assert_eq!(counts(frame.count), [3, 3, 0, 0, 2, 1, 0, 0]);
                assert!(frame.panic.is_none());
            },
        );
        assert_eq!(pc, 10);
        identical(table.get_raw(&ctx, Value::Integer(1)), Value::Integer(82));
        identical(values[2], Value::Integer(82));
        identical(values[3], Value::Integer(82));
    });
}
