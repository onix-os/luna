use super::*;

#[cfg(feature = "jit")]
#[test]
fn direct_capture_matches_register_projection_without_mutation() {
    use crate::{jit::projection::Origin, thread::activation::ActivationHost};
    use std::cell::Cell;

    crate::Lua::empty().enter(|ctx| {
        let caller = Closure::load(ctx, None, &b"return"[..]).unwrap();
        let other = Closure::load(ctx, None, &b"return 1"[..]).unwrap();
        let foreign = Thread::new(ctx).0.borrow().stack;
        for base in [0, 1, 4, 6] {
            let thread = Thread::new(ctx);
            let mut state = thread.0.borrow_mut(&ctx);
            let storage = state.stack;
            let mut stack = storage.borrow_mut(&ctx);
            stack.extend([
                Value::Integer(10),
                Value::Boolean(false),
                Value::Integer(30),
                Value::Number(2.5),
                Value::Nil,
                Value::Integer(60),
            ]);
            state.frames.push(Frame::Lua {
                closure: caller,
                bottom: 0,
                base,
                pc: 0,
                is_variable: false,
                stack_size: 6 - base,
                expected_return: None,
            });
            let mut fuel = Fuel::with(17);
            let mut host = ActivationHost::new(&mut state, stack, &mut fuel);
            for index in [0, base.saturating_sub(1), base, 5, 6, usize::MAX] {
                for source in [
                    UpValueState::Open(OpenUpValue {
                        stack: Gc::downgrade(storage),
                        stack_index: index,
                    }),
                    UpValueState::Open(OpenUpValue {
                        stack: Gc::downgrade(foreign),
                        stack_index: index,
                    }),
                    UpValueState::Closed(Value::Integer(77)),
                    UpValueState::Closed(Value::Nil),
                ] {
                    let upvalue = UpValue::new(&ctx, source);
                    for pending_len in [0, 1, 6 - base, 8] {
                        for pending_value in [
                            Value::Integer(-91),
                            Value::Nil,
                            Value::Number(0.5),
                            Value::String(ctx.intern(b"pending")),
                        ] {
                            for expected_caller in [caller, other] {
                                let expected_reads = Cell::new(0);
                                let actual_reads = Cell::new(0);
                                let expected = host.with_registers(|found, registers| {
                                    if found != expected_caller {
                                        return None;
                                    }
                                    match registers.projection_origin(upvalue)? {
                                        Origin::Upper(index, Value::Integer(value)) => {
                                            Some(((true, index), value))
                                        }
                                        Origin::Register(index, _) => {
                                            expected_reads.set(expected_reads.get() + 1);
                                            if index >= pending_len {
                                                return None;
                                            }
                                            let Value::Integer(value) = pending_value else {
                                                return None;
                                            };
                                            Some(((false, index), value))
                                        }
                                        _ => None,
                                    }
                                });
                                let actual =
                                    host.snapshot_capture(expected_caller, upvalue, |index| {
                                        actual_reads.set(actual_reads.get() + 1);
                                        (index < pending_len).then_some(pending_value)
                                    });
                                assert_eq!(
                                    actual, expected,
                                    "base={base}, index={index}, pending_len={pending_len}"
                                );
                                assert_eq!(actual_reads.get(), expected_reads.get());
                                host.with_registers(|found, registers| {
                                    assert_eq!(found, caller);
                                    assert_eq!(*registers.pc, 0);
                                    assert_eq!(registers.stack_frame.len(), 6 - base);
                                    for (local, value) in registers.stack_frame.iter().enumerate() {
                                        let absolute = local + base;
                                        assert_eq!(
                                            value.type_name(),
                                            [
                                                "number", "boolean", "number", "number", "nil",
                                                "number"
                                            ][absolute]
                                        );
                                        if let Value::Integer(value) = value {
                                            assert_eq!(*value, [10, 0, 30, 0, 0, 60][absolute]);
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
            }
            drop(host);
            assert_eq!(fuel.remaining(), 17);
        }
    });
}

#[test]
fn return_spans_preserve_bounds_fuel_and_capture_closing() {
    for bottom in [0, 2, 6, 7] {
        for start in [0, 4, 5] {
            for count in [0, 1, 3] {
                for available in [i32::MIN, 0, 100] {
                    crate::Lua::empty().enter(|ctx| {
                        let closure = Closure::load(ctx, None, &b"return"[..]).unwrap();
                        let thread = Thread::new(ctx);
                        let mut state = thread.0.borrow_mut(&ctx);
                        let storage = state.stack;
                        let mut stack = storage.borrow_mut(&ctx);
                        stack.extend((0..6).map(Value::Integer));
                        state.frames.push(Frame::Lua {
                            closure,
                            bottom,
                            base: 2,
                            is_variable: false,
                            pc: 0,
                            stack_size: 4,
                            expected_return: None,
                        });
                        let capture = UpValue::new(
                            &ctx,
                            UpValueState::Open(OpenUpValue {
                                stack: Gc::downgrade(storage),
                                stack_index: 3,
                            }),
                        );
                        state.open_upvalues.push(capture);
                        let mut fuel = Fuel::with(available);
                        fuel.interrupt();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            LuaFrame {
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
                            }
                            .return_upper(&ctx, RegisterIndex(start), VarCount::constant(count))
                            .unwrap();
                        }));
                        let start = 2 + usize::from(start);
                        let count = usize::from(count);
                        let valid = start + count <= 6 && bottom + count <= 6;
                        assert_eq!(result.is_ok(), valid);
                        let charged_items = if cfg!(debug_assertions) && start > 6 {
                            0
                        } else {
                            count as i32
                        };
                        assert_eq!(fuel.remaining(), available.saturating_sub(4 + charged_items));
                        assert!(fuel.is_interrupted());
                        assert_eq!(state.open_upvalues.len(), usize::from(bottom > 3));
                        match capture.get() {
                            UpValueState::Closed(Value::Integer(3)) => assert!(bottom <= 3),
                            UpValueState::Open(_) => assert!(bottom > 3),
                            _ => panic!("unexpected capture state"),
                        }
                        #[cfg(feature = "jit")]
                        drop(stack);
                        let stack = storage.borrow();
                        let mut expected: Vec<_> = (0..6).collect();
                        if valid {
                            expected.copy_within(start..start + count, bottom);
                            expected.truncate(bottom + count);
                            assert!(matches!(state.frames.as_slice(), [Frame::Result { bottom: b }] if *b == bottom));
                        } else {
                            assert!(state.frames.is_empty());
                        }
                        assert_eq!(stack.len(), expected.len());
                        for (value, expected) in stack.iter().zip(expected) {
                            assert!(matches!(value, Value::Integer(actual) if *actual == expected));
                        }
                    });
                }
            }
        }
    }
}

#[test]
fn closing_upvalue_suffix_preserves_lower_captures_and_exact_boundary() {
    for indices in [&[][..], &[0][..], &[0, 2, 5][..]] {
        for bottom in [0, 1, 2, 3, 5, 6, usize::MAX] {
            crate::Lua::empty().enter(|ctx| {
                let thread = Thread::new(ctx);
                let mut state = thread.0.borrow_mut(&ctx);
                let storage = state.stack;
                let mut stack = storage.borrow_mut(&ctx);
                stack.extend((0..6).map(|i| Value::Integer(100 + i)));
                let captures: Vec<_> = indices
                    .iter()
                    .map(|&index| {
                        UpValue::new(
                            &ctx,
                            UpValueState::Open(OpenUpValue {
                                stack: Gc::downgrade(storage),
                                stack_index: index,
                            }),
                        )
                    })
                    .collect();
                state.open_upvalues.extend(captures.iter().copied());
                state.close_upvalues(&ctx, &stack, bottom);
                assert_eq!(
                    state.open_upvalues.len(),
                    indices.iter().filter(|&&i| i < bottom).count()
                );
                for (&index, capture) in indices.iter().zip(&captures) {
                    match capture.get() {
                        UpValueState::Open(open) => {
                            assert!(index < bottom);
                            assert_eq!(open.stack_index, index);
                            assert_eq!(open.stack.as_ptr(), Gc::as_ptr(storage));
                        }
                        UpValueState::Closed(Value::Integer(value)) => {
                            assert!(index >= bottom);
                            assert_eq!(value, 100 + index as i64);
                        }
                        _ => panic!("unexpected capture state"),
                    }
                }
                stack.fill(Value::Integer(999));
                state.close_upvalues(&ctx, &stack, 0);
                assert!(state.open_upvalues.is_empty());
                for (&index, capture) in indices.iter().zip(&captures) {
                    let UpValueState::Closed(Value::Integer(value)) = capture.get() else {
                        panic!("capture remains open");
                    };
                    assert_eq!(
                        value,
                        if index < bottom {
                            999
                        } else {
                            100 + index as i64
                        }
                    );
                }
            });
        }
    }
}
