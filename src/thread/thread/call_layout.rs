use super::*;

fn assert_values<'gc>(actual: &[Value<'gc>], expected: &[Value<'gc>]) {
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        match (*actual, *expected) {
            (Value::Nil, Value::Nil) => {}
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b, "{index}"),
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits(), "{index}"),
            (Value::Table(a), Value::Table(b)) => assert_eq!(a, b, "{index}"),
            (Value::Function(a), Value::Function(b)) => assert_eq!(a, b, "{index}"),
            _ => panic!("value mismatch at {index}: {actual:?}, {expected:?}"),
        }
    }
}

#[test]
fn calls_preserve_fixed_and_excess_argument_layout() {
    crate::Lua::empty().enter(|ctx| {
        let caller = Closure::load(ctx, None, b"return").unwrap();
        let table = Table::new(&ctx);
        for fixed in [0, 1, 2, 5, 16, 255] {
            for variadic in [false, true] {
                let mut parameters = (0..fixed).map(|i| format!("p{i}")).collect::<Vec<_>>();
                if variadic {
                    parameters.push("...".to_owned());
                }
                let source = format!(
                    "return function({}) local pad; return end",
                    parameters.join(",")
                );
                let parent = Closure::load(ctx, None, source.as_bytes()).unwrap();
                let prototype = parent.prototype().prototypes[0];
                assert!(prototype.upvalues.is_empty());
                assert_eq!(usize::from(prototype.fixed_params), fixed);
                assert_eq!(prototype.has_varargs, variadic);
                let closure =
                    Closure::from_parts(&ctx, prototype, vec::Vec::new_in(MetricsAlloc::new(&ctx)));
                for prefix in [0, 1, 7] {
                    for given in [0usize, 1, 2, 4, 6, 17, 255, 256, 257] {
                        for refused in [false, true] {
                            let thread = Thread::new(ctx);
                            let mut state = thread.0.borrow_mut(&ctx);
                            state.max_call_depth = if refused { 1 } else { 2 };
                            state.frames.push(Frame::Lua {
                                bottom: 0,
                                closure: caller,
                                base: 0,
                                is_variable: false,
                                pc: 0,
                                stack_size: prefix,
                                expected_return: Some(LuaReturn::Normal(VarCount::constant(2))),
                            });
                            let storage = state.stack;
                            let mut stack = storage.borrow_mut(&ctx);
                            let prefix_values = (0..prefix)
                                .map(|i| Value::Integer(-(i as i64) - 1))
                                .collect::<Vec<_>>();
                            let arguments = (0..given)
                                .map(|i| match i % 5 {
                                    0 => Value::Integer(i as i64),
                                    1 => Value::Number(f64::from_bits(0x7ff8_0000_0000_0031)),
                                    2 => Value::Number(-0.0),
                                    3 => Value::Table(table),
                                    _ => Value::Function(closure.into()),
                                })
                                .collect::<Vec<_>>();
                            stack.extend(prefix_values.iter().copied());
                            stack.extend(arguments.iter().copied());
                            state.push_call(&mut stack, prefix, closure.into());
                            assert_eq!(state.frames.len(), 2);
                            assert!(matches!(state.frames[0], Frame::Lua {
                                bottom: 0, closure: found, base: 0, is_variable: false,
                                pc: 0, stack_size, expected_return: Some(LuaReturn::Normal(count)),
                            } if found == caller && stack_size == prefix && count.to_constant() == Some(2)));
                            if refused {
                                assert_values(&stack, &prefix_values);
                                assert!(matches!(state.frames.last(), Some(Frame::Error(_))));
                                assert_eq!(state.mode(), ThreadMode::Normal);
                                continue;
                            }
                            let excess = given.saturating_sub(fixed);
                            let base = prefix + excess;
                            let mut expected = prefix_values;
                            expected.extend(arguments.iter().skip(fixed).copied());
                            expected.extend(arguments.iter().take(fixed).copied());
                            expected.resize(base + usize::from(prototype.stack_size), Value::Nil);
                            assert_values(&stack, &expected);
                            let Some(Frame::Lua {
                                bottom,
                                closure: found,
                                base: found_base,
                                is_variable,
                                pc,
                                stack_size,
                                expected_return,
                            }) = state.frames.last()
                            else {
                                panic!("call did not queue a Lua frame");
                            };
                            assert_eq!(
                                (*bottom, *found_base, *pc, *stack_size),
                                (prefix, base, 0, usize::from(prototype.stack_size))
                            );
                            assert_eq!(*found, closure);
                            assert!(!is_variable && expected_return.is_none());
                            assert_eq!(state.mode(), ThreadMode::Normal);
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn calls_reject_invalid_bottom_before_publishing_a_frame() {
    crate::Lua::empty().enter(|ctx| {
        for source in ["return function() end", "return function(x) end"] {
            let parent = Closure::load(ctx, None, source.as_bytes()).unwrap();
            let closure = Closure::from_parts(
                &ctx,
                parent.prototype().prototypes[0],
                vec::Vec::new_in(MetricsAlloc::new(&ctx)),
            );
            for length in [0, 1, 7] {
                let thread = Thread::new(ctx);
                let mut state = thread.0.borrow_mut(&ctx);
                let storage = state.stack;
                let mut stack = storage.borrow_mut(&ctx);
                stack.resize(length, Value::Integer(17));
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    state.push_call(&mut stack, length + 1, closure.into());
                }));
                assert!(outcome.is_err());
                assert_eq!(stack.len(), length);
                assert!(state.frames.is_empty());
            }
        }
    });
}
