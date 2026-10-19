use super::*;

fn counts() -> helpers::Counts {
    helpers::Counts {
        calls: 11,
        completed: 12,
        declined: 13,
        table_reads: 14,
        table_writes: 15,
        upvalue_reads: 16,
        upvalue_writes: 17,
        allocations: 18,
    }
}

fn check_counts(counts: &helpers::Counts) {
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
        [11, 12, 13, 14, 15, 16, 17, 18]
    );
}

#[cfg(not(miri))]
pub(super) fn check_native(
    entry: unsafe extern "C" fn(*mut Payload, *mut Bridge, u32),
    reference_fallback: bool,
) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"native copy")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for condition in [0, 1] {
            for source in kinds {
                for dest in kinds {
                    let reference = reference_fallback && Slot::from_value(source).tag == REFERENCE;
                    let mut values = [source, dest, Value::Integer(19)];
                    let mut expected = source;
                    let mut pc = 23;
                    LuaRegisters::with_test_native_frame(
                        ctx,
                        &mut pc,
                        &mut values,
                        |mut registers, mut _list| {
                            let mut frame = helpers::Frame {
                                list: Some(_list.reborrow()),
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: counts(),
                                slot_count: 3,
                                panic: None,
                                projection: None,
                            };
                            with_bridge::<256>(&mut frame, |slots, host| unsafe {
                                let saved = slots.read();
                                let sibling = slots.add(2).read();
                                if matches!(source, Value::Integer(_)) {
                                    assert!(store(
                                        saved,
                                        Slot {
                                            tag: INTEGER,
                                            bits: 41
                                        }
                                    ));
                                    expected = Value::Integer(41);
                                }
                                entry(slots, host, condition);
                                let slow = (*host).materializations != 0;
                                let same_scalar = Slot::from_value(source).tag
                                    == Slot::from_value(dest).tag
                                    && matches!(
                                        source,
                                        Value::Nil
                                            | Value::Boolean(_)
                                            | Value::Integer(_)
                                            | Value::Number(_)
                                    );
                                assert_eq!(slow, !same_scalar && !reference);
                                assert_eq!((*host).materializations, u64::from(slow));
                                if slow {
                                    check_retired_descriptor(slots.read());
                                    check_retired_descriptor(slots.add(1).read());
                                } else if !reference {
                                    if let Some(value) = load(saved) {
                                        assert!(store(saved, value));
                                    }
                                }
                                let sibling = if reference {
                                    slots.add(2).read()
                                } else {
                                    sibling
                                };
                                assert!(store(
                                    sibling,
                                    Slot {
                                        tag: INTEGER,
                                        bits: 29
                                    }
                                ));
                                assert_eq!(load(slots.add(2).read()).unwrap().bits, 29);
                            });
                            if reference {
                                assert_eq!(frame.count.calls, 12);
                                assert_eq!(frame.count.completed, 13);
                                frame.count.calls -= 1;
                                frame.count.completed -= 1;
                            }
                            check_counts(&frame.count);
                            assert!(frame.panic.is_none());
                        },
                    );
                    assert_eq!(pc, if reference { 24 } else { 23 });
                    closed_table_tests::identical(values[0], expected);
                    closed_table_tests::identical(values[1], expected);
                    assert!(matches!(values[2], Value::Integer(29)));
                }
            }
        }
    });
}

#[test]
fn copies_all_values_without_invalidating_source_or_sibling_descriptors() {
    copy_cases::<false>();
}

#[test]
fn copies_retire_both_dead_descriptors_without_invalidating_other_siblings() {
    copy_cases::<true>();
}

fn copy_cases<const RETIRE_SOURCE: bool>() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"copy")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for count in [1, 7, 255, 256] {
            for source in [0, count - 1] {
                for dest in [0, count - 1] {
                    for value in kinds {
                        let mut values: Vec<_> =
                            (0..count).map(|i| kinds[i % kinds.len()]).collect();
                        values[source] = value;
                        let mut expected = values.clone();
                        expected[dest] = value;
                        let mut pc = 23;
                        LuaRegisters::with_test_native_frame(
                            ctx,
                            &mut pc,
                            &mut values,
                            |mut registers, mut _list| {
                                let mut frame = helpers::Frame {
                                    list: Some(_list.reborrow()),
                                    ctx,
                                    closure,
                                    registers: &mut registers,
                                    count: counts(),
                                    slot_count: count,
                                    panic: None,
                                    projection: None,
                                };
                                with_session(&mut frame, |host, slots| unsafe {
                                    let saved: Vec<_> =
                                        (0..count).map(|i| slots.add(i).read()).collect();
                                    if source != dest && matches!(value, Value::Integer(_)) {
                                        assert!(store(
                                            saved[source],
                                            Slot {
                                                tag: INTEGER,
                                                bits: 41
                                            }
                                        ));
                                        expected[source] = Value::Integer(41);
                                        expected[dest] = Value::Integer(41);
                                    }
                                    assert_eq!(
                                        copy_terminal::<RETIRE_SOURCE>(
                                            host,
                                            slots,
                                            dest as u32,
                                            source as u64
                                        ),
                                        abi::HELPER_COMPLETED
                                    );
                                    for (i, previous) in saved.iter().copied().enumerate() {
                                        let current = slots.add(i).read();
                                        if i == dest || (RETIRE_SOURCE && i == source) {
                                            assert_eq!(current.tag, NIL);
                                            assert!(current.pointer.is_null());
                                        } else {
                                            assert_eq!(
                                                (current.tag, current.pointer),
                                                (previous.tag, previous.pointer)
                                            );
                                            if let Some(value) = load(previous) {
                                                assert!(store(previous, value));
                                                let actual = load(current).unwrap();
                                                assert_eq!(
                                                    (actual.tag, actual.bits),
                                                    (value.tag, value.bits)
                                                );
                                            }
                                        }
                                    }
                                });
                                check_counts(&frame.count);
                                assert!(frame.panic.is_none());
                            },
                        );
                        assert_eq!(pc, 23);
                        for (actual, expected) in values.into_iter().zip(expected) {
                            closed_table_tests::identical(actual, expected);
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn stopped_and_invalid_copies_leave_canonical_values_and_descriptors_unchanged() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for count in [0, 1, 7, 256] {
            for stopped in [false, true] {
                for (source, dest) in [(0, 0), (count, 0), (0, count), (usize::MAX, usize::MAX)] {
                    if !stopped && source < count && dest < count {
                        continue;
                    }
                    let mut values = vec![Value::Integer(42); count];
                    let mut pc = 19;
                    LuaRegisters::with_test_native_frame(
                        ctx,
                        &mut pc,
                        &mut values,
                        |mut registers, mut _list| {
                            let mut frame = helpers::Frame {
                                list: Some(_list.reborrow()),
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: counts(),
                                slot_count: count,
                                panic: stopped
                                    .then(|| Box::new(71u64) as Box<dyn std::any::Any + Send>),
                                projection: None,
                            };
                            with_session(&mut frame, |host, slots| unsafe {
                                let saved: Vec<_> =
                                    (0..count).map(|i| slots.add(i).read()).collect();
                                let expected = if stopped {
                                    abi::HELPER_PANICKED
                                } else {
                                    abi::HELPER_DECLINED
                                };
                                assert_eq!(
                                    copy_terminal::<false>(host, slots, dest as u32, source as u64),
                                    expected
                                );
                                assert_eq!(
                                    copy_terminal::<true>(host, slots, dest as u32, source as u64),
                                    expected
                                );
                                closed_table_tests::siblings(slots, &saved, None);
                            });
                            check_counts(&frame.count);
                            if stopped {
                                assert_eq!(*frame.panic.unwrap().downcast::<u64>().unwrap(), 71);
                            }
                        },
                    );
                    assert_eq!(pc, 19);
                    assert!(values
                        .iter()
                        .all(|value| matches!(value, Value::Integer(42))));
                }
            }
        }
    });
}
