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
                            count: helpers::Counts::default(),
                            slot_count: count,
                            panic: None,
                            projection: None,
                        };
                        for replacement in [Value::Number(-0.0), Value::Integer(i64::MAX)] {
                            with_binding::<256>(&mut frame, |binding| unsafe {
                                let slots = binding.slots.as_ptr();
                                for (index, value) in expected[..count].iter().copied().enumerate()
                                {
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
                                    assert_eq!(
                                        load(slots.add(dest).read()).unwrap().bits,
                                        scalar.bits
                                    );
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
                    },
                );
                assert_eq!(pc, 5);
                for (actual, expected) in values.into_iter().zip(expected) {
                    identical(actual, expected);
                }
            }
        }
    });
}
