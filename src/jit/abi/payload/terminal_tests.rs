use super::*;

#[test]
fn terminal_materialization_retires_only_its_cell_and_refreshes_at_helpers() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Number(-0.0),
            Value::String(ctx.intern(b"terminal")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for count in [0usize, 1, 7, 255, 256] {
            for stopped in [false, true] {
                for target in [
                    0,
                    count / 2,
                    count.saturating_sub(1),
                    count,
                    u32::MAX as usize,
                ] {
                    for (case, (tag, bits)) in [
                        (NIL, 0),
                        (BOOLEAN, 1),
                        (BOOLEAN, 2),
                        (INTEGER, i64::MAX as u64),
                        (NUMBER, (-0.0f64).to_bits()),
                        (NUMBER, 0x7ff8000000000042),
                        (REFERENCE, 0),
                        (u64::MAX, 1),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let mut values: Vec<_> = (0..count)
                            .map(|i| kinds[(i + case) % kinds.len()])
                            .collect();
                        let mut expected = values.clone();
                        let accepted =
                            !stopped && target < count && matches!(tag, NIL | INTEGER | NUMBER)
                                || (!stopped && target < count && tag == BOOLEAN && bits <= 1);
                        if accepted {
                            expected[target] = Slot { tag, bits }.value(expected[target]);
                        }
                        let mut pc = 23;
                        LuaRegisters::with_test_frame(
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
                                    slot_count: count,
                                    panic: stopped.then(|| {
                                        Box::new("stopped") as Box<dyn std::any::Any + Send>
                                    }),
                                    projection: None,
                                };
                                with_session(&mut frame, |host, slots| unsafe {
                                    let saved: Vec<_> =
                                        (0..count).map(|i| slots.add(i).read()).collect();
                                    let status =
                                        materialize_terminal(host, slots, target as u32, tag, bits);
                                    assert_eq!(
                                        status,
                                        if stopped {
                                            abi::HELPER_PANICKED
                                        } else if accepted {
                                            abi::HELPER_COMPLETED
                                        } else {
                                            abi::HELPER_DECLINED
                                        }
                                    );
                                    for (i, previous) in saved.into_iter().enumerate() {
                                        let current = slots.add(i).read();
                                        if accepted && i == target {
                                            assert_eq!(current.tag, NIL);
                                            assert!(current.pointer.is_null());
                                        } else {
                                            assert_eq!(
                                                (current.tag, current.pointer),
                                                (previous.tag, previous.pointer)
                                            );
                                            let old = load(previous).map(|v| (v.tag, v.bits));
                                            assert_eq!(load(current).map(|v| (v.tag, v.bits)), old);
                                        }
                                    }
                                    let session = &*host.cast::<Session<'_, '_, '_, '_, '_>>();
                                    assert_eq!(*session.pc.as_ptr(), 23);
                                    let counts = session.counts.as_ref();
                                    assert_eq!(
                                        [
                                            counts.calls,
                                            counts.completed,
                                            counts.declined,
                                            counts.table_reads,
                                            counts.table_writes,
                                            counts.upvalue_reads,
                                            counts.upvalue_writes,
                                            counts.allocations
                                        ],
                                        [0; 8]
                                    );
                                    if !stopped && count != 0 {
                                        assert_eq!(
                                            call::<{ abi::HELPER_MOVE }>(host, slots, 0, 0, 0, 23),
                                            abi::HELPER_COMPLETED
                                        );
                                        for (i, value) in expected.iter().copied().enumerate() {
                                            let slot = load(slots.add(i).read());
                                            let expected = Slot::from_value(value);
                                            assert_eq!(
                                                slot.map(|slot| (slot.tag, slot.bits)),
                                                (expected.tag != REFERENCE)
                                                    .then_some((expected.tag, expected.bits))
                                            );
                                        }
                                    }
                                });
                                assert_eq!(frame.count.calls, u64::from(!stopped && count != 0));
                                assert_eq!(frame.count.completed, frame.count.calls);
                                assert_eq!(frame.panic.is_some(), stopped);
                            },
                        );
                        assert_eq!(pc, if !stopped && count != 0 { 24 } else { 23 });
                        for (actual, expected) in values.into_iter().zip(expected) {
                            closed_table_tests::identical(actual, expected);
                        }
                    }
                }
            }
        }
    });
}
