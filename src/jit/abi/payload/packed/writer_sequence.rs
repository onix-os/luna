use super::*;

fn model(slots: *mut Packed, bridge: &Bridge, terminal: bool, tag: u64, bits: u64) -> u32 {
    let refused = super::model(slots, bridge, terminal, 0, BOOLEAN, 2);
    if refused != abi::HELPER_DECLINED {
        return refused;
    }
    let written = super::model(slots, bridge, terminal, 0, tag, bits);
    if written != abi::HELPER_COMPLETED {
        return written;
    }
    unsafe {
        let moved = (bridge.functions.helpers[0])(bridge.control, 1, 0, 0, 7);
        if moved == abi::HELPER_COMPLETED {
            let sibling = slots.add(2).read();
            assert!(sibling.store_numeric(Slot {
                tag: INTEGER,
                bits: sibling.load().unwrap().bits.wrapping_add(1),
            }));
        }
        moved
    }
}

pub(super) fn check(entry: impl Fn(*mut Packed, &Bridge, bool, u64, u64) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let original = Value::Table(crate::Table::new(&ctx));
        for (tag, bits) in inputs() {
            for terminal in [false, true] {
                for stopped in 0..3 {
                    let mut values = [original, Value::Boolean(true), Value::Integer(73)];
                    let mut pc = 5;
                    let mut fuel = crate::Fuel::with(47);
                    let completed = stopped == 0 && expected(tag, bits).is_some();
                    crate::thread::LuaRegisters::with_test_frame_state(
                        ctx,
                        &mut pc,
                        &mut values,
                        &mut fuel,
                        false,
                        |mut registers| {
                            let mut frame = helpers::Frame {
                                read_hints: None,
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: helpers::Counts::default(),
                                slot_count: 3,
                                panic: (stopped == 2)
                                    .then(|| Box::new(731u64) as Box<dyn std::any::Any + Send>),
                                projection: None,
                            };
                            let result = with_bridge::<3, _>(&mut frame, |slots, bridge| {
                                if stopped == 1 {
                                    unsafe {
                                        (*bridge
                                            .control
                                            .cast::<FrameScope<'_, '_, '_, '_, '_, 3>>())
                                        .scope
                                        .stopped = true
                                    };
                                }
                                let result = entry(slots, bridge, terminal, tag, bits);
                                assert_eq!(
                                    result,
                                    match stopped {
                                        1 => REFUSED,
                                        2 => abi::HELPER_PANICKED,
                                        _ if completed => abi::HELPER_COMPLETED,
                                        _ => abi::HELPER_DECLINED,
                                    }
                                );
                                assert_eq!(
                                    unsafe {
                                        (*bridge
                                            .control
                                            .cast::<FrameScope<'_, '_, '_, '_, '_, 3>>())
                                        .materializations
                                    },
                                    if stopped == 0 { 2 } else { 0 }
                                );
                                if completed {
                                    let destination =
                                        unsafe { slots.add(1).read().load().unwrap() };
                                    assert_eq!(
                                        (destination.tag, destination.bits),
                                        expected(tag, bits).unwrap()
                                    );
                                }
                            });
                            assert_eq!(
                                result,
                                if stopped == 1 {
                                    Err(Refusal::Stopped)
                                } else {
                                    Ok(())
                                }
                            );
                            assert_eq!(
                                tests::counters(&frame.count),
                                [u64::from(completed), u64::from(completed), 0, 0, 0, 0, 0, 0]
                            );
                            if stopped == 2 {
                                assert_eq!(
                                    *frame.panic.take().unwrap().downcast::<u64>().unwrap(),
                                    731
                                );
                            } else {
                                assert!(frame.panic.is_none());
                            }
                        },
                    );
                    assert_eq!(pc, if completed { 8 } else { 5 });
                    assert_eq!(fuel.remaining(), 47);
                    if completed {
                        assert_eq!(scalar(values[0]), expected(tag, bits).unwrap());
                        assert_eq!(scalar(values[1]), expected(tag, bits).unwrap());
                    } else {
                        same_value(values[0], original);
                        same_value(values[1], Value::Boolean(true));
                    }
                    same_value(values[2], Value::Integer(if completed { 74 } else { 73 }));
                }
            }
        }
    });
}

#[test]
fn packed_writer_sequence_recovers_after_decline_and_refreshes_before_numeric_access() {
    check(model);
}
