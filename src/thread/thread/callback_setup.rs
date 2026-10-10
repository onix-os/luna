use super::*;

fn values(stack: &[Value<'_>]) -> Vec<std::string::String> {
    stack
        .iter()
        .map(|value| match value {
            Value::Number(number) => format!("number:{:016x}", number.to_bits()),
            _ => format!("{value:?}"),
        })
        .collect()
}

#[test]
fn callback_setup_matches_physical_call_and_declines_without_effects() {
    crate::Lua::empty().enter(|ctx| {
        let caller = Closure::load(ctx, None, &b"return"[..]).unwrap();
        let callback = Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return));
        let table = Table::new(&ctx);
        let mut cases = Vec::new();
        for count in 0..=254 {
            cases.push((
                2,
                1,
                VarCount::constant(count),
                false,
                270usize,
                2,
                count % 2 == 0,
            ));
        }
        for args in [
            VarCount::constant(0),
            VarCount::constant(3),
            VarCount::variable(),
        ] {
            for variable in [false, true] {
                for depth in [0, 1, 2] {
                    for len in [0, 3, 8] {
                        cases.push((0, 1, args, variable, len, depth, true));
                    }
                }
            }
        }
        let mut accepted = 0;
        for (index, (base, register, args, variable, len, depth, interrupted)) in
            cases.into_iter().enumerate()
        {
            for available in [i32::MIN, 0, 29] {
                let returns = if index % 2 == 0 {
                    VarCount::variable()
                } else {
                    VarCount::constant(2)
                };
                let run = |candidate: u8| {
                    let thread = Thread::new(ctx);
                    let mut state = thread.0.borrow_mut(&ctx);
                    state.max_call_depth = depth;
                    state.frames.push(Frame::Lua {
                        closure: caller,
                        bottom: 0,
                        base,
                        pc: 17,
                        is_variable: variable,
                        stack_size: len.saturating_sub(base),
                        expected_return: Some(LuaReturn::Meta(MetaReturn::None)),
                    });
                    let storage = state.stack;
                    let mut stack = storage.borrow_mut(&ctx);
                    stack.extend((0..len).map(|index| Value::Integer(index as i64)));
                    if let Some(slot) = stack.get_mut(base + register) {
                        *slot = if index < 255 || index % 4 == 0 {
                            Value::Function(Function::Callback(callback))
                        } else {
                            match index % 4 {
                                1 => Value::Function(Function::Closure(caller)),
                                2 => Value::Table(table),
                                _ => Value::Nil,
                            }
                        };
                    }
                    if let Some(slot) = stack.get_mut(base + register + 1) {
                        *slot = Value::Number(f64::from_bits(0x7ff8_0000_0000_0031));
                    }
                    if let Some(slot) = stack.get_mut(base + register + 2) {
                        *slot = Value::Table(table);
                    }
                    let mut fuel = Fuel::with(available);
                    if interrupted {
                        fuel.interrupt();
                    }
                    let mut used = false;
                    let mut decline_unchanged = true;
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                        if candidate == 2 {
                            return frame.call_function_common(
                                ctx,
                                RegisterIndex(register as u8),
                                args,
                                returns,
                            );
                        }
                        if candidate == 1 {
                            let before = (
                                format!("{:?}", frame.state.frames),
                                values(&frame.stack),
                                frame.fuel.remaining(),
                                frame.fuel.is_interrupted(),
                            );
                            used = frame.try_callback_call(
                                RegisterIndex(register as u8),
                                args,
                                returns,
                            );
                            if used {
                                return Ok(());
                            }
                            decline_unchanged = before
                                == (
                                    format!("{:?}", frame.state.frames),
                                    values(&frame.stack),
                                    frame.fuel.remaining(),
                                    frame.fuel.is_interrupted(),
                                );
                        }
                        frame.call_function(ctx, RegisterIndex(register as u8), args, returns)
                    }));
                    #[cfg(feature = "jit")]
                    drop(stack);
                    assert!(decline_unchanged, "decline changed state: case={index}");
                    let outcome = match outcome {
                        Ok(result) => format!("{result:?}"),
                        Err(payload) => {
                            if let Some(message) = payload.downcast_ref::<&str>() {
                                format!("panic str:{message}")
                            } else if let Some(message) =
                                payload.downcast_ref::<std::string::String>()
                            {
                                format!("panic string:{message}")
                            } else {
                                format!("panic type:{:?}", payload.as_ref().type_id())
                            }
                        }
                    };
                    let snapshot = (
                        outcome,
                        format!("{:?}", state.frames),
                        values(&storage.borrow()),
                        fuel.remaining(),
                        fuel.is_interrupted(),
                        state.running,
                        state.closing,
                        state.open_upvalues.len(),
                    );
                    (snapshot, used)
                };
                let (expected, _) = run(0);
                let (actual, used) = run(1);
                assert_eq!(actual, expected, "case={index}, fuel={available}");
                assert_eq!(run(2).0, expected, "public case={index}, fuel={available}");
                accepted += usize::from(used);
            }
        }
        assert!(accepted >= 255 * 3);
    });
}
