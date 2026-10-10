use super::*;

#[test]
fn scalar_changes_keep_sibling_descriptors_live_and_refusals_untouched() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let table = crate::Table::new(&ctx);
        let kinds = [
            Value::Nil,
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::Table(table),
        ];
        for count in [0, 1, 7, 255, 256] {
            let initial: Vec<_> = (0..count).map(|index| kinds[index % kinds.len()]).collect();
            for target in [0, count / 2, count.saturating_sub(1)] {
                let mut values = initial.clone();
                let mut pc = 11;
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
                    with_session(&mut frame, |host, slots| unsafe {
                        for value in kinds.into_iter().take(4).chain([Value::Integer(i64::MAX)]) {
                            let expected = Slot::from_value(value);
                            let saved: Vec<_> =
                                (0..count).map(|index| slots.add(index).read()).collect();
                            let result = materialize(
                                host,
                                slots,
                                target as u32,
                                expected.tag,
                                expected.bits,
                            );
                            assert_eq!(
                                result,
                                if count == 0 {
                                    abi::HELPER_DECLINED
                                } else {
                                    abi::HELPER_COMPLETED
                                }
                            );
                            for (index, previous) in saved.into_iter().enumerate() {
                                if index == target {
                                    let actual = load(slots.add(index).read()).unwrap();
                                    assert_eq!(
                                        (actual.tag, actual.bits),
                                        (expected.tag, expected.bits)
                                    );
                                    continue;
                                }
                                let current = slots.add(index).read();
                                assert_eq!(
                                    (current.tag, current.pointer),
                                    (previous.tag, previous.pointer)
                                );
                                if let Some(value) = load(previous) {
                                    assert!(store(previous, value));
                                    let actual = load(current).unwrap();
                                    assert_eq!((actual.tag, actual.bits), (value.tag, value.bits));
                                }
                            }
                        }
                        let saved: Vec<_> =
                            (0..count).map(|index| slots.add(index).read()).collect();
                        for (index, tag, bits) in [
                            (count as u32, INTEGER, 0),
                            (u32::MAX, NIL, 0),
                            (0, BOOLEAN, 2),
                            (0, REFERENCE, 0),
                            (0, u64::MAX, 0),
                        ] {
                            assert_eq!(
                                materialize(host, slots, index, tag, bits),
                                abi::HELPER_DECLINED
                            );
                            for (index, previous) in saved.iter().copied().enumerate() {
                                let current = slots.add(index).read();
                                assert_eq!(
                                    (current.tag, current.pointer),
                                    (previous.tag, previous.pointer)
                                );
                                if let Some(value) = load(previous) {
                                    assert!(store(previous, value));
                                }
                            }
                        }
                        if count != 0 {
                            assert_eq!(
                                call::<{ abi::HELPER_MOVE }>(
                                    host,
                                    slots,
                                    target as u32,
                                    target as u32,
                                    0,
                                    19
                                ),
                                abi::HELPER_COMPLETED
                            );
                            assert_eq!(
                                materialize(
                                    host,
                                    slots,
                                    target as u32,
                                    NUMBER,
                                    (-0.0f64).to_bits()
                                ),
                                abi::HELPER_COMPLETED
                            );
                        }
                    });
                    assert_eq!(frame.count.calls, u64::from(count != 0));
                    assert!(frame.panic.is_none());
                });
                for (index, (actual, original)) in
                    values.into_iter().zip(initial.iter().copied()).enumerate()
                {
                    let expected = if index == target {
                        Value::Number(-0.0)
                    } else {
                        original
                    };
                    let (actual_slot, expected_slot) =
                        (Slot::from_value(actual), Slot::from_value(expected));
                    assert_eq!(
                        (actual_slot.tag, actual_slot.bits),
                        (expected_slot.tag, expected_slot.bits)
                    );
                    if let Value::Table(expected) = expected {
                        assert!(matches!(actual, Value::Table(actual) if actual == expected));
                    }
                }
                assert_eq!(pc, if count == 0 { 11 } else { 20 });
            }
        }
    });
}

#[test]
fn initially_stopped_sessions_refuse_materialization_and_helpers() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let mut values = [Value::Integer(42)];
        let mut pc = 7;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 1,
                panic: Some(Box::new("stopped")),
                projection: None,
            };
            with_session(&mut frame, |host, slots| unsafe {
                assert_eq!(materialize(host, slots, 0, NIL, 0), abi::HELPER_PANICKED);
                assert_eq!(
                    call::<{ abi::HELPER_MOVE }>(host, slots, 0, 0, 0, 99),
                    abi::HELPER_PANICKED
                );
            });
            assert_eq!(frame.count.calls, 0);
            assert_eq!(
                frame.panic.unwrap().downcast_ref::<&str>(),
                Some(&"stopped")
            );
        });
        assert!(matches!(values[0], Value::Integer(42)));
        assert_eq!(pc, 7);
    });
}
