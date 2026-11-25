use super::*;

pub(super) fn counters(count: &helpers::Counts) -> [u64; 8] {
    [
        count.calls,
        count.completed,
        count.declined,
        count.table_reads,
        count.table_writes,
        count.upvalue_reads,
        count.upvalue_writes,
        count.allocations,
    ]
}

fn scalar(value: Value<'_>) -> (u64, u64) {
    let slot = Slot::from_value(value);
    (slot.tag, slot.bits)
}

fn dispatch(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    kind: u32,
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    match kind {
        1 => helpers::call_direct::<1>(frame, a, b, c, pc),
        2 => helpers::call_direct::<2>(frame, a, b, c, pc),
        3 => helpers::call_direct::<3>(frame, a, b, c, pc),
        4 => helpers::call_direct::<4>(frame, a, b, c, pc),
        5 => helpers::call_direct::<5>(frame, a, b, c, pc),
        6 => helpers::call_direct::<6>(frame, a, b, c, pc),
        7 => helpers::call_direct::<7>(frame, a, b, c, pc),
        8 => helpers::call_direct::<8>(frame, a, b, c, pc),
        9 => helpers::call_direct::<9>(frame, a, b, c, pc),
        10 => helpers::call_direct::<10>(frame, a, b, c, pc),
        _ => unreachable!(),
    }
}

#[test]
fn packed_c_bridge_matches_all_ten_real_helper_effects() {
    crate::Lua::empty().enter(|ctx| {
        for kind in 1..=10 {
            let run = |packed: bool| {
                let env = crate::Table::new(&ctx);
                let table = crate::Table::new(&ctx);
                let key = Value::String(ctx.intern(b"key"));
                env.set_raw(&ctx, key, Value::Integer(17)).unwrap();
                table.set_raw(&ctx, key, Value::Integer(17)).unwrap();
                let closure =
                    crate::Closure::load_with_env(ctx, None, b"return _ENV,17", env).unwrap();
                let mut values = [
                    Value::Nil,
                    Value::Table(table),
                    key,
                    Value::Integer(42),
                    Value::Integer(73),
                ];
                let (a, b, c) = match kind {
                    1 => (0, 3, 0),
                    2 => (0, 0, 0),
                    3 => (0, 0, 0),
                    4 => (0, 1, 2),
                    5 => (1, 2, 3),
                    6 => (0, 0, 2),
                    7 => (0, 2, 3),
                    8 => (0, 0, 0),
                    9 => (0, 3, 0),
                    10 => {
                        values[2] = Value::Integer(0);
                        (1, 2, 0)
                    }
                    _ => unreachable!(),
                };
                let mut pc = 5;
                let mut fuel = crate::Fuel::with(47);
                let counts =
                    crate::thread::LuaRegisters::with_test_frame_state(
                        ctx,
                        &mut pc,
                        &mut values,
                        &mut fuel,
                        false,
                        |mut registers| {
                            let mut frame = helpers::Frame {
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: helpers::Counts::default(),
                                slot_count: 5,
                                panic: None,
                                read_hints: None,
                                projection: None,
                            };
                            let result =
                                if packed {
                                    with_bridge::<5, _>(&mut frame, |slots, bridge| unsafe {
                                        assert!(slots.add(4).read().store_numeric(Slot {
                                            tag: INTEGER,
                                            bits: 731
                                        }));
                                        let result = (bridge.functions.helpers
                                            [(kind - 1) as usize])(
                                            bridge.control, a, b, c, 7
                                        );
                                        assert_eq!(slots.add(4).read().load().unwrap().bits, 731);
                                        assert!(slots.add(4).read().store_numeric(Slot {
                                            tag: INTEGER,
                                            bits: 99
                                        }));
                                        result
                                    })
                                    .unwrap()
                                } else {
                                    frame.registers.stack_frame[4] = Value::Integer(731);
                                    let result = dispatch(&mut frame, kind, a, b, c, 7);
                                    frame.registers.stack_frame[4] = Value::Integer(99);
                                    result
                                };
                            assert_eq!(result, abi::HELPER_COMPLETED, "helper {kind}");
                            assert!(frame.panic.is_none());
                            counters(&frame.count)
                        },
                    );
                if kind == 3 {
                    assert!(matches!(values[0], Value::Table(_)));
                }
                if kind == 8 {
                    assert!(matches!(values[0], Value::Table(value) if value == env));
                }
                let upvalue = closure.upvalues()[0].get().get();
                let crate::closure::UpValueState::Closed(upvalue) = upvalue else {
                    panic!("unexpected open capture")
                };
                (
                    values.map(scalar),
                    counts,
                    pc,
                    fuel.remaining(),
                    scalar(table.get_raw(&ctx, key)),
                    scalar(env.get_raw(&ctx, key)),
                    scalar(table.get_raw(&ctx, Value::Integer(1))),
                    scalar(table.get_raw(&ctx, Value::Integer(2))),
                    scalar(upvalue),
                )
            };
            assert_eq!(run(false), run(true), "helper {kind}");
        }
    });
}

#[test]
fn packed_c_bridge_contains_helper_panics_and_preserves_pending_payload() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for pending in [false, true] {
            let mut values = [Value::Integer(17)];
            let mut pc = 5;
            crate::thread::LuaRegisters::with_test_frame(
                ctx,
                &mut pc,
                &mut values,
                |mut registers| {
                    let mut frame = helpers::Frame {
                        read_hints: None,
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: 1,
                        panic: pending.then(|| Box::new(731u64) as Box<dyn std::any::Any + Send>),
                        projection: None,
                    };
                    with_bridge::<1, _>(&mut frame, |slots, bridge| unsafe {
                        if !pending {
                            assert!(slots.read().store_numeric(Slot {
                                tag: INTEGER,
                                bits: 42
                            }));
                        }
                        assert_eq!(
                            (bridge.functions.helpers[0])(bridge.control, 99, 0, 0, 7),
                            abi::HELPER_PANICKED
                        );
                        assert_eq!(
                            (bridge.functions.helpers[0])(bridge.control, 0, 0, 0, 8),
                            abi::HELPER_PANICKED
                        );
                    })
                    .unwrap();
                    assert_eq!(frame.count.calls, u64::from(!pending));
                    if pending {
                        assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
                    } else {
                        assert!(frame.panic.is_some());
                    }
                },
            );
            assert_eq!(pc, if pending { 5 } else { 8 });
            assert_eq!(scalar(values[0]), (INTEGER, if pending { 17 } else { 42 }));
        }
    });
}

#[test]
fn packed_c_bridge_retains_refusal_without_fabricating_panic() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let mut values = [Value::Integer(17)];
        let mut pc = 5;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 1,
                panic: None,
                read_hints: None,
                projection: None,
            };
            let result = with_bridge::<1, _>(&mut frame, |_, bridge| unsafe {
                let bound = &mut *bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 1>>();
                bound.scope.stopped = true;
                assert_eq!(
                    (bridge.functions.helpers[0])(bridge.control, 0, 0, 0, 7),
                    REFUSED
                );
                assert_eq!(
                    (bridge.functions.helpers[0])(bridge.control, 0, 0, 0, 8),
                    REFUSED
                );
            });
            assert_eq!(result, Err(Refusal::Stopped));
            assert_eq!(frame.count.calls, 0);
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 5);
        assert_eq!(scalar(values[0]), (INTEGER, 17));
    });
}

#[test]
fn packed_c_bridge_refuses_failed_refresh_without_replaying_effects() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let mut values = [Value::Integer(17)];
        let mut pc = 5;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 1,
                panic: None,
                read_hints: None,
                projection: None,
            };
            let result = with_bridge::<2, _>(&mut frame, |_, bridge| unsafe {
                let bound = &mut *bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 2>>();
                bound.scope.count = 2;
                assert_eq!(
                    (bridge.functions.helpers[0])(bridge.control, 0, 0, 0, 7),
                    REFUSED
                );
                assert_eq!(
                    (bridge.functions.helpers[0])(bridge.control, 0, 0, 0, 8),
                    REFUSED
                );
            });
            assert_eq!(result, Err(Refusal::Bounds));
            assert_eq!(counters(&frame.count), [1, 1, 0, 0, 0, 0, 0, 0]);
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 8);
        assert_eq!(scalar(values[0]), (INTEGER, 17));
    });
}

unsafe extern "C" fn panic_gateway(control: *mut (), payload: u32, _: u32, _: u32, _: u32) -> u32 {
    unsafe { boundary::<1>(control, |_| std::panic::panic_any(u64::from(payload))) }
}

#[test]
fn packed_c_boundary_preserves_original_unwind_payload_and_direct_writes() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let mut values = [Value::Integer(17)];
        let mut pc = 5;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 1,
                panic: None,
                read_hints: None,
                projection: None,
            };
            with_bridge::<1, _>(&mut frame, |slots, bridge| unsafe {
                assert!(slots.read().store_numeric(Slot {
                    tag: INTEGER,
                    bits: 42
                }));
                assert_eq!(
                    panic_gateway(bridge.control, 731, 0, 0, 7),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    panic_gateway(bridge.control, 999, 0, 0, 7),
                    abi::HELPER_PANICKED
                );
                assert_eq!(
                    (bridge.functions.helpers[0])(bridge.control, 0, 0, 0, 8),
                    abi::HELPER_PANICKED
                );
            })
            .unwrap();
            assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
            assert_eq!(frame.count.calls, 0);
        });
        assert_eq!(pc, 5);
        assert_eq!(scalar(values[0]), (INTEGER, 42));
    });
}

#[test]
fn packed_c_bridge_declines_without_poisoning_fresh_descriptor_reads() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let mut values = [Value::Integer(17), Value::Boolean(false)];
        let mut pc = 5;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
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
            with_bridge::<2, _>(&mut frame, |slots, bridge| unsafe {
                assert_eq!(
                    (bridge.functions.helpers[3])(bridge.control, 1, 0, 0, 7),
                    abi::HELPER_DECLINED
                );
                assert_eq!(slots.add(1).read().load().unwrap().bits, 0);
                assert!(slots.read().store_numeric(Slot {
                    tag: INTEGER,
                    bits: 42
                }));
                assert_eq!(
                    (bridge.functions.helpers[0])(bridge.control, 1, 0, 0, 8),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(slots.add(1).read().load().unwrap().bits, 42);
            })
            .unwrap();
            assert_eq!(counters(&frame.count), [2, 1, 1, 0, 0, 0, 0, 0]);
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 9);
        assert_eq!(scalar(values[0]), (INTEGER, 42));
        assert_eq!(scalar(values[1]), (INTEGER, 42));
    });
}
