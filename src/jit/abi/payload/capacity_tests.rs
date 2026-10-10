use super::*;

fn check<const CAPACITY: usize>() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for count in [0, CAPACITY.saturating_sub(1), CAPACITY, CAPACITY + 1] {
            let mut values = vec![Value::Integer(17); count + 1];
            let mut pc = 5;
            let mut entered = false;
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
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    with_bridge::<CAPACITY>(&mut frame, |slots, host| unsafe {
                        entered = true;
                        for index in 0..count {
                            assert_eq!(load(slots.add(index).read()).unwrap().bits, 17);
                        }
                        if count != 0 {
                            ((*host).write)(
                                host,
                                slots,
                                (count - 1) as u32,
                                NUMBER,
                                (-0.0f64).to_bits(),
                            );
                            assert_eq!(
                                load(slots.add(count - 1).read()).unwrap().bits,
                                (-0.0f64).to_bits()
                            );
                            assert_eq!(
                                ((*host).helpers[(abi::HELPER_MOVE - 1) as usize])(
                                    host.cast(),
                                    slots.cast(),
                                    0,
                                    (count - 1) as u32,
                                    0,
                                    7,
                                ),
                                abi::HELPER_COMPLETED,
                            );
                            assert_eq!(load(slots.read()).unwrap().bits, (-0.0f64).to_bits());
                        }
                        std::panic::panic_any(42u32);
                    });
                }));
                assert!(result.is_err());
                let admitted = CAPACITY <= 256 && count <= CAPACITY;
                assert_eq!(entered, admitted);
                if admitted {
                    assert_eq!(result.unwrap_err().downcast_ref::<u32>(), Some(&42));
                }
                let calls = u64::from(admitted && count != 0);
                assert_eq!((frame.count.calls, frame.count.completed), (calls, calls));
                assert!(frame.panic.is_none());
            });
            let changed = entered && count != 0;
            assert_eq!(pc, if changed { 8 } else { 5 });
            for (index, value) in values.iter().enumerate() {
                if changed && (index == 0 || index + 1 == count) {
                    assert!(
                        matches!(value, Value::Number(n) if n.to_bits() == (-0.0f64).to_bits())
                    );
                } else {
                    assert!(matches!(value, Value::Integer(17)));
                }
            }
        }
    });
}

#[test]
fn bounded_payload_storage_preserves_prefix_rebinding_and_unwind() {
    check::<0>();
    check::<1>();
    check::<2>();
    check::<3>();
    check::<4>();
    check::<5>();
    check::<6>();
    check::<7>();
    check::<8>();
    check::<16>();
    check::<32>();
    check::<64>();
    check::<128>();
    check::<256>();
    check::<257>();
}
