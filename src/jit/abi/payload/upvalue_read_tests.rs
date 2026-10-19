use super::*;
use crate::closure::{UpValue, UpValueState};
use closed_table_tests::{identical, siblings};
use table_write_tests::{counts, scope};

fn check_foreign<'gc>(
    ctx: crate::Context<'gc>,
    closure: crate::Closure<'gc>,
    expected: Option<Value<'gc>>,
) {
    let run = |partial| {
        let mut values = [Value::Number(-0.0), Value::Integer(7), Value::Boolean(true)];
        let mut pc = 0;
        let (c, panic) =
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 3,
                    panic: None,
                    projection: None,
                };
                let status = if partial {
                    let mut status = 0;
                    with_session(&mut frame, |host, slots| unsafe {
                        let saved: Vec<_> = (0..3).map(|i| slots.add(i).read()).collect();
                        assert!(store(
                            slots.add(1).read(),
                            Slot {
                                tag: INTEGER,
                                bits: 41
                            }
                        ));
                        status = call::<8>(host, slots, 0, 0, 0, 42);
                        if expected.is_some() {
                            siblings(slots, &saved, Some(0));
                        } else {
                            assert_eq!(call::<8>(host, slots, 0, 0, 0, 43), abi::HELPER_PANICKED);
                            assert_eq!(
                                materialize(host, slots, 1, INTEGER, 99),
                                abi::HELPER_PANICKED
                            );
                        }
                    });
                    status
                } else {
                    frame.registers.stack_frame[1] = Value::Integer(41);
                    helpers::call_direct::<8>(&mut frame, 0, 0, 0, 42)
                };
                assert_eq!(
                    status,
                    if expected.is_some() {
                        abi::HELPER_COMPLETED
                    } else {
                        abi::HELPER_PANICKED
                    }
                );
                let panic = frame.panic.map(|payload| {
                    if let Some(message) = payload.downcast_ref::<&str>() {
                        (true, (*message).to_owned())
                    } else {
                        (false, payload.downcast_ref::<String>().unwrap().clone())
                    }
                });
                (counts(frame.count), panic)
            });
        assert_eq!(pc, 43);
        identical(values[0], expected.unwrap_or(Value::Number(-0.0)));
        identical(values[1], Value::Integer(41));
        identical(values[2], Value::Boolean(true));
        (c, panic)
    };
    let original = run(false);
    let partial = run(true);
    assert_eq!(original, partial);
    let completed = u64::from(expected.is_some());
    assert_eq!(partial.0, [1, completed, 0, 0, 0, completed, 0, 0]);
}

#[test]
fn foreign_upvalue_reads_preserve_values_and_exact_panic_payloads() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for value in [
            Value::Nil,
            Value::Boolean(false),
            Value::Integer(i64::MIN),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"foreign")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ] {
            for (index, locked) in [(0, false), (1, false), (0, true)] {
                crate::thread::OpenUpValue::with_test_stack(ctx, &[value], index, locked, |cell| {
                    closure.set_upvalue(&ctx, 0, cell);
                    check_foreign(ctx, closure, (!locked && index == 0).then_some(value));
                });
            }
        }
    });
}

#[test]
fn expired_foreign_stack_preserves_original_panic_and_prior_writes() {
    let mut lua = crate::Lua::empty();
    let closure = lua.enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        crate::thread::OpenUpValue::with_test_stack(ctx, &[Value::Integer(42)], 0, false, |cell| {
            closure.set_upvalue(&ctx, 0, cell);
        });
        ctx.stash(closure)
    });
    lua.gc_collect();
    lua.enter(|ctx| check_foreign(ctx, ctx.fetch(&closure), None));
}

#[test]
fn upvalue_prefix_fallback_preserves_original_full_frame_access() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for source in [0, 2, 3, usize::MAX] {
            for dest in [0, 2] {
                let mut values = [Value::Integer(7), Value::Boolean(true), Value::Number(-0.0)];
                let mut pc = 0;
                LuaRegisters::projection_split_frame(
                    ctx,
                    &mut pc,
                    &mut values,
                    0,
                    |mut registers| {
                        closure.set_upvalue(&ctx, 0, registers.projection_open_at(ctx, source));
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
                                materialize_terminal(host, slots, 1, BOOLEAN, 0),
                                abi::HELPER_COMPLETED
                            );
                            assert_eq!(
                                call::<8>(host, slots, dest, 0, 0, 17),
                                if source < 3 && dest == 0 {
                                    abi::HELPER_COMPLETED
                                } else {
                                    abi::HELPER_PANICKED
                                }
                            );
                            if source < 3 && dest == 0 {
                                assert_eq!(load(slots.add(1).read()).unwrap().bits, 0);
                                assert_eq!(
                                    slots.add(1).read().tag,
                                    if source == 2 { BOOLEAN } else { NIL }
                                );
                            }
                        });
                        let completed = u64::from(source < 3 && dest == 0);
                        assert_eq!(
                            counts(frame.count),
                            [1, completed, 0, 0, 0, completed, 0, 0]
                        );
                        assert_eq!(frame.panic.is_some(), completed == 0);
                    },
                );
                assert_eq!(pc, 18);
                identical(
                    values[0],
                    if source == 2 && dest == 0 {
                        Value::Number(-0.0)
                    } else {
                        Value::Integer(7)
                    },
                );
                identical(values[1], Value::Boolean(false));
                identical(
                    values[2],
                    if source == 0 && dest == 2 {
                        Value::Integer(7)
                    } else {
                        Value::Number(-0.0)
                    },
                );
            }
        }
    });
}

#[test]
fn upvalue_reads_preserve_values_aliases_and_sibling_field_loans() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"capture")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for count in [1, 7, 256] {
            for dest in [0, count - 1] {
                for origin in 0..4 {
                    for value in kinds {
                        let mut values: Vec<_> =
                            (0..count + 2).map(|i| kinds[i % kinds.len()]).collect();
                        let source = if origin == 3 { count - 1 } else { dest };
                        values[0] = value;
                        values[source + 2] = value;
                        let before = values.clone();
                        let mut pc = 0;
                        LuaRegisters::projection_split_frame(
                            ctx,
                            &mut pc,
                            &mut values,
                            2,
                            |mut registers| {
                                let cell = match origin {
                                    0 => UpValue::new(&ctx, UpValueState::Closed(value)),
                                    1 => registers.projection_open_at(ctx, 0),
                                    _ => registers.projection_open_at(ctx, source + 2),
                                };
                                closure.set_upvalue(&ctx, 0, cell);
                                let mut frame = helpers::Frame {
                                    ctx,
                                    closure,
                                    registers: &mut registers,
                                    count: helpers::Counts::default(),
                                    slot_count: count,
                                    panic: None,
                                    projection: None,
                                };
                                with_session(&mut frame, |host, slots| unsafe {
                                    let saved: Vec<_> =
                                        (0..count).map(|i| slots.add(i).read()).collect();
                                    assert_eq!(
                                        call::<{ abi::HELPER_GET_UPVALUE }>(
                                            host,
                                            slots,
                                            dest as u32,
                                            0,
                                            0,
                                            17
                                        ),
                                        abi::HELPER_COMPLETED
                                    );
                                    siblings(slots, &saved, Some(dest));
                                });
                                assert_eq!(counts(frame.count), [1, 1, 0, 0, 0, 1, 0, 0]);
                                assert!(frame.panic.is_none());
                            },
                        );
                        assert_eq!(pc, 18);
                        for (index, actual) in values.into_iter().enumerate() {
                            identical(
                                actual,
                                if index == dest + 2 {
                                    value
                                } else {
                                    before[index]
                                },
                            );
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn upvalue_reads_observe_retired_sources_closing_and_joining() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let mut values = [Value::Integer(7), Value::Number(-0.0), Value::Boolean(true)];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let cell = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            closure.set_upvalue(&ctx, 0, cell);
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 3,
                panic: None,
                projection: None,
            };
            with_session(&mut frame, |host, slots| unsafe {
                assert_eq!(
                    materialize_terminal(host, slots, 0, INTEGER, 42),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    materialize_terminal(host, slots, 2, BOOLEAN, 0),
                    abi::HELPER_COMPLETED
                );
                let saved: Vec<_> = (0..3).map(|i| slots.add(i).read()).collect();
                assert_eq!(call::<8>(host, slots, 1, 0, 0, 1), abi::HELPER_COMPLETED);
                siblings(slots, &saved, Some(1));
                assert_eq!(load(slots.add(1).read()).unwrap().bits, 42);
                assert_eq!(slots.read().tag, NIL);
                cell.set(&ctx, UpValueState::Closed(Value::Number(-0.0)));
                assert_eq!(call::<8>(host, slots, 1, 0, 0, 2), abi::HELPER_COMPLETED);
                assert_eq!(load(slots.add(1).read()).unwrap().bits, (-0.0f64).to_bits());
                closure.set_upvalue(
                    &ctx,
                    0,
                    UpValue::new(&ctx, UpValueState::Closed(Value::Boolean(true))),
                );
                assert_eq!(call::<8>(host, slots, 1, 0, 0, 3), abi::HELPER_COMPLETED);
                assert_eq!(load(slots.add(1).read()).unwrap().bits, 1);
                assert_eq!(slots.add(2).read().tag, NIL);
                assert_eq!(
                    call::<{ abi::HELPER_MOVE }>(host, slots, 1, 0, 0, 4),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(load(slots.read()).unwrap().bits, 42);
                assert_eq!(load(slots.add(2).read()).unwrap().bits, 0);
            });
            assert_eq!(counts(frame.count), [4, 4, 0, 0, 0, 3, 0, 0]);
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 5);
        identical(values[0], Value::Integer(42));
        identical(values[1], Value::Integer(42));
        identical(values[2], Value::Boolean(false));
    });
}

#[test]
fn upvalue_read_invalid_indices_preserve_fallback_and_stopped_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        closure.set_upvalue(
            &ctx,
            0,
            UpValue::new(&ctx, UpValueState::Closed(Value::Integer(42))),
        );
        for (count, dest, source) in [
            (0, 0, 0),
            (3, 3, 0),
            (3, u32::MAX, 0),
            (3, 0, 1),
            (3, 0, u32::MAX),
        ] {
            let mut values = vec![Value::Integer(7); count];
            let (c, pc, panic) = scope(ctx, closure, &mut values, |host, slots| unsafe {
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
                    call::<8>(host, slots, dest, source, 0, 42),
                    abi::HELPER_PANICKED
                );
                assert_eq!(call::<8>(host, slots, 0, 0, 0, 44), abi::HELPER_PANICKED);
                assert_eq!(
                    materialize(host, slots, 0, INTEGER, 99),
                    abi::HELPER_PANICKED
                );
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
