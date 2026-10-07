use super::*;

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
