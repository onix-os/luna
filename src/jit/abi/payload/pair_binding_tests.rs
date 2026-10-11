use super::*;
use closed_table_tests::identical;

fn kinds(ctx: crate::Context<'_>) -> [Value<'_>; 14] {
    [
        Value::Nil,
        Value::Boolean(false),
        Value::Boolean(true),
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Number(-0.0),
        Value::Number(f64::INFINITY),
        Value::Number(f64::from_bits(0x7ff8000000000042)),
        Value::String(ctx.intern(b"pair")),
        Value::Table(crate::Table::new(&ctx)),
        Value::Function(
            crate::Closure::load(ctx, None, b"return 42")
                .unwrap()
                .into(),
        ),
        Value::Function(
            crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
        ),
        Value::Thread(crate::Thread::new(ctx)),
        Value::UserData(crate::UserData::new_static(&ctx, 19)),
    ]
}

#[test]
fn integer_pairs_preserve_exact_payloads_fallbacks_and_sibling_loans() {
    crate::Lua::empty().enter(|ctx| {
        for left in kinds(ctx) {
            for right in kinds(ctx) {
                let mut values = [Value::Integer(7), left, right, Value::Number(-0.0)];
                let mut frame = Frame::new(&mut values);
                let before = frame.bind(0).unwrap();
                let after = frame.bind(3).unwrap();
                let pair = frame.bind_integer_pair(1);
                let paired = matches!((left, right), (Value::Integer(_), Value::Integer(_)));
                assert_eq!(pair.is_some(), paired);
                let descriptors =
                    pair.unwrap_or_else(|| [frame.bind(1).unwrap(), frame.bind(2).unwrap()]);
                for (descriptor, value) in descriptors.into_iter().zip([left, right]) {
                    let expected = Slot::from_value(value);
                    assert_eq!(descriptor.tag, expected.tag);
                    if expected.tag == REFERENCE {
                        assert!(descriptor.pointer.is_null());
                        assert!(unsafe { load(descriptor) }.is_none());
                    } else {
                        let actual = unsafe { load(descriptor) }.unwrap();
                        assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                        assert!(unsafe { store(descriptor, expected) });
                    }
                }
                assert!(unsafe {
                    store(
                        before,
                        Slot {
                            tag: INTEGER,
                            bits: 29,
                        },
                    )
                });
                assert!(unsafe {
                    store(
                        after,
                        Slot {
                            tag: NUMBER,
                            bits: 0x7ff8000000000063,
                        },
                    )
                });
                assert_eq!(unsafe { load(before) }.unwrap().bits, 29);
                assert_eq!(unsafe { load(after) }.unwrap().bits, 0x7ff8000000000063);
                if paired {
                    for (offset, descriptor) in descriptors.into_iter().enumerate() {
                        let rebound = frame.bind(offset + 1).unwrap();
                        assert_eq!(descriptor.pointer, rebound.pointer);
                        assert_eq!(unsafe { load(rebound) }.unwrap().tag, INTEGER);
                    }
                }
                identical(values[0], Value::Integer(29));
                identical(values[1], left);
                identical(values[2], right);
                identical(values[3], Value::Number(f64::from_bits(0x7ff8000000000063)));
            }
        }
    });
}

#[test]
fn integer_pairs_reject_short_and_overflowing_ranges_without_reborrowing() {
    for count in [0, 1, 2, 3, 7, 8, 256] {
        let mut values = vec![Value::Integer(i64::MIN); count];
        let mut frame = Frame::new(&mut values);
        let slots: Vec<_> = (0..count).map(|index| frame.bind(index).unwrap()).collect();
        for index in [count.saturating_sub(1), count, usize::MAX - 1, usize::MAX] {
            assert!(frame.bind_integer_pair(index).is_none());
        }
        for slot in slots {
            assert!(unsafe {
                store(
                    slot,
                    Slot {
                        tag: INTEGER,
                        bits: i64::MAX as u64,
                    },
                )
            });
            assert_eq!(unsafe { load(slot) }.unwrap().bits, i64::MAX as u64);
        }
        for value in values {
            identical(value, Value::Integer(i64::MAX));
        }
    }
}

#[test]
fn full_binding_preserves_mixed_prefixes_and_reentry_after_type_changes() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for count in [0, 1, 2, 3, 7, 8, 256] {
            for (position, kind) in kinds(ctx).into_iter().enumerate() {
                let mut values = vec![Value::Integer(i64::MIN); count + 1];
                let dest = position % count.max(1);
                if count != 0 {
                    values[dest] = kind;
                }
                values[count] = Value::Number(-0.0);
                let mut expected = values.clone();
                let mut pc = 5;
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
                    for replacement in [Value::Number(-0.0), Value::Integer(i64::MAX)] {
                        with_binding::<256>(&mut frame, |binding| unsafe {
                            let slots = binding.slots.as_ptr();
                            for (index, value) in expected[..count].iter().copied().enumerate() {
                                let descriptor = slots.add(index).read();
                                let expected = Slot::from_value(value);
                                assert_eq!(descriptor.tag, expected.tag);
                                if expected.tag != REFERENCE {
                                    assert_eq!(load(descriptor).unwrap().bits, expected.bits);
                                } else {
                                    assert!(descriptor.pointer.is_null());
                                }
                            }
                            if count != 0 {
                                let scalar = Slot::from_value(replacement);
                                let host = binding.host;
                                (host.functions.write)(
                                    host,
                                    slots,
                                    dest as u32,
                                    scalar.tag,
                                    scalar.bits,
                                );
                                assert_eq!(load(slots.add(dest).read()).unwrap().bits, scalar.bits);
                                expected[dest] = replacement;
                            }
                        });
                        for (actual, expected) in
                            frame.registers.stack_frame.iter().copied().zip(&expected)
                        {
                            identical(actual, *expected);
                        }
                    }
                    assert_eq!((frame.count.calls, frame.count.completed), (0, 0));
                    assert!(frame.panic.is_none());
                });
                assert_eq!(pc, 5);
                for (actual, expected) in values.into_iter().zip(expected) {
                    identical(actual, expected);
                }
            }
        }
    });
}
