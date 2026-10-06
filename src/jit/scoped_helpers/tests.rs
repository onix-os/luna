use super::*;
use crate::{thread::activation::with_test_thread, Fuel, JitConfig, JitMode, Lua, Value};

fn slots(host: &mut ActivationHost<'_, '_>) -> Vec<abi::Slot> {
    host.with_registers(|_, registers| {
        registers
            .stack_frame
            .iter()
            .copied()
            .map(abi::Slot::from_value)
            .collect()
    })
}

fn transition(frame: &mut Frame<'_, '_, '_>, slots: &[abi::Slot], instructions: u32) {
    let transition = frame.host.with_registers(|closure, registers| {
        assert_eq!(slots.len(), registers.stack_frame.len());
        for (slot, value) in slots.iter().zip(registers.stack_frame.iter_mut()) {
            slot.write_back(value);
        }
        closure.prototype().opcodes[*registers.pc]
            .call_transition()
            .unwrap()
    });
    frame
        .host
        .native_transition(frame.ctx, transition, instructions)
        .unwrap();
}

#[test]
fn scoped_helpers_reborrow_after_physical_call_growth_and_return() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let mut interner = crate::compiler::interning::BasicInterner::default();
        let chunk = crate::compiler::parse_chunk(
            &b"local n=7 local function f(v) n=n+v end f(5) return n"[..],
            &mut interner,
        )
        .unwrap();
        let mut compiled = crate::compiler::compile_chunk(&chunk, &mut interner).unwrap();
        compiled.prototypes[0].stack_size = 256;
        let prototype = crate::FunctionPrototype::from_compiled_map_strings(
            &ctx,
            ctx.intern(b"scoped-growth"),
            &compiled,
            false,
            |s| ctx.intern(s.as_ref()),
        );
        let closure = Closure::new(&ctx, prototype, Some(ctx.globals())).unwrap();
        let prototype = closure.prototype();
        ctx.jit_registry().borrow_mut(&ctx).register(ctx, prototype);
        let caller = Code::new(ctx, closure).unwrap();
        let start = prototype
            .opcodes
            .iter()
            .position(|op| matches!(op.decode(), crate::opcode::Operation::Closure { .. }))
            .unwrap()
            + 1;
        let run = |native| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                if native {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let mut caller_slots = slots(host);
                    let mut frame = Frame::new(ctx, host);
                    let prefix = caller.invoke(&mut frame, &mut caller_slots, 64).unwrap();
                    assert!(prefix.instructions > 0);
                    let capacity = frame.host.test_capacity();
                    transition(&mut frame, &caller_slots, prefix.instructions);
                    assert!(frame.host.test_capacity() > capacity);
                    assert!(caller.invoke(&mut frame, &mut caller_slots, 64).is_none());
                    let child_closure = frame.host.with_registers(|closure, _| closure);
                    let child = Code::new(ctx, child_closure).unwrap();
                    let mut child_slots = slots(frame.host);
                    assert_eq!(child_slots.len(), 256);
                    let body = child.invoke(&mut frame, &mut child_slots, 64).unwrap();
                    assert_eq!(body.instructions, 3);
                    transition(&mut frame, &child_slots, body.instructions);
                    caller_slots = slots(frame.host);
                    let suffix = caller.invoke(&mut frame, &mut caller_slots, 64).unwrap();
                    assert!(suffix.instructions > 0);
                    transition(&mut frame, &caller_slots, suffix.instructions);
                    assert_eq!(
                        (frame.count.upvalue_reads, frame.count.upvalue_writes),
                        (1, 1)
                    );
                    assert!(frame.panic.is_none());
                } else {
                    host.run(ctx, 3, 64, 4).result.unwrap();
                }
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let (frames, values, open, fuel) = host.test_trace();
                assert_eq!(values.len(), 1);
                assert!(matches!(values[0], Value::Integer(12)));
                (frames, open, fuel)
            })
        };
        assert_eq!(run(true), run(false));
    });
}

#[test]
fn generated_scoped_helpers_match_canonical_effects_and_release_register_borrows() {
    let mut lua = Lua::empty();
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"local n=0 return function() n=3 shared=4 local t={} t.key=shared local alias=t local s='x' local v=t.key n=n+v return n,s,alias.key end"[..]).unwrap();
        ctx.stash(crate::Executor::start(ctx, closure.into(), ()))
    });
    lua.finish(&executor).unwrap();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = ctx
            .fetch(&executor)
            .take_result::<Closure>(ctx)
            .unwrap()
            .unwrap();
        let code = Code::new(ctx, closure).unwrap();
        let run = |native| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                if native {
                    let mut slots = host.with_registers(|_, registers| {
                        registers
                            .stack_frame
                            .iter()
                            .copied()
                            .map(abi::Slot::from_value)
                            .collect::<Vec<_>>()
                    });
                    let mut frame = Frame::new(ctx, host);
                    let exit = code.invoke(&mut frame, &mut slots, 64).unwrap();
                    assert!(frame.panic.is_none());
                    assert_eq!(exit.reason, super::super::exits::Kind::Interpreter as u32);
                    assert_eq!(
                        exit.instructions as usize,
                        closure
                            .prototype()
                            .opcodes
                            .iter()
                            .position(|op| matches!(
                                op.decode(),
                                crate::opcode::Operation::Return { .. }
                            ))
                            .unwrap()
                    );
                    assert_eq!(frame.kinds, (1 << 9) - 1);
                    assert_eq!(frame.count.calls, frame.count.completed);
                    assert_eq!(frame.count.declined, 0);
                    let transition = frame.host.with_registers(|_, registers| {
                        for (slot, value) in slots.iter().zip(registers.stack_frame.iter_mut()) {
                            slot.write_back(value);
                        }
                        closure.prototype().opcodes[*registers.pc]
                            .call_transition()
                            .unwrap()
                    });
                    frame
                        .host
                        .native_transition(ctx, transition, exit.instructions)
                        .unwrap();
                    assert!(ctx.jit().0.try_borrow_mut().is_ok());
                } else {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, 64, 4).result.unwrap();
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                }
                let (frames, values, open, fuel) = host.test_trace();
                assert_eq!(values.len(), 3);
                assert!(matches!(values[0], Value::Integer(7)));
                assert!(matches!(values[1], Value::String(s) if s.as_bytes() == b"x"));
                assert!(matches!(values[2], Value::Integer(4)));
                (
                    frames,
                    values
                        .iter()
                        .copied()
                        .map(abi::Slot::from_value)
                        .map(|s| (s.tag, s.bits))
                        .collect::<Vec<_>>(),
                    open,
                    fuel,
                )
            })
        };
        assert_eq!(run(true), run(false));
        assert!(matches!(
            ctx.globals().get(ctx, "shared"),
            Ok(Value::Integer(4))
        ));
    });
}

#[test]
fn scoped_entry_rejects_wrong_source_owner_mode_hooks_and_slot_layout() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"local t={} return t"[..]).unwrap();
        let foreign = Closure::load(ctx, None, &b"local t={} return t"[..]).unwrap();
        let mut code = Code::new(ctx, closure).unwrap();
        for case in 0..6 {
            with_test_thread(
                ctx,
                if case == 0 { foreign } else { closure },
                &mut Fuel::with(10000),
                |host| {
                    let before = format!("{:?}", host.test_trace());
                    let mut slots = host.with_registers(|_, registers| {
                        registers
                            .stack_frame
                            .iter()
                            .copied()
                            .map(abi::Slot::from_value)
                            .collect::<Vec<_>>()
                    });
                    let original = code.origin.clone();
                    match case {
                        1 => {
                            code.origin =
                                MappingCounter::new(super::super::resources::Ledger::new(1024))
                        }
                        2 => ctx.jit().0.borrow_mut().config.mode = JitMode::Off,
                        3 => {
                            let hook = crate::Callback::from_fn(&ctx, |_, _, _| {
                                panic!("declined hook executed")
                            });
                            ctx.set_debug_hook(hook.into(), false, 1);
                        }
                        4 => {
                            slots.pop();
                        }
                        5 => slots[0].tag = u64::MAX,
                        _ => {}
                    }
                    let mut frame = Frame::new(ctx, host);
                    assert!(
                        code.invoke(&mut frame, &mut slots, 64).is_none(),
                        "case={case}"
                    );
                    assert_eq!(frame.count.calls, 0);
                    assert_eq!(frame.kinds, 0);
                    assert!(frame.panic.is_none());
                    assert_eq!(format!("{:?}", frame.host.test_trace()), before);
                    code.origin = original;
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    ctx.set_debug_hook(Value::Nil, false, 0);
                },
            );
        }
    });
}

#[test]
fn generated_scoped_decline_preserves_pending_values_and_canonical_error() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &b"local t=1 local p=9 local v=t.key return p,v"[..],
        )
        .unwrap();
        let code = Code::new(ctx, closure).unwrap();
        let run = |native| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                if native {
                    let mut slots = slots(host);
                    let mut frame = Frame::new(ctx, host);
                    let exit = code.invoke(&mut frame, &mut slots, 64).unwrap();
                    assert_eq!(exit.instructions, 2);
                    assert_eq!(
                        (
                            frame.count.calls,
                            frame.count.completed,
                            frame.count.declined
                        ),
                        (1, 0, 1)
                    );
                    assert!(frame.panic.is_none());
                    frame.host.with_registers(|_, registers| {
                        assert_eq!(*registers.pc, 2);
                        assert!(registers
                            .stack_frame
                            .iter()
                            .all(|v| matches!(v, Value::Nil)));
                        for (slot, value) in slots.iter().zip(registers.stack_frame.iter_mut()) {
                            slot.write_back(value);
                        }
                    });
                }
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                let error = host.run(ctx, 1, 64, 4).result.unwrap_err();
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                (format!("{error:?}"), format!("{:?}", host.test_trace()))
            })
        };
        assert_eq!(run(true), run(false));
    });
}

#[test]
fn scoped_helper_panic_materializes_pending_slots_and_releases_register_borrow() {
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"local n=7 return n"[..]).unwrap();
        with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            let mut slots = host.with_registers(|_, registers| {
                registers
                    .stack_frame
                    .iter()
                    .copied()
                    .map(abi::Slot::from_value)
                    .collect::<Vec<_>>()
            });
            slots[0] = abi::Slot::from_value(Value::Integer(99));
            let mut frame = Frame::new(ctx, host);
            frame.slot_count = slots.len();
            let mut native = abi::Host {
                data: std::ptr::from_mut(&mut frame).cast(),
                projection: std::ptr::null_mut(),
            };
            assert_eq!(
                unsafe {
                    call::<{ abi::HELPER_MOVE }>(&mut native, slots.as_mut_ptr(), u32::MAX, 0, 0, 0)
                },
                abi::HELPER_PANICKED
            );
            let payload = frame.panic.take().unwrap();
            let original = payload.as_ref() as *const dyn Any as *const ();
            let propagated =
                catch_unwind(AssertUnwindSafe(|| std::panic::resume_unwind(payload))).unwrap_err();
            assert_eq!(propagated.as_ref() as *const dyn Any as *const (), original);
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.completed,
                    frame.count.declined
                ),
                (1, 0, 0)
            );
            frame.host.with_registers(|_, registers| {
                assert_eq!(*registers.pc, 1);
                assert!(matches!(registers.stack_frame[0], Value::Integer(99)));
            });
            assert!(ctx.jit().0.try_borrow_mut().is_ok());
        });
    });
}
