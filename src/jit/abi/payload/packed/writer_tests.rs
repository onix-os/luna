use super::*;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "writer_native.rs"]
mod native;

#[path = "writer_sequence.rs"]
mod sequence;

fn model(_: *mut Packed, bridge: &Bridge, terminal: bool, index: u32, tag: u64, bits: u64) -> u32 {
    let writer = if terminal {
        bridge.functions.terminal
    } else {
        bridge.functions.write
    };
    unsafe { writer(bridge.control, index, tag, bits) }
}

pub(super) fn same_value<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::String(a), Value::String(b)) => {
            assert!(ottavino_gc_arena::Gc::ptr_eq(
                a.into_inner(),
                b.into_inner()
            ));
        }
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        (a, b) => {
            assert_ne!(scalar(a).0, REFERENCE);
            assert_eq!(scalar(a), scalar(b));
        }
    }
}

pub(super) fn scalar(value: Value<'_>) -> (u64, u64) {
    let slot = Slot::from_value(value);
    (slot.tag, slot.bits)
}

pub(super) fn inputs() -> [(u64, u64); 16] {
    [
        (NIL, 0),
        (NIL, u64::MAX),
        (BOOLEAN, 0),
        (BOOLEAN, 1),
        (BOOLEAN, 2),
        (BOOLEAN, u64::MAX),
        (INTEGER, i64::MIN as u64),
        (INTEGER, i64::MAX as u64),
        (INTEGER, u64::MAX),
        (NUMBER, (-0.0f64).to_bits()),
        (NUMBER, f64::INFINITY.to_bits()),
        (NUMBER, 0x7ff8000000000042),
        (NUMBER, 0xfff0000000000042),
        (REFERENCE, 0),
        (6, u64::MAX),
        (u64::MAX, 0),
    ]
}

pub(super) fn expected(tag: u64, bits: u64) -> Option<(u64, u64)> {
    match tag {
        NIL => Some((NIL, 0)),
        BOOLEAN if bits <= 1 => Some((BOOLEAN, bits)),
        INTEGER | NUMBER => Some((tag, bits)),
        _ => None,
    }
}

#[test]
fn packed_scalar_writers_preserve_bits_identity_and_live_siblings() {
    check_kinds(model);
}

fn check_kinds(entry: impl Fn(*mut Packed, &Bridge, bool, u32, u64, u64) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for initial in kinds(ctx) {
            for (tag, bits) in inputs() {
                for terminal in [false, true] {
                    let mut values = [initial, Value::Integer(123), Value::Number(-0.0)];
                    let mut pc = 7;
                    let mut fuel = crate::Fuel::with(47);
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
                                slot_count: 3,
                                panic: None,
                                read_hints: None,
                                projection: None,
                            };
                            with_bridge::<3, _>(&mut frame, |slots, bridge| unsafe {
                                let sibling_integer = slots.add(1).read();
                                let sibling_number = slots.add(2).read();
                                let original = slots.read();
                                let result = entry(slots, bridge, terminal, 0, tag, bits);
                                let bound =
                                    &*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 3>>();
                                assert_eq!(bound.materializations, 1);
                                if let Some(value) = expected(tag, bits) {
                                    assert_eq!(result, abi::HELPER_COMPLETED);
                                    assert_eq!(scalar(bound.scope.read(0).unwrap()), value);
                                    if terminal {
                                        assert_eq!(slots.read().0.addr(), EMPTY);
                                    } else {
                                        let loaded = slots.read().load().unwrap();
                                        assert_eq!((loaded.tag, loaded.bits), value);
                                    }
                                } else {
                                    assert_eq!(result, abi::HELPER_DECLINED);
                                    assert_eq!(slots.read().0, original.0);
                                    same_value(bound.scope.read(0).unwrap(), initial);
                                    if matches!(
                                        initial,
                                        Value::Table(_)
                                            | Value::Function(_)
                                            | Value::Thread(_)
                                            | Value::UserData(_)
                                            | Value::String(_)
                                    ) {
                                        assert_eq!(slots.read().tag(), Some(REFERENCE));
                                    }
                                }
                                assert!(sibling_integer.store_numeric(Slot {
                                    tag: INTEGER,
                                    bits: 731
                                }));
                                assert!(sibling_number.store_numeric(Slot {
                                    tag: NUMBER,
                                    bits: 42f64.to_bits()
                                }));
                                assert_eq!(slots.add(1).read().load().unwrap().bits, 731);
                                assert_eq!(
                                    slots.add(2).read().load().unwrap().bits,
                                    42f64.to_bits()
                                );
                            })
                            .unwrap();
                            assert_eq!(tests::counters(&frame.count), [0; 8]);
                            assert!(frame.panic.is_none());
                        },
                    );
                    assert_eq!(pc, 7);
                    assert_eq!(fuel.remaining(), 47);
                    assert_eq!(
                        scalar(values[0]),
                        expected(tag, bits).unwrap_or_else(|| scalar(initial))
                    );
                    if expected(tag, bits).is_none() {
                        same_value(values[0], initial);
                    }
                    assert!(matches!(values[1], Value::Integer(731)));
                    assert!(matches!(values[2], Value::Number(42.0)));
                }
            }
        }
    });
}

#[test]
fn packed_scalar_writer_refusals_leave_canonical_state_and_descriptors_unchanged() {
    check_bounds(model);
}

fn check_bounds(entry: impl Fn(*mut Packed, &Bridge, bool, u32, u64, u64) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for count in [0, 1, 7, 255, 256] {
            let mut values = vec![Value::Integer(17); count + 1];
            let mut pc = 5;
            crate::thread::LuaRegisters::with_test_frame(
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
                    with_bridge::<256, _>(&mut frame, |slots, bridge| unsafe {
                        let before: Vec<_> =
                            (0..count).map(|index| slots.add(index).read()).collect();
                        let mut attempts = 0;
                        for terminal in [false, true] {
                            for index in [count as u32, u32::MAX] {
                                assert_eq!(
                                    entry(slots, bridge, terminal, index, INTEGER, 42),
                                    abi::HELPER_DECLINED
                                );
                                attempts += 1;
                            }
                            assert_eq!(
                                entry(slots, bridge, terminal, 0, BOOLEAN, 2),
                                abi::HELPER_DECLINED
                            );
                            attempts += 1;
                        }
                        assert_eq!(
                            (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 256>>())
                                .materializations,
                            attempts
                        );
                        for (index, old) in before.into_iter().enumerate() {
                            assert_eq!(slots.add(index).read().0, old.0);
                            assert_eq!(old.load().unwrap().bits, 17);
                        }
                    })
                    .unwrap();
                    assert_eq!(frame.count.calls, 0);
                    assert!(frame.panic.is_none());
                },
            );
            assert_eq!(pc, 5);
            assert!(values
                .iter()
                .all(|value| matches!(value, Value::Integer(17))));
        }
    });
}

#[test]
fn packed_terminal_writes_refresh_through_helpers_and_later_writers() {
    check_reentry(model);
}

fn check_reentry(entry: impl Fn(*mut Packed, &Bridge, bool, u32, u64, u64) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for (tag, bits) in inputs()
            .into_iter()
            .filter(|&(tag, bits)| expected(tag, bits).is_some())
        {
            let mut values = [Value::Integer(17), Value::Nil];
            let mut pc = 5;
            crate::thread::LuaRegisters::with_test_frame(
                ctx,
                &mut pc,
                &mut values,
                |mut registers| {
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
                            entry(slots, bridge, true, 0, tag, bits),
                            abi::HELPER_COMPLETED
                        );
                        assert_eq!(slots.read().0.addr(), EMPTY);
                        assert_eq!(
                            (bridge.functions.helpers[0])(bridge.control, 1, 0, 0, 7),
                            abi::HELPER_COMPLETED
                        );
                        let actual = slots.add(1).read().load().unwrap();
                        assert_eq!((actual.tag, actual.bits), expected(tag, bits).unwrap());
                        assert_eq!(
                            entry(slots, bridge, false, 0, BOOLEAN, 1),
                            abi::HELPER_COMPLETED
                        );
                        assert_eq!(slots.read().0.addr(), TRUE);
                        assert_eq!(
                            entry(slots, bridge, false, 0, BOOLEAN, 0),
                            abi::HELPER_COMPLETED
                        );
                        assert_eq!(slots.read().0.addr(), FALSE);
                        assert_eq!(
                            (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 2>>())
                                .materializations,
                            3
                        );
                    })
                    .unwrap();
                    assert_eq!((frame.count.calls, frame.count.completed), (1, 1));
                    assert!(frame.panic.is_none());
                },
            );
            assert_eq!(pc, 8);
            assert!(matches!(values[0], Value::Boolean(false)));
            assert_eq!(scalar(values[1]), expected(tag, bits).unwrap());
        }
    });
}

#[test]
fn packed_scalar_writers_preserve_stopped_states_and_pending_panics() {
    check_stopped(model);
}

fn check_stopped(entry: impl Fn(*mut Packed, &Bridge, bool, u32, u64, u64) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for panic in [false, true] {
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
                        panic: panic.then(|| Box::new(731u64) as Box<dyn std::any::Any + Send>),
                        projection: None,
                    };
                    let result = with_bridge::<1, _>(&mut frame, |slots, bridge| unsafe {
                        let bound =
                            &mut *bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 1>>();
                        if !panic {
                            bound.scope.stopped = true;
                        }
                        for terminal in [false, true] {
                            assert_eq!(
                                entry(slots, bridge, terminal, 0, INTEGER, 42),
                                if panic { abi::HELPER_PANICKED } else { REFUSED }
                            );
                            assert_eq!(
                                (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 1>>())
                                    .materializations,
                                0
                            );
                        }
                    });
                    assert_eq!(result, if panic { Ok(()) } else { Err(Refusal::Stopped) });
                    if panic {
                        assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
                    } else {
                        assert!(frame.panic.is_none());
                    }
                    assert_eq!(frame.count.calls, 0);
                },
            );
            assert_eq!(pc, 5);
            assert!(matches!(values[0], Value::Integer(17)));
        }
    });
}
