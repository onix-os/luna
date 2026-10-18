use super::*;

fn integer(value: Value<'_>) -> i64 {
    match value {
        Value::Integer(value) => value,
        _ => panic!("expected integer, got {value:?}"),
    }
}

#[test]
fn split_boundary_preserves_capture_and_close_coordinates() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"return"[..]).unwrap();
        for base in 0..=6 {
            let thread = Thread::new(ctx);
            let mut state = thread.0.borrow_mut(&ctx);
            let storage = state.stack;
            let mut stack = storage.borrow_mut(&ctx);
            stack.extend((0..6).map(Value::Integer));
            state.frames.push(Frame::Lua {
                closure,
                bottom: 0,
                base,
                is_variable: false,
                pc: 17,
                stack_size: 6 - base,
                expected_return: None,
            });
            let captures: Vec<_> = (0..6)
                .map(|stack_index| {
                    UpValue::new(
                        &ctx,
                        UpValueState::Open(OpenUpValue {
                            stack: Gc::downgrade(storage),
                            stack_index,
                        }),
                    )
                })
                .collect();
            state.open_upvalues.extend(captures.iter().copied());
            state.to_be_closed.extend(0..base);
            let mut fuel = Fuel::with(31);
            fuel.interrupt();
            let mut frame = LuaFrame {
                #[cfg(all(
                    feature = "jit",
                    not(miri),
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ))]
                pair_handoff: None,
                state: &mut state,
                #[cfg(feature = "jit")]
                stack: &mut stack,
                #[cfg(not(feature = "jit"))]
                stack,
                fuel: &mut fuel,
            };
            {
                let mut registers = frame.registers();
                assert_eq!(registers.upper_stack.len(), base);
                assert_eq!(registers.stack_frame.len(), 6 - base);
                #[cfg(feature = "jit")]
                assert_eq!(registers.shadow_key(), (Gc::as_ptr(storage) as usize, base));
                for (index, capture) in captures.iter().copied().enumerate() {
                    let mut read_at = None;
                    let value = registers.get_upvalue_with(&ctx, capture, |local, value| {
                        read_at = Some(local);
                        value
                    });
                    assert_eq!(integer(value), index as i64);
                    assert_eq!(read_at, (index >= base).then(|| index - base));
                    let mut wrote_at = None;
                    registers.set_upvalue_with(
                        &ctx,
                        capture,
                        Value::Integer(index as i64 + 100),
                        |local| wrote_at = Some(local),
                    );
                    assert_eq!(wrote_at, read_at);
                    assert_eq!(
                        integer(registers.get_upvalue(&ctx, capture)),
                        index as i64 + 100
                    );
                    if index >= base {
                        let register = RegisterIndex((index - base) as u8);
                        assert!(Gc::ptr_eq(
                            registers.open_upvalue(&ctx, register).into_inner(),
                            capture.into_inner()
                        ));
                        registers.mark_to_be_closed(register);
                    }
                }
                let closed = registers.take_to_be_closed(RegisterIndex(0));
                assert_eq!(
                    closed.into_iter().map(integer).collect::<Vec<_>>(),
                    (base..6).map(|i| i as i64 + 100).collect::<Vec<_>>()
                );
                registers.close_upvalues(&ctx, RegisterIndex(0));
                assert_eq!(*registers.pc, 17);
            }
            assert_eq!(frame.state.open_upvalues.len(), base);
            assert_eq!(
                &frame.state.to_be_closed[..],
                &(0..base).collect::<Vec<_>>()
            );
            for (index, capture) in captures.iter().copied().enumerate() {
                match capture.get() {
                    UpValueState::Open(open) => {
                        assert!(index < base);
                        assert_eq!(open.stack_index, index);
                    }
                    UpValueState::Closed(value) => {
                        assert!(index >= base);
                        assert_eq!(integer(value), index as i64 + 100);
                    }
                }
            }
            assert_eq!(frame.fuel.remaining(), 31);
            assert!(frame.fuel.is_interrupted());
        }
    });
}
