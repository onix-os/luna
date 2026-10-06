use super::*;
use crate::jit::{
    backend::{
        self,
        calls::{compile, LinkFault},
    },
    calls::Plan,
    ir::Snapshot,
    resources::{BudgetAllocator, Ledger, MappingCounter},
    work,
};
use crate::{thread::activation::with_test_thread, Closure, Fuel, JitConfig, Lua};

const ADD: &[u8] = b"local n=7 local function f(v) n=n+v end f(2) return n";

#[test]
fn caller_shadow_transfer_refreshes_after_real_native_call_and_return() {
    fixture(ADD, |lua, closure, site, code| {
        lua.enter(|ctx| {
            let native =
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let (mut shadow, capture) = host.with_registers(|caller, registers| {
                        let Value::Function(Function::Closure(callee)) =
                            registers.stack_frame[usize::from(site.function.0)]
                        else {
                            panic!("expected closure");
                        };
                        (
                            crate::jit::caller_shadow::Shadow::<256>::capture(caller, &registers)
                                .unwrap(),
                            callee.upvalues()[usize::from(site.pattern.upvalue)].get(),
                        )
                    });
                    shadow.slots_mut()[0] = Slot::from_value(Value::Integer(12));
                    shadow.slots_mut()[usize::from(site.function.0) + 1] =
                        Slot::from_value(Value::Integer(5));
                    let transfer = host.with_registers(|caller, mut registers| {
                        shadow
                            .prepare_call(
                                caller,
                                &mut registers,
                                site.function.0,
                                site.arguments,
                                capture,
                            )
                            .unwrap()
                    });
                    let before = ctx.jit().0.borrow().stats.native_instructions;
                    let before_stats = stats(ctx);
                    let outcome = invoke_result(ctx, host, &site, &code, 64, 0).unwrap();
                    outcome.result.unwrap();
                    assert_eq!((outcome.calls, outcome.returns), (1, 1));
                    assert_eq!(ctx.jit().0.borrow().stats.native_instructions - before, 3);
                    assert!(host
                        .with_registers(|caller, registers| transfer.resume(caller, &registers)));
                    assert!(host.with_registers(
                        |caller, mut registers| shadow.flush(caller, &mut registers)
                    ));
                    host.with_registers(|_, registers| {
                        assert!(matches!(registers.stack_frame[0], Value::Integer(17)));
                        assert_eq!(*registers.pc, site.pc + 1);
                    });
                    trace(ctx, host, before_stats)
                });
            let interpreted =
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    host.with_registers(|_, registers| {
                        registers.stack_frame[0] = Value::Integer(12);
                        registers.stack_frame[usize::from(site.function.0) + 1] = Value::Integer(5);
                    });
                    let before_stats = stats(ctx);
                    let outcome = host.run(ctx, 2, 64, 4);
                    outcome.result.unwrap();
                    trace(ctx, host, before_stats)
                });
            assert_eq!(native, interpreted);
        });
    });
}

fn fixture(source: &[u8], test: impl FnOnce(&mut Lua, crate::StashedClosure, Site, CallCode)) {
    fixture_hooks(source, Hooks { enter, leave }, test);
}

fn fixture_hooks(
    source: &[u8],
    hooks: Hooks,
    test: impl FnOnce(&mut Lua, crate::StashedClosure, Site, CallCode),
) {
    fixture_return_count(source, hooks, None, test);
}

fn fixture_return_count(
    source: &[u8],
    hooks: Hooks,
    returns: Option<u8>,
    test: impl FnOnce(&mut Lua, crate::StashedClosure, Site, CallCode),
) {
    fixture_layout(source, hooks, returns, None, test);
}

fn fixture_layout(
    source: &[u8],
    hooks: Hooks,
    returns: Option<u8>,
    registers: Option<u16>,
    test: impl FnOnce(&mut Lua, crate::StashedClosure, Site, CallCode),
) {
    let mut lua = Lua::empty();
    let (closure, caller, callee, caller_id, callee_id) = lua.enter(|ctx| {
        let closure = if returns.is_some() || registers.is_some() {
            let mut interner = crate::compiler::interning::BasicInterner::default();
            let chunk = crate::compiler::parse_chunk(source, &mut interner).unwrap();
            let mut compiled = crate::compiler::compile_chunk(&chunk, &mut interner).unwrap();
            let child = &mut compiled.prototypes[0];
            assert_eq!(child.opcodes.len(), 4);
            if let Some(count) = returns {
                child.opcodes[3] = crate::opcode::OpCode::encode(Operation::Return {
                    start: RegisterIndex(0),
                    count: crate::types::VarCount::constant(count),
                });
            }
            if let Some(registers) = registers {
                child.stack_size = registers;
            }
            let prototype = crate::FunctionPrototype::from_compiled_map_strings(
                &ctx,
                ctx.intern(b"fixed-return"),
                &compiled,
                false,
                |string| ctx.intern(string.as_ref()),
            );
            let closure = Closure::new(&ctx, prototype, Some(ctx.globals())).unwrap();
            ctx.jit_registry()
                .borrow_mut(&ctx)
                .register(ctx, closure.prototype());
            closure
        } else {
            Closure::load(ctx, None, source).unwrap()
        };
        let prototype = closure.prototype();
        let registry = ctx.jit_registry().borrow();
        (
            ctx.stash(closure),
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap(),
            Snapshot::new(&prototype.prototypes[0], 4096, 2 * 1024 * 1024).unwrap(),
            registry.identity(ctx, prototype).unwrap(),
            registry.identity(ctx, prototype.prototypes[0]).unwrap(),
        )
    });
    let pc = caller
        .operations
        .iter()
        .position(|op| matches!(op, Operation::Call { .. }))
        .unwrap();
    let limits = work::Limits::from(&JitConfig::default());
    let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
    let site = Site {
        caller: caller_id,
        callee: callee_id,
        pc,
        function: plan.function,
        arguments: plan.arguments,
        pattern: plan.pattern,
        registers: callee.registers,
        start: match callee.operations[3] {
            Operation::Return { start, .. } => start,
            _ => unreachable!(),
        },
        returns: match callee.operations[3] {
            Operation::Return { count, .. } => count.to_constant().unwrap(),
            _ => unreachable!(),
        },
    };
    let root = Ledger::new(8 * 1024 * 1024);
    let code = compile(
        &plan,
        hooks,
        MappingCounter::new(Ledger::child(2 * 1024 * 1024, root.clone())),
        2 * 1024 * 1024,
        BudgetAllocator(Ledger::child(2 * 1024 * 1024, root)),
        limits,
        backend::Failure::None,
        LinkFault::None,
    )
    .unwrap();
    drop(caller);
    drop(callee);
    test(&mut lua, closure, site, code);
}

#[derive(Debug, PartialEq)]
struct Trace {
    frames: Vec<String>,
    slots: Vec<(u64, u64)>,
    open: usize,
    fuel: i32,
    dispatches: u64,
    instructions: u64,
}

fn trace(ctx: Context<'_>, host: &ActivationHost<'_, '_>, before: (u64, u64)) -> Trace {
    let (frames, values, open, fuel) = host.test_trace();
    let stats = &ctx.jit().0.borrow().stats;
    Trace {
        frames,
        slots: values
            .into_iter()
            .map(|value| {
                let slot = Slot::from_value(value);
                (slot.tag, slot.bits)
            })
            .collect(),
        open,
        fuel,
        dispatches: stats.total_dispatches - before.0,
        instructions: stats.native_instructions + stats.interpreted_instructions - before.1,
    }
}

fn stats(ctx: Context<'_>) -> (u64, u64) {
    let stats = &ctx.jit().0.borrow().stats;
    (
        stats.total_dispatches,
        stats.native_instructions + stats.interpreted_instructions,
    )
}

fn rebind<'gc>(
    ctx: Context<'gc>,
    callee: Closure<'gc>,
    prototype: ottavino_gc_arena::Gc<'gc, crate::FunctionPrototype<'gc>>,
) -> Closure<'gc> {
    let mut upvalues =
        allocator_api2::vec::Vec::new_in(ottavino_gc_arena::allocator_api::MetricsAlloc::new(&ctx));
    upvalues.extend(
        callee
            .upvalues()
            .iter()
            .map(|value| ottavino_gc_arena::lock::Lock::new(value.get())),
    );
    Closure::from_parts(&ctx, prototype, upvalues)
}

#[test]
fn physical_call_checks_replacements_and_refreshes_capture_after_preflight() {
    fixture(ADD, |lua, closure, site, code| {
        for case in 0..3 {
            lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let original = host.with_registers(|_, registers| {
                        let Value::Function(Function::Closure(callee)) =
                            registers.stack_frame[usize::from(site.function.0)]
                        else {
                            panic!();
                        };
                        callee
                    });
                    let prototype = if case == 1 {
                        Closure::load(ctx, None, ADD)
                            .unwrap()
                            .prototype()
                            .prototypes[0]
                    } else {
                        original.prototype()
                    };
                    let replacement = rebind(ctx, original, prototype);
                    assert_ne!(replacement, original);
                    if case == 2 {
                        replacement.set_upvalue(
                            &ctx,
                            usize::from(site.pattern.upvalue),
                            crate::closure::UpValue::new(
                                &ctx,
                                crate::closure::UpValueState::Closed(Value::Integer(99)),
                            ),
                        );
                    }
                    let before = stats(ctx);
                    let mut scratch = [MaybeUninit::uninit(); 256];
                    let mut session = Session::new(ctx, host, &site, &mut scratch);
                    assert!(session.preflight(64));
                    session.host.with_registers(|_, registers| {
                        registers.stack_frame[usize::from(site.function.0)] = replacement.into();
                    });
                    let completed = usize::from(case == 0);
                    assert_eq!(
                        unsafe { code.invoke(std::ptr::addr_of_mut!(session).cast(), 64) },
                        2 * completed as u32,
                        "case={case}"
                    );
                    assert_eq!((session.calls, session.returns), (1, completed));
                    assert!(session.error.is_none() && session.panic.is_none());
                    assert_eq!(
                        trace(ctx, session.host, before).instructions,
                        3 * completed as u64
                    );
                    if case != 0 {
                        session
                            .host
                            .with_registers(|_, registers| assert_eq!(*registers.pc, 0));
                    }
                    if case == 2 {
                        assert!(matches!(
                            replacement.upvalues()[usize::from(site.pattern.upvalue)]
                                .get()
                                .get(),
                            crate::closure::UpValueState::Closed(Value::Integer(99))
                        ));
                    }
                });
            });
        }
    });
}

#[test]
fn changed_physical_prototype_refuses_leave_before_materialization() {
    fixture(ADD, |lua, closure, site, _code| {
        lua.enter(|ctx| {
            with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                position(host, ctx, site.pc);
                let replacement = Closure::load(ctx, None, b"return 0").unwrap();
                let mut scratch = [MaybeUninit::uninit(); 256];
                let mut session = Session::new(ctx, host, &site, &mut scratch);
                assert!(session.preflight(64));
                let data = std::ptr::addr_of_mut!(session).cast();
                let frame = unsafe {
                    enter(
                        data,
                        site.pc as u64,
                        u32::from(site.function.0),
                        u32::from(site.arguments),
                    )
                };
                assert!(!frame.is_null());
                session.frame.exit = Exit {
                    pc: 3,
                    instructions: 3,
                    reason: Kind::Interpreter as u32,
                };
                session.slots[usize::from(site.pattern.result.0)]
                    .write(Slot::from_value(Value::Integer(999)));
                session.host.test_replace_closure(replacement);
                let before = stats(ctx);
                let original = trace(ctx, session.host, before);
                assert_eq!(unsafe { leave(data, frame, 3, u32::from(site.start.0)) }, 0);
                assert!(session.panic.is_some());
                assert_eq!(session.returns, 0);
                assert_eq!(trace(ctx, session.host, before), original);
            });
        });
    });
}

fn position<'gc>(host: &mut ActivationHost<'gc, '_>, ctx: Context<'gc>, pc: usize) {
    let outcome = host.run(ctx, 1, pc.try_into().unwrap(), 4);
    assert!(outcome.result.is_ok());
    host.with_registers(|_, registers| assert_eq!(*registers.pc, pc));
}

#[test]
fn linked_native_calls_match_physical_frames_fuel_and_dispatches() {
    for source in [
        ADD,
        &b"local n=7 local function f(v) n=n-v end f(2) return n"[..],
        &b"local n=7 local function f(v) n=n*v end f(2) return n"[..],
        &b"local n=9223372036854775807 local function f(v) n=n+v end f(2) return n"[..],
        &b"local n=-9223372036854775807-1 local function f(v) n=n-v end f(1) return n"[..],
        &b"local n=9223372036854775807 local function f(v) n=n*v end f(2) return n"[..],
        &b"local n=9223372036854775807 local function f() n=n+2 end f() return n"[..],
        &b"local n=7 local function f(v,w,x) n=n+v end f(2,{},'keep') return n"[..],
        &b"local n=7 local function f() n=n+2 end f() return n"[..],
    ] {
        fixture(source, |lua, closure, site, code| {
            for available in [-1, 0, 1, 8, 9, 10, 19, 20, 21, 1000] {
                let mut interrupted = Fuel::with(1000);
                interrupted.interrupt();
                for fuel in [Fuel::with(available), interrupted] {
                    let interpreted = lua.enter(|ctx| {
                        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                            position(host, ctx, site.pc);
                            host.test_fuel(fuel.clone());
                            let before = stats(ctx);
                            let outcome = host.run(ctx, 2, 64, 4);
                            assert!(outcome.result.is_ok());
                            trace(ctx, host, before)
                        })
                    });
                    let native = lua.enter(|ctx| {
                        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                            position(host, ctx, site.pc);
                            host.test_fuel(fuel.clone());
                            let before = stats(ctx);
                            let result = invoke(ctx, host, &site, &code, 64);
                            if result.is_none() {
                                assert!(host.run(ctx, 2, 64, 4).result.is_ok());
                            } else {
                                assert_eq!(result, Some((1, 1)));
                            }
                            trace(ctx, host, before)
                        })
                    });
                    assert_eq!(native, interpreted, "source={source:?}, fuel={fuel:?}");
                }
            }
        });
    }
}

#[test]
fn largest_callee_scratch_prefix_matches_canonical_frames() {
    fixture_layout(
        ADD,
        Hooks { enter, leave },
        None,
        Some(256),
        |lua, closure, site, code| {
            assert_eq!(site.registers, 256);
            let interpreted = lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let before = stats(ctx);
                    assert!(host.run(ctx, 2, 64, 4).result.is_ok());
                    trace(ctx, host, before)
                })
            });
            let native = lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let before = stats(ctx);
                    assert_eq!(invoke(ctx, host, &site, &code, 64), Some((1, 1)));
                    trace(ctx, host, before)
                })
            });
            assert_eq!(native, interpreted);
        },
    );
}

#[test]
fn leave_before_enter_is_caught_without_reading_scratch() {
    fixture(ADD, |lua, closure, site, _code| {
        lua.enter(|ctx| {
            with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                position(host, ctx, site.pc);
                let mut scratch = [MaybeUninit::uninit(); 256];
                let mut session = Session::new(ctx, host, &site, &mut scratch);
                let before = trace(ctx, session.host, stats(ctx));
                let frame = std::ptr::addr_of_mut!(session.frame);
                let data = std::ptr::addr_of_mut!(session).cast();
                assert_eq!(unsafe { leave(data, frame, 3, u32::from(site.start.0)) }, 0);
                assert!(session.panic.is_some());
                assert_eq!(session.calls, 0);
                assert_eq!(trace(ctx, session.host, stats(ctx)), before);
            });
        });
    });
}

#[test]
fn declined_source_and_budget_requests_preserve_caller_state() {
    fixture(ADD, |lua, closure, mut site, code| {
        lua.enter(|ctx| {
            with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                position(host, ctx, site.pc);
                let before = stats(ctx);
                let original = trace(ctx, host, before);
                for budget in 0..4 {
                    assert_eq!(invoke(ctx, host, &site, &code, budget), None);
                }
                site.caller += 1000;
                assert_eq!(invoke(ctx, host, &site, &code, 64), None);
                site.caller -= 1000;
                site.callee += 1000;
                assert_eq!(invoke(ctx, host, &site, &code, 64), None);
                site.callee -= 1000;
                site.pc += 1;
                assert_eq!(invoke(ctx, host, &site, &code, 64), None);
                assert_eq!(trace(ctx, host, before), original);
            });
        });
    });
}

#[test]
fn fixed_callee_result_counts_preserve_canonical_return_effects_and_fuel() {
    for count in [1, 2] {
        fixture_return_count(
            b"local n=7 local function f(v,w) n=n+v end f(2,3) return n",
            Hooks { enter, leave },
            Some(count),
            |lua, closure, site, code| {
                for available in [10, 11, 20, 21, 1000] {
                    let run = |lua: &mut Lua, native: bool| {
                        lua.enter(|ctx| {
                            with_test_thread(
                                ctx,
                                ctx.fetch(&closure),
                                &mut Fuel::with(10000),
                                |host| {
                                    position(host, ctx, site.pc);
                                    host.test_fuel(Fuel::with(available));
                                    let before = stats(ctx);
                                    if !native || invoke(ctx, host, &site, &code, 64).is_none() {
                                        assert!(host.run(ctx, 2, 64, 4).result.is_ok());
                                    }
                                    trace(ctx, host, before)
                                },
                            )
                        })
                    };
                    assert_eq!(
                        run(lua, true),
                        run(lua, false),
                        "count={count}, fuel={available}"
                    );
                }
            },
        );
    }
}

#[test]
fn noninteger_capture_and_argument_decline_without_mutation() {
    for source in [
        &b"local n=7.5 local function f(v) n=n+v end f(2) return n"[..],
        &b"local n=7 local function f(v) n=n+v end f(2.5) return n"[..],
    ] {
        fixture(source, |lua, closure, site, code| {
            lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let before = stats(ctx);
                    let original = trace(ctx, host, before);
                    assert_eq!(invoke(ctx, host, &site, &code, 64), None);
                    assert_eq!(trace(ctx, host, before), original);
                });
            });
        });
    }
}

#[test]
fn native_budget_declines_keep_the_callee_frame_for_interpreter_resume() {
    fixture(ADD, |lua, closure, site, code| {
        for budget in 0..4 {
            let interpreted = lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let before = stats(ctx);
                    assert!(host.run(ctx, 1, 64, 4).result.is_ok());
                    assert!(host.run(ctx, 1, 0, 4).result.is_ok());
                    trace(ctx, host, before)
                })
            });
            let native = lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let before = stats(ctx);
                    let mut scratch = [MaybeUninit::uninit(); 256];
                    let mut session = Session::new(ctx, host, &site, &mut scratch);
                    assert!(session.preflight(64));
                    let returned =
                        unsafe { code.invoke(std::ptr::addr_of_mut!(session).cast(), budget) };
                    assert_eq!(returned, 1);
                    assert_eq!(
                        (session.calls, session.returns, session.panic.is_some()),
                        (1, 0, false)
                    );
                    assert!(session.error.is_none());
                    drop(session);
                    host.with_registers(|_, registers| assert_eq!(*registers.pc, 0));
                    trace(ctx, host, before)
                })
            });
            assert_eq!(native, interpreted, "budget={budget}");
        }
    });
}

#[test]
fn hooks_callbacks_tables_and_closed_captures_decline_before_call() {
    fixture(ADD, |lua, closure, site, code| {
        for case in 0..5 {
            lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let callback =
                        crate::Callback::from_fn(&ctx, |_, _, _| panic!("declined callback ran"));
                    match case {
                        0 | 1 => {
                            ctx.set_debug_hook(callback.into(), false, 1);
                            if case == 1 {
                                ctx.suppress_hook_at(1);
                            }
                        }
                        2 => host.with_registers(|_, registers| {
                            registers.stack_frame[usize::from(site.function.0)] = callback.into()
                        }),
                        3 => host.with_registers(|_, registers| {
                            registers.stack_frame[usize::from(site.function.0)] =
                                crate::Table::new(&ctx).into()
                        }),
                        4 => host.with_registers(|_, registers| {
                            let Value::Function(Function::Closure(callee)) =
                                registers.stack_frame[usize::from(site.function.0)]
                            else {
                                panic!();
                            };
                            callee.set_upvalue(
                                &ctx,
                                usize::from(site.pattern.upvalue),
                                crate::closure::UpValue::new(
                                    &ctx,
                                    crate::closure::UpValueState::Closed(Value::Integer(7)),
                                ),
                            );
                        }),
                        _ => unreachable!(),
                    }
                    let before = stats(ctx);
                    let original = trace(ctx, host, before);
                    assert_eq!(invoke(ctx, host, &site, &code, 64), None);
                    assert_eq!(trace(ctx, host, before), original);
                    ctx.set_debug_hook(Value::Nil, false, 0);
                });
            });
        }
    });
}

#[test]
fn foreign_thread_capture_declines_without_borrowing_its_stack() {
    fixture(ADD, |lua, closure, site, code| {
        lua.enter(|ctx| {
            with_test_thread(
                ctx,
                ctx.fetch(&closure),
                &mut Fuel::with(10000),
                |foreign| {
                    position(foreign, ctx, site.pc);
                    let capture = foreign.with_registers(|_, registers| {
                        let Value::Function(Function::Closure(callee)) =
                            registers.stack_frame[usize::from(site.function.0)]
                        else {
                            panic!();
                        };
                        callee.upvalues()[usize::from(site.pattern.upvalue)].get()
                    });
                    with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                        position(host, ctx, site.pc);
                        host.with_registers(|_, registers| {
                            let Value::Function(Function::Closure(callee)) =
                                registers.stack_frame[usize::from(site.function.0)]
                            else {
                                panic!();
                            };
                            callee.set_upvalue(&ctx, usize::from(site.pattern.upvalue), capture);
                        });
                        let before = stats(ctx);
                        let original = trace(ctx, host, before);
                        assert_eq!(invoke(ctx, host, &site, &code, 64), None);
                        assert_eq!(trace(ctx, host, before), original);
                    });
                },
            );
        });
    });
}

#[test]
fn refused_callee_depth_preserves_the_consumed_call() {
    fixture(ADD, |lua, closure, site, code| {
        let run = |lua: &mut Lua, native| {
            lua.enter(|ctx| {
                ctx.set_max_call_depth(1);
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let before = stats(ctx);
                    if native {
                        assert_eq!(invoke(ctx, host, &site, &code, 64), Some((1, 0)));
                    } else {
                        assert!(host.run(ctx, 2, 64, 4).result.is_ok());
                    }
                    trace(ctx, host, before)
                })
            })
        };
        assert_eq!(run(lua, true), run(lua, false));
    });
}

#[test]
fn callee_stack_growth_rebuilds_typed_capture_and_register_views() {
    let parameters = (0..200)
        .map(|index| format!("v{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let source = format!("local n=7 local function f({parameters}) n=n+v0 end f(2) return n");
    fixture(source.as_bytes(), |lua, closure, site, code| {
        lua.enter(|ctx| {
            with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                position(host, ctx, site.pc);
                let capacity = host.test_capacity();
                assert_eq!(invoke(ctx, host, &site, &code, 64), Some((1, 1)));
                assert!(host.test_capacity() > capacity);
                host.with_registers(|_, registers| {
                    assert!(registers
                        .stack_frame
                        .iter()
                        .any(|value| matches!(value, Value::Integer(9))))
                });
            });
        });
    });
}

#[test]
fn host_releases_before_collection_callbacks_and_nested_executor_steps() {
    use crate::{
        thread::activation::with_test_existing_thread, Callback, CallbackReturn, Executor,
    };
    use std::{cell::RefCell, rc::Rc};

    fixture(
        b"local cb=... local n=7 local function f(v) n=n+v end f(2) cb() return n",
        |lua, closure, site, code| {
            let observed = Rc::new(RefCell::new(Vec::new()));
            let saved = observed.clone();
            let executor = lua.enter(|ctx| {
                let thread = crate::Thread::new(ctx);
                thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
                with_test_existing_thread(ctx, thread, &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let capture = host.with_registers(|_, registers| {
                        let Value::Function(Function::Closure(callee)) =
                            registers.stack_frame[usize::from(site.function.0)]
                        else {
                            panic!();
                        };
                        callee.upvalues()[usize::from(site.pattern.upvalue)].get()
                    });
                    let nested =
                        Closure::load(ctx, None, b"local n=0 for i=1,7 do n=n+i end return n")
                            .unwrap();
                    let callback = Callback::from_fn_with(
                        &ctx,
                        (capture, nested),
                        move |(capture, nested), ctx, _, mut stack| {
                            let crate::closure::UpValueState::Open(open) = capture.get() else {
                                panic!("capture closed before parent return");
                            };
                            assert!(matches!(open.get(&ctx), Value::Integer(9)));
                            let before = stats(ctx);
                            assert!(ctx.jit().0.borrow().stats.native_upvalue_writes > 0);
                            let executor = Executor::start(ctx, (*nested).into(), ());
                            let mut fuel = Fuel::with(65536);
                            while !executor.step(ctx, &mut fuel).unwrap() {}
                            assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 28);
                            let after = stats(ctx);
                            assert!(after.0 > before.0);
                            open.set(&ctx, Value::Integer(11));
                            saved.borrow_mut().push((before, after));
                            stack.clear();
                            Ok(CallbackReturn::Return)
                        },
                    );
                    host.with_registers(|_, registers| registers.stack_frame[0] = callback.into());
                    assert_eq!(invoke(ctx, host, &site, &code, 64), Some((1, 1)));
                });
                ctx.stash(Executor::run(&ctx, thread).unwrap())
            });
            lua.gc_collect();
            lua.gc_collect();
            lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                let mut fuel = Fuel::with(65536);
                while !executor.step(ctx, &mut fuel).unwrap() {}
                assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 11);
            });
            assert_eq!(observed.borrow().len(), 1);
        },
    );
}

#[test]
fn mismatched_leave_is_caught_before_canonical_return_or_materialization() {
    for (pc, start_offset, calls, returns) in
        [(999, 0, 1, 0), (3, 1, 1, 0), (3, 0, 0, 0), (3, 0, 1, 1)]
    {
        fixture(ADD, |lua, closure, site, _code| {
            lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let mut scratch = [MaybeUninit::uninit(); 256];
                    let mut session = Session::new(ctx, host, &site, &mut scratch);
                    assert!(session.preflight(64));
                    let data = std::ptr::addr_of_mut!(session).cast();
                    let frame = unsafe {
                        enter(
                            data,
                            site.pc as u64,
                            u32::from(site.function.0),
                            u32::from(site.arguments),
                        )
                    };
                    assert!(!frame.is_null());
                    session.calls = calls;
                    session.returns = returns;
                    let original = trace(ctx, session.host, stats(ctx));
                    assert_eq!(
                        unsafe { leave(data, frame, pc, u32::from(site.start.0) + start_offset) },
                        0
                    );
                    assert!(session.panic.is_some());
                    assert_eq!(session.returns, returns);
                    assert_eq!(trace(ctx, session.host, stats(ctx)), original);
                });
            });
        });
    }
}

#[test]
fn argument_shifting_rebinds_current_frame_aliases_and_preserves_late_declines() {
    for (source, expected) in [
        (ADD, (1, 0)),
        (
            &b"local n=7 local function f(v,w) n=n+v end f(2,3) return n"[..],
            (1, 1),
        ),
    ] {
        fixture(source, |lua, closure, site, code| {
            let run = |lua: &mut Lua, native| {
                lua.enter(|ctx| {
                    with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                        position(host, ctx, site.pc);
                        let capture = host.with_registers(|_, mut registers| {
                            let Value::Function(Function::Closure(callee)) =
                                registers.stack_frame[usize::from(site.function.0)]
                            else {
                                panic!();
                            };
                            let capture = registers
                                .open_test_upvalue(&ctx, RegisterIndex(site.function.0 + 1));
                            callee.set_upvalue(&ctx, usize::from(site.pattern.upvalue), capture);
                            capture
                        });
                        let before = stats(ctx);
                        if native {
                            assert_eq!(invoke(ctx, host, &site, &code, 64), Some(expected));
                        } else {
                            assert!(host.run(ctx, expected.0 + expected.1, 64, 4).result.is_ok());
                        }
                        let value = if expected.1 == 1 {
                            let crate::closure::UpValueState::Closed(value) = capture.get() else {
                                panic!("callee alias was not closed");
                            };
                            assert!(matches!(value, Value::Integer(5)));
                            Slot::from_value(value)
                        } else {
                            host.with_registers(|_, registers| {
                                assert_eq!(*registers.pc, 0);
                                let value = registers.get_upvalue(&ctx, capture);
                                assert!(matches!(value, Value::Nil));
                                Slot::from_value(value)
                            })
                        };
                        (trace(ctx, host, before), value.tag, value.bits)
                    })
                })
            };
            assert_eq!(run(lua, true), run(lua, false));
        });
    }
}

#[test]
fn panic_payload_resumes_after_session_borrow_release() {
    use std::panic::{catch_unwind, panic_any, AssertUnwindSafe};

    #[derive(Debug, PartialEq)]
    struct Payload(u64);

    fixture(ADD, |lua, closure, site, _code| {
        lua.enter(|ctx| {
            with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                position(host, ctx, site.pc);
                let mut scratch = [MaybeUninit::uninit(); 256];
                let mut session = Session::new(ctx, host, &site, &mut scratch);
                session.panic = catch_unwind(AssertUnwindSafe(|| panic_any(Payload(731)))).err();
                let payload = match catch_unwind(AssertUnwindSafe(|| session.finish())) {
                    Err(payload) => payload,
                    Ok(_) => panic!("session swallowed panic payload"),
                };
                assert_eq!(*payload.downcast::<Payload>().unwrap(), Payload(731));
                host.with_registers(|_, registers| assert_eq!(*registers.pc, site.pc));
                assert!(ctx.jit().0.try_borrow_mut().is_ok());
            });
        });
    });
}

#[test]
fn consumed_call_vm_error_is_returned_without_charging_an_unsuccessful_prefix() {
    unsafe extern "C" fn faulty_enter(
        data: *mut c_void,
        pc: u64,
        function: u32,
        arguments: u32,
    ) -> *mut NativeFrame {
        unsafe { &mut *data.cast::<Session<'_, '_, '_>>() }
            .host
            .test_variable_stack();
        unsafe { enter(data, pc, function, arguments) }
    }

    fixture_hooks(
        ADD,
        Hooks {
            enter: faulty_enter,
            leave,
        },
        |lua, closure, site, code| {
            lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let fuel = host.fuel().remaining();
                    let outcome = invoke_result(ctx, host, &site, &code, 64, 4).unwrap();
                    assert_eq!((outcome.calls, outcome.returns), (1, 0));
                    assert!(matches!(
                        outcome.result,
                        Err(crate::thread::VMError::ExpectedVariableStack(false))
                    ));
                    assert_eq!(host.fuel().remaining(), fuel - 4);
                    host.with_registers(|_, registers| assert_eq!(*registers.pc, site.pc + 1));
                });
            });
        },
    );
}

#[test]
fn generated_return_vm_error_preserves_canonical_effects_and_fuel() {
    unsafe extern "C" fn faulty_enter(
        data: *mut c_void,
        pc: u64,
        function: u32,
        arguments: u32,
    ) -> *mut NativeFrame {
        let frame = unsafe { enter(data, pc, function, arguments) };
        if !frame.is_null() {
            unsafe { &mut *data.cast::<Session<'_, '_, '_>>() }
                .host
                .test_variable_stack();
        }
        frame
    }

    fixture_hooks(
        ADD,
        Hooks {
            enter: faulty_enter,
            leave,
        },
        |lua, closure, site, code| {
            let run = |lua: &mut Lua, native| {
                lua.enter(|ctx| {
                    with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                        position(host, ctx, site.pc);
                        let before = stats(ctx);
                        let result = if native {
                            let outcome = invoke_result(ctx, host, &site, &code, 64, 0).unwrap();
                            assert_eq!((outcome.calls, outcome.returns), (1, 1));
                            outcome.result
                        } else {
                            assert!(host.run(ctx, 1, 64, 4).result.is_ok());
                            host.test_variable_stack();
                            host.run(ctx, 1, 64, 4).result
                        };
                        assert!(matches!(
                            result,
                            Err(crate::thread::VMError::ExpectedVariableStack(false))
                        ));
                        let trace = trace(ctx, host, before);
                        (
                            trace.frames,
                            trace.slots,
                            trace.open,
                            trace.fuel,
                            trace.dispatches,
                        )
                    })
                })
            };
            assert_eq!(run(lua, true), run(lua, false));
        },
    );
}

#[test]
fn generated_hook_panic_resumes_only_after_native_return() {
    unsafe extern "C" fn faulty_leave(
        data: *mut c_void,
        frame: *mut NativeFrame,
        pc: u64,
        start: u32,
    ) -> u32 {
        unsafe { leave(data, frame, pc + 1, start) }
    }

    fixture_hooks(
        ADD,
        Hooks {
            enter,
            leave: faulty_leave,
        },
        |lua, closure, site, code| {
            lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    position(host, ctx, site.pc);
                    let payload = match catch_unwind(AssertUnwindSafe(|| {
                        invoke_result(ctx, host, &site, &code, 64, 0)
                    })) {
                        Err(payload) => payload,
                        Ok(_) => panic!("generated hook swallowed panic"),
                    };
                    assert!(payload
                        .downcast::<String>()
                        .unwrap()
                        .contains("assertion `left == right` failed"));
                    host.with_registers(|_, registers| assert_eq!(*registers.pc, 0));
                    assert!(ctx.jit().0.try_borrow_mut().is_ok());
                });
            });
        },
    );
}
