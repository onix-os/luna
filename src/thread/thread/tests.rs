use super::*;

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
