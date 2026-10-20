use super::*;
use crate::closure::{UpValue, UpValueState};
use crate::thread::LuaRegisters;

fn identical<'gc>(a: Value<'gc>, b: Value<'gc>) {
    assert_eq!(std::mem::discriminant(&a), std::mem::discriminant(&b));
    match (a, b) {
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

fn counts(c: &helpers::Counts) -> [u64; 8] {
    [
        c.calls,
        c.completed,
        c.declined,
        c.table_reads,
        c.table_writes,
        c.upvalue_reads,
        c.upvalue_writes,
        c.allocations,
    ]
}

fn frame<'gc, R>(
    ctx: crate::Context<'gc>,
    closure: crate::Closure<'gc>,
    values: &mut [Value<'gc>],
    count: usize,
    body: impl FnOnce(&mut helpers::Frame<'gc, '_, '_, '_>) -> R,
) -> (R, usize) {
    let mut pc = 0;
    let result = LuaRegisters::with_test_frame(ctx, &mut pc, values, |mut registers| {
        body(&mut helpers::Frame {
            ctx,
            closure,
            registers: &mut registers,
            count: helpers::Counts::default(),
            slot_count: count,
            panic: None,
            read_hints: None,
            projection: None,
        })
    });
    (result, pc)
}

unsafe fn saved(slots: *mut Packed, count: usize) -> Vec<Packed> {
    (0..count).map(|i| unsafe { slots.add(i).read() }).collect()
}

unsafe fn siblings(slots: *mut Packed, saved: &[Packed], dest: Option<usize>) {
    for (index, previous) in saved.iter().copied().enumerate() {
        if Some(index) == dest {
            continue;
        }
        let current = unsafe { slots.add(index).read() };
        assert_eq!(current.0, previous.0);
        if matches!(previous.tag(), Some(INTEGER | NUMBER)) {
            let value = unsafe { previous.load() }.unwrap();
            assert!(unsafe { previous.store_numeric(value) });
            let actual = unsafe { current.load() }.unwrap();
            assert_eq!((actual.tag, actual.bits), (value.tag, value.bits));
        }
    }
}

fn key(closure: crate::Closure<'_>) -> u32 {
    abi::CONSTANT_OPERAND
        | closure
            .prototype()
            .constants
            .iter()
            .position(|c| matches!(c, crate::Constant::String(s) if s.as_bytes() == b"key"))
            .unwrap() as u32
}

#[test]
fn packed_fast_upvalues_preserve_all_values_aliases_and_sibling_loans() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for count in [1, 7, 256] {
            for dest in [0, count - 1] {
                for origin in 0..4 {
                    for value in kinds(ctx) {
                        let mut values = vec![Value::Integer(17); count + 2];
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
                                    read_hints: None,
                                    projection: None,
                                };
                                let (status, refusal, result) =
                                    bridge::with_bridge_report::<256, _>(
                                        &mut frame,
                                        |slots, bridge| unsafe {
                                            let before = saved(slots, count);
                                            let status = (bridge.functions.helpers[7])(
                                                bridge.control,
                                                dest as u32,
                                                0,
                                                0,
                                                17,
                                            );
                                            siblings(slots, &before, Some(dest));
                                            status
                                        },
                                    )
                                    .unwrap();
                                assert_eq!(
                                    (status, refusal, result),
                                    (
                                        abi::HELPER_COMPLETED,
                                        None,
                                        Some((17, abi::HELPER_COMPLETED))
                                    )
                                );
                                assert_eq!(counts(&frame.count), [1, 1, 0, 0, 0, 1, 0, 0]);
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

fn foreign<'gc>(
    ctx: crate::Context<'gc>,
    closure: crate::Closure<'gc>,
    expected: Option<Value<'gc>>,
) {
    let run = |packed| {
        let mut values = [
            Value::Number(-0.0),
            Value::Integer(731),
            Value::Boolean(true),
        ];
        let (result, pc) = frame(ctx, closure, &mut values, 3, |frame| {
            let status = if packed {
                bridge::with_bridge_report::<256, _>(frame, |slots, bridge| unsafe {
                    let before = saved(slots, 3);
                    let status = (bridge.functions.helpers[7])(bridge.control, 0, 0, 0, 17);
                    if expected.is_some() {
                        siblings(slots, &before, Some(0));
                    } else {
                        assert_eq!(
                            (bridge.functions.helpers[7])(bridge.control, 0, 0, 0, 18),
                            abi::HELPER_PANICKED
                        );
                    }
                    status
                })
                .unwrap()
                .0
            } else {
                helpers::call_direct::<8>(frame, 0, 0, 0, 17)
            };
            let panic = frame.panic.take().map(|p| {
                if let Some(s) = p.downcast_ref::<&str>() {
                    (true, (*s).to_owned())
                } else {
                    (false, p.downcast_ref::<String>().unwrap().clone())
                }
            });
            (status, counts(&frame.count), panic)
        });
        assert_eq!(pc, 18);
        identical(values[0], expected.unwrap_or(Value::Number(-0.0)));
        identical(values[1], Value::Integer(731));
        (result, pc)
    };
    assert_eq!(run(true), run(false));
}

#[test]
fn packed_fast_foreign_upvalues_preserve_live_expired_locked_and_invalid_outcomes() {
    let mut lua = crate::Lua::empty();
    let expired = lua.enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for value in kinds(ctx) {
            for (index, locked) in [(0, false), (1, false), (0, true)] {
                crate::thread::OpenUpValue::with_test_stack(ctx, &[value], index, locked, |cell| {
                    closure.set_upvalue(&ctx, 0, cell);
                    foreign(ctx, closure, (index == 0 && !locked).then_some(value));
                });
            }
        }
        crate::thread::OpenUpValue::with_test_stack(ctx, &[Value::Integer(17)], 0, false, |cell| {
            closure.set_upvalue(&ctx, 0, cell)
        });
        ctx.stash(closure)
    });
    lua.gc_collect();
    lua.enter(|ctx| foreign(ctx, ctx.fetch(&expired), None));
}

#[test]
fn packed_fast_table_reads_writes_and_allocation_preserve_sibling_loans() {
    crate::Lua::empty().enter(|ctx| {
        let env = crate::Table::new(&ctx);
        let closure =
            crate::Closure::load_with_env(ctx, None, b"local t=... t.key=42 return key", env)
                .unwrap();
        let key = key(closure);
        let string_key = Value::String(ctx.intern(b"key"));
        for count in [4, 7, 256] {
            for value in kinds(ctx) {
                for kind in [3, 4, 5, 6] {
                    let table = crate::Table::new(&ctx);
                    table.set_raw(&ctx, string_key, value).unwrap();
                    env.set_raw(&ctx, string_key, value).unwrap();
                    let mut values = vec![Value::Integer(731); count];
                    values[1] = Value::Table(table);
                    values[2] = value;
                    let (c, pc) = frame(ctx, closure, &mut values, count, |frame| {
                        let (_, refusal, result) =
                            bridge::with_bridge_report::<256, _>(frame, |slots, bridge| unsafe {
                                let before = saved(slots, count);
                                let (a, b, c) = match kind {
                                    3 => (0, 2, 2),
                                    4 => (0, 1, key),
                                    5 => (1, key, 2),
                                    _ => (0, 0, key),
                                };
                                assert_eq!(
                                    (bridge.functions.helpers[kind - 1])(
                                        bridge.control,
                                        a,
                                        b,
                                        c,
                                        17
                                    ),
                                    abi::HELPER_COMPLETED
                                );
                                siblings(slots, &before, if kind == 5 { None } else { Some(0) });
                            })
                            .unwrap();
                        assert_eq!(refusal, None);
                        assert_eq!(result, Some((17, abi::HELPER_COMPLETED)));
                        assert!(frame.panic.is_none());
                        counts(&frame.count)
                    });
                    assert_eq!(pc, 18);
                    assert_eq!(
                        c,
                        [
                            1,
                            1,
                            0,
                            u64::from(kind == 4 || kind == 6),
                            u64::from(kind == 5),
                            u64::from(kind == 6),
                            0,
                            u64::from(kind == 3)
                        ]
                    );
                    if kind == 3 {
                        assert!(matches!(values[0], Value::Table(_)));
                    } else if kind != 5 {
                        identical(values[0], value);
                    }
                    identical(table.get_raw(&ctx, string_key), value);
                }
            }
        }
    });
}

#[test]
fn packed_fast_table_guards_preserve_declines_aliases_and_retired_operands() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... t.key=42").unwrap();
        for mask in 0..32 {
            for key_value in [Value::Integer(1), Value::Nil, Value::Number(f64::NAN)] {
                for retired in [false, true] {
                    let table = crate::Table::new(&ctx);
                    let (readonly, metatable, newindex, intercept, existing) = (
                        mask & 1 != 0,
                        mask & 2 != 0,
                        mask & 4 != 0,
                        mask & 8 != 0,
                        mask & 16 != 0,
                    );
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
                    let completed = matches!(key_value, Value::Integer(_))
                        && !readonly
                        && !(metatable && newindex && (!existing || intercept));
                    let mut values = [
                        Value::Table(table),
                        key_value,
                        Value::Integer(42),
                        Value::Number(-0.0),
                    ];
                    let (c, pc) = frame(ctx, closure, &mut values, 4, |frame| {
                        let (_, refusal, result) =
                            bridge::with_bridge_report::<256, _>(frame, |slots, bridge| unsafe {
                                if retired {
                                    (bridge.functions.terminal)(bridge.control, 2, INTEGER, 42);
                                }
                                let before = saved(slots, 4);
                                let status =
                                    (bridge.functions.helpers[4])(bridge.control, 0, 1, 2, 17);
                                assert_eq!(status, u32::from(completed));
                                siblings(slots, &before, None);
                            })
                            .unwrap();
                        assert_eq!((refusal, result), (None, Some((17, u32::from(completed)))));
                        assert!(frame.panic.is_none());
                        counts(&frame.count)
                    });
                    assert_eq!(pc, 17 + usize::from(completed));
                    assert_eq!(
                        c,
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
        }
        for dest in [0, 1] {
            for intercept in [false, true] {
                let table = crate::Table::new(&ctx);
                if intercept {
                    let mt = crate::Table::new(&ctx);
                    mt.set(ctx, crate::meta_ops::MetaMethod::Index, true)
                        .unwrap();
                    table.set_metatable(ctx, Some(mt));
                }
                let mut values = [Value::Integer(42), Value::Table(table), Value::Number(-0.0)];
                let (c, pc) = frame(ctx, closure, &mut values, 3, |frame| {
                    bridge::with_bridge_report::<256, _>(frame, |slots, bridge| unsafe {
                        let before = saved(slots, 3);
                        assert_eq!(
                            (bridge.functions.helpers[3])(
                                bridge.control,
                                dest,
                                1,
                                key(closure),
                                23
                            ),
                            u32::from(!intercept)
                        );
                        siblings(slots, &before, (!intercept).then_some(dest as usize));
                    })
                    .unwrap();
                    counts(&frame.count)
                });
                assert_eq!(pc, 23 + usize::from(!intercept));
                assert_eq!(
                    c,
                    [
                        1,
                        u64::from(!intercept),
                        u64::from(intercept),
                        u64::from(!intercept),
                        0,
                        0,
                        0,
                        0
                    ]
                );
                identical(
                    values[dest as usize],
                    if !intercept {
                        Value::Nil
                    } else if dest == 0 {
                        Value::Integer(42)
                    } else {
                        Value::Table(table)
                    },
                );
            }
        }
    });
}

#[test]
fn packed_fast_table_write_constants_and_receiver_aliases_keep_loans_live() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local t=... t.key=42").unwrap();
        let constant = abi::CONSTANT_OPERAND
            | closure
                .prototype()
                .constants
                .iter()
                .position(|c| matches!(c, crate::Constant::Integer(42)))
                .unwrap() as u32;
        for key_operand in [0, 1, key(closure)] {
            for value_operand in [0, 2, constant] {
                for retired in [false, true] {
                    let table = crate::Table::new(&ctx);
                    let mut values = [
                        Value::Table(table),
                        Value::String(ctx.intern(b"key")),
                        Value::Integer(17),
                        Value::Number(-0.0),
                    ];
                    let expected_key = if key_operand == 0 {
                        values[0]
                    } else {
                        values[1]
                    };
                    let expected_value = match value_operand {
                        0 => values[0],
                        2 => values[2],
                        _ => Value::Integer(42),
                    };
                    let (c, pc) = frame(ctx, closure, &mut values, 4, |frame| {
                        bridge::with_bridge_report::<256, _>(frame, |slots, bridge| unsafe {
                            if retired {
                                assert_eq!(
                                    (bridge.functions.terminal)(bridge.control, 2, INTEGER, 17),
                                    1
                                );
                            }
                            let before = saved(slots, 4);
                            assert_eq!(
                                (bridge.functions.helpers[4])(
                                    bridge.control,
                                    0,
                                    key_operand,
                                    value_operand,
                                    17
                                ),
                                1
                            );
                            siblings(slots, &before, None);
                        })
                        .unwrap();
                        counts(&frame.count)
                    });
                    assert_eq!(pc, 18);
                    assert_eq!(c, [1, 1, 0, 0, 1, 0, 0, 0]);
                    identical(table.get_raw(&ctx, expected_key), expected_value);
                    identical(values[0], Value::Table(table));
                }
            }
        }
    });
}

#[test]
fn packed_fast_controls_refresh_after_full_helper_and_preserve_retired_reads() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV,42").unwrap();
        closure.set_upvalue(
            &ctx,
            0,
            UpValue::new(&ctx, UpValueState::Closed(Value::Integer(17))),
        );
        let constant = closure
            .prototype()
            .constants
            .iter()
            .position(|c| matches!(c, crate::Constant::Integer(42)))
            .unwrap() as u32;
        let mut values = [Value::Number(-0.0), Value::Integer(731), Value::Integer(7)];
        let (c, pc) = frame(ctx, closure, &mut values, 3, |frame| {
            bridge::with_bridge_report::<256, _>(frame, |slots, bridge| unsafe {
                assert_eq!(
                    (bridge.functions.helpers[7])(bridge.control, 0, 0, 0, 17),
                    1
                );
                assert_eq!(
                    (bridge.functions.helpers[1])(bridge.control, 0, constant, 0, 23),
                    1
                );
                assert_eq!(
                    (bridge.functions.terminal)(bridge.control, 2, INTEGER, 9),
                    1
                );
                let before = saved(slots, 3);
                assert_eq!(
                    (bridge.functions.helpers[7])(bridge.control, 1, 0, 0, 42),
                    1
                );
                siblings(slots, &before, Some(1));
                assert_eq!(slots.add(2).read().0.addr(), EMPTY);
            })
            .unwrap();
            assert!(frame.panic.is_none());
            counts(&frame.count)
        });
        assert_eq!(c, [3, 3, 0, 0, 0, 2, 0, 0]);
        assert_eq!(pc, 43);
        for (actual, expected) in values.into_iter().zip([42, 17, 9]) {
            identical(actual, Value::Integer(expected));
        }
    });
}

#[test]
fn packed_fast_upvalue_prefix_fallback_matches_full_frame_partial_effects() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for source in [0, 2, 3, usize::MAX] {
            for dest in [0, 2] {
                let run = |packed| {
                    let mut values = [
                        Value::Integer(7),
                        Value::Boolean(false),
                        Value::Number(-0.0),
                    ];
                    let mut pc = 0;
                    let result = LuaRegisters::projection_split_frame(
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
                                read_hints: None,
                                projection: None,
                            };
                            let status = if packed {
                                bridge::with_bridge_report::<256, _>(
                                    &mut frame,
                                    |_, bridge| unsafe {
                                        (bridge.functions.helpers[7])(
                                            bridge.control,
                                            dest,
                                            0,
                                            0,
                                            17,
                                        )
                                    },
                                )
                                .unwrap()
                                .0
                            } else {
                                helpers::call_direct::<8>(&mut frame, dest, 0, 0, 17)
                            };
                            (status, counts(&frame.count), frame.panic.is_some())
                        },
                    );
                    (result, pc, values)
                };
                let (actual, pc, values) = run(true);
                let (expected, expected_pc, expected_values) = run(false);
                assert_eq!(actual, expected);
                assert_eq!(pc, expected_pc);
                for (actual, expected) in values.into_iter().zip(expected_values) {
                    identical(actual, expected);
                }
            }
        }
    });
}

#[test]
fn packed_fast_panics_preserve_payload_identity_and_completed_write_progress() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for completed in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let token = Box::new(Payload(drops.clone()));
            let pointer = std::ptr::from_ref(&*token);
            let mut values = [Value::Number(-0.0), Value::Integer(731)];
            let (_, pc) = frame(ctx, closure, &mut values, 2, |frame| {
                let (_, refusal, result) =
                    bridge::with_bridge_report::<256, _>(frame, |_, bridge| unsafe {
                        assert_eq!(
                            bridge::boundary::<256>(bridge.control, |bound| {
                                if completed {
                                    bound.control.begin(17, &mut bound.helper_result);
                                    bound.store_helper::<{ abi::HELPER_GET_UPVALUE }>(
                                        0,
                                        Value::Integer(42),
                                        17,
                                    )?;
                                    std::panic::resume_unwind(token);
                                } else {
                                    bound.new_table_with(0, 17, || {
                                        std::panic::resume_unwind(token)
                                    })?;
                                }
                                unreachable!()
                            }),
                            abi::HELPER_PANICKED
                        );
                        assert_eq!(
                            (bridge.functions.helpers[7])(bridge.control, 0, 0, 0, 23),
                            abi::HELPER_PANICKED
                        );
                        assert_eq!(
                            (bridge.functions.write)(bridge.control, 1, INTEGER, 0),
                            abi::HELPER_PANICKED
                        );
                    })
                    .unwrap();
                assert_eq!(
                    (refusal, result),
                    (
                        None,
                        Some((
                            17,
                            if completed {
                                abi::HELPER_COMPLETED
                            } else {
                                abi::HELPER_PANICKED
                            }
                        ))
                    )
                );
                assert_eq!(
                    counts(&frame.count),
                    [1, u64::from(completed), 0, 0, 0, u64::from(completed), 0, 0]
                );
                let payload = frame.panic.take().unwrap().downcast::<Payload>().unwrap();
                assert_eq!(std::ptr::from_ref(&*payload), pointer);
                assert_eq!(drops.load(Ordering::Relaxed), 0);
                drop(payload);
                assert_eq!(drops.load(Ordering::Relaxed), 1);
            });
            assert_eq!(pc, 18);
            identical(
                values[0],
                if completed {
                    Value::Integer(42)
                } else {
                    Value::Number(-0.0)
                },
            );
            identical(values[1], Value::Integer(731));
        }
    });
}
