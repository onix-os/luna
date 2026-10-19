use super::*;
use closed_table_tests::identical;

#[test]
fn sparse_binding_upvalue_alias_reads_canonical_omitted_cells() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let mut values = [
            Value::Integer(42),
            Value::Boolean(false),
            Value::Number(-0.0),
        ];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let cell = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            closure.set_upvalue(&ctx, 0, cell);
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
            unsafe {
                with_binding_mask::<3>(&mut frame, [4, 0, 0, 0], |binding| {
                    let slots = binding.slots.as_ptr();
                    let host: *mut () = binding.host.base.data.cast();
                    let sibling = slots.add(2).read();
                    for (dest, pc) in [(1, 7), (0, 8)] {
                        assert_eq!(
                            call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, dest, 0, 0, pc),
                            abi::HELPER_COMPLETED
                        );
                        assert_eq!(load(slots.add(dest as usize).read()).unwrap().bits, 42);
                        assert!(store(
                            sibling,
                            Slot {
                                tag: NUMBER,
                                bits: (-0.0f64).to_bits()
                            }
                        ));
                    }
                });
            }
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.completed,
                    frame.count.upvalue_reads
                ),
                (2, 2, 2)
            );
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 9);
        for (actual, expected) in
            values
                .into_iter()
                .zip([Value::Integer(42), Value::Integer(42), Value::Number(-0.0)])
        {
            identical(actual, expected);
        }
    });
}

#[test]
fn sparse_binding_preserves_canonical_reads_all_kinds_and_prefixes() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"sparse")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for count in [0, 1, 7, 65, 256] {
            for mask in [[0; 4], [u64::MAX; 4], [0x5555555555555555; 4]] {
                let mut values: Vec<_> = (0..count + 1).map(|i| kinds[i % kinds.len()]).collect();
                let before = values.clone();
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
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
                    unsafe {
                        with_binding_mask::<256>(&mut frame, mask, |binding| {
                            let slots = binding.slots.as_ptr();
                            let session =
                                &*binding.host.base.data.cast::<Session<'_, '_, '_, '_, '_>>();
                            for index in 0..count {
                                let descriptor = slots.add(index).read();
                                if mask[index / 64] & (1 << (index % 64)) == 0 {
                                    assert_eq!(descriptor.tag, REFERENCE);
                                    assert!(descriptor.pointer.is_null());
                                }
                                identical(
                                    table_operand(session, slots, index as u32).unwrap(),
                                    before[index],
                                );
                            }
                        });
                    }
                    assert_eq!(frame.count.calls, 0);
                    assert!(frame.panic.is_none());
                });
                assert_eq!(pc, 0);
                for (actual, expected) in values.into_iter().zip(before) {
                    identical(actual, expected);
                }
            }
        }
    });
}

#[test]
fn sparse_binding_refreshes_destinations_preserves_siblings_and_full_helpers() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let mut values = [Value::Integer(7), Value::Integer(11), Value::Number(-0.0)];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
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
            unsafe {
                with_binding_mask::<3>(&mut frame, [6, 0, 0, 0], |binding| {
                    let slots = binding.slots.as_ptr();
                    let host: *mut () = binding.host.base.data.cast();
                    let sibling = slots.add(1).read();
                    assert_eq!(
                        materialize(host, slots, 0, NUMBER, (-0.0f64).to_bits()),
                        abi::HELPER_COMPLETED
                    );
                    assert!(store(
                        sibling,
                        Slot {
                            tag: INTEGER,
                            bits: 99
                        }
                    ));
                    assert_eq!(load(slots.read()).unwrap().bits, (-0.0f64).to_bits());
                    assert_eq!(
                        materialize_terminal(host, slots, 0, INTEGER, 42),
                        abi::HELPER_COMPLETED
                    );
                    identical(
                        table_operand(&*host.cast::<Session<'_, '_, '_, '_, '_>>(), slots, 0)
                            .unwrap(),
                        Value::Integer(42),
                    );
                    assert_eq!(
                        call::<{ abi::HELPER_MOVE }>(host, slots, 2, 0, 0, 7),
                        abi::HELPER_COMPLETED
                    );
                    for (index, bits) in [42, 99, 42].into_iter().enumerate() {
                        assert_eq!(load(slots.add(index).read()).unwrap().bits, bits);
                    }
                });
            }
            assert_eq!((frame.count.calls, frame.count.completed), (1, 1));
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 8);
        for (actual, expected) in values.into_iter().zip([42, 99, 42]) {
            identical(actual, Value::Integer(expected));
        }
    });
}

#[test]
fn sparse_binding_preserves_unwind_payload_writes_and_admission() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for count in [0, 1, 2, 3] {
            let mut values = [Value::Integer(17); 3];
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
                    read_hints: None,
                    projection: None,
                };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    with_binding_mask::<2>(&mut frame, [0; 4], |binding| {
                        entered = true;
                        if count > 0 {
                            assert_eq!(
                                materialize(
                                    binding.host.base.data.cast(),
                                    binding.slots.as_ptr(),
                                    0,
                                    INTEGER,
                                    99
                                ),
                                abi::HELPER_COMPLETED
                            );
                        }
                        std::panic::panic_any(73u32);
                    });
                }));
                assert!(outcome.is_err());
                assert_eq!(entered, count <= 2);
                if entered {
                    assert_eq!(outcome.unwrap_err().downcast_ref::<u32>(), Some(&73));
                }
                assert!(frame.panic.is_none());
            });
            assert_eq!(pc, 5);
            identical(
                values[0],
                Value::Integer(if entered && count > 0 { 99 } else { 17 }),
            );
            for value in &values[1..] {
                identical(*value, Value::Integer(17));
            }
        }
    });
}
