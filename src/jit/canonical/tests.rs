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
fn generated_shadow_entry_rejects_hooks_owner_source_and_stale_code() {
    fixture(ADD, |lua, closure, site, _code| {
        lua.set_jit_config(JitConfig {
            mode: crate::JitMode::Auto,
            ..JitConfig::default()
        })
        .unwrap();
        lua.enter(|ctx| {
            let prototype = ctx.fetch(&closure).prototype();
            let snapshot = || Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap();
            ctx.jit().compile(site.caller, snapshot()).unwrap();
            let prepared = ctx.jit().lookup(site.caller).unwrap();
            let foreign = crate::jit::Runtime::new();
            foreign.0.borrow_mut().config.mode = crate::JitMode::Auto;
            foreign
                .0
                .borrow_mut()
                .tracked
                .insert(site.caller, Default::default());
            foreign.compile(site.caller, snapshot()).unwrap();
            let foreign_code = foreign.lookup(site.caller).unwrap();
            for case in 0..8 {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                    let mut shadow = host.with_registers(|caller, registers| {
                        crate::jit::caller_shadow::Shadow::<256>::capture(caller, &registers)
                            .unwrap()
                    });
                    let mut removed = None;
                    match case {
                        0 => ctx.jit().0.borrow_mut().config.mode = crate::JitMode::Off,
                        1 | 2 => {
                            let hook = crate::Callback::from_fn(&ctx, |_, _, _| {
                                panic!("declined hook ran")
                            });
                            ctx.set_debug_hook(hook.into(), false, 1);
                            if case == 2 {
                                ctx.suppress_hook_at(1);
                            }
                        }
                        3 => {}
                        4 => removed = ctx.jit().0.borrow_mut().code.remove(&site.caller),
                        5 => host.test_replace_closure(rebind(ctx, ctx.fetch(&closure), prototype)),
                        6 => host.with_registers(|_, registers| *registers.pc = site.pc),
                        7 => shadow.slots_mut()[0].tag = u64::MAX,
                        _ => unreachable!(),
                    }
                    let before = stats(ctx);
                    let original = trace(ctx, host, before);
                    assert_eq!(
                        shadow.invoke(
                            ctx,
                            if case == 3 { &foreign_code } else { &prepared },
                            host,
                            64
                        ),
                        None,
                        "case={case}"
                    );
                    assert_eq!(trace(ctx, host, before), original, "case={case}");
                    if let Some(removed) = removed {
                        ctx.jit().0.borrow_mut().code.insert(site.caller, removed);
                    }
                    ctx.jit().0.borrow_mut().config.mode = crate::JitMode::Auto;
                    ctx.set_debug_hook(Value::Nil, false, 0);
                });
            }
        });
    });
}

#[test]
fn generated_caller_callee_caller_preserves_pending_values_and_full_trace() {
    fixture(
        b"local n=7 local p=10 local function f(v) n=n+v end p=99 n=12 f(5) p=p+n return n,p",
        |lua, closure, site, code| {
            lua.set_jit_config(JitConfig {
                mode: crate::JitMode::Auto,
                ..JitConfig::default()
            })
            .unwrap();
            lua.enter(|ctx| {
                let prototype = ctx.fetch(&closure).prototype();
                ctx.jit()
                    .compile(
                        site.caller,
                        Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap(),
                    )
                    .unwrap();
                let caller_code = ctx.jit().lookup(site.caller).unwrap();
                let start = prototype
                    .opcodes
                    .iter()
                    .position(|op| matches!(op.decode(), Operation::Closure { .. }))
                    .unwrap()
                    + 1;
                assert!(start < site.pc);
                let run = |native: bool, budget: u32| {
                    with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                        position(host, ctx, start);
                        let before = stats(ctx);
                        let native_before = ctx.jit().0.borrow().stats.native_instructions;
                        if native {
                            let mut shadow = host.with_registers(|caller, registers| {
                                crate::jit::caller_shadow::Shadow::<256>::capture(
                                    caller, &registers,
                                )
                                .unwrap()
                            });
                            let mut prefix = 0;
                            for _ in 0..prototype.opcodes.len() {
                                if host.with_registers(|_, registers| *registers.pc == site.pc) {
                                    break;
                                }
                                let completed =
                                    shadow.invoke(ctx, &caller_code, host, budget).unwrap();
                                assert!(completed > 0 && completed <= budget);
                                prefix += completed;
                            }
                            assert_eq!(prefix as usize, site.pc - start);
                            let capture = host.with_registers(|_, registers| {
                                assert_eq!(*registers.pc, site.pc);
                                assert!(matches!(registers.stack_frame[0], Value::Integer(7)));
                                assert!(matches!(registers.stack_frame[1], Value::Integer(10)));
                                let Value::Function(Function::Closure(callee)) =
                                    registers.stack_frame[usize::from(site.function.0)]
                                else {
                                    panic!("expected closure");
                                };
                                callee.upvalues()[usize::from(site.pattern.upvalue)].get()
                            });
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
                            let outcome =
                                invoke_result(ctx, host, &site, &code, 64, prefix).unwrap();
                            assert_eq!((outcome.calls, outcome.returns), (1, 1));
                            outcome.result.unwrap();
                            assert!(host.with_registers(
                                |caller, registers| transfer.resume(caller, &registers)
                            ));
                            host.with_registers(|_, registers| {
                                assert!(matches!(registers.stack_frame[0], Value::Integer(17)));
                                assert!(matches!(registers.stack_frame[1], Value::Integer(10)));
                            });
                            let mut suffix = 0;
                            for _ in 0..prototype.opcodes.len() {
                                if host.with_registers(|caller, registers| {
                                    caller.prototype().opcodes[*registers.pc]
                                        .call_transition()
                                        .is_some()
                                }) {
                                    break;
                                }
                                let completed =
                                    shadow.invoke(ctx, &caller_code, host, budget).unwrap();
                                assert!(completed > 0 && completed <= budget);
                                suffix += completed;
                            }
                            assert!(suffix > 0);
                            let transition = host.with_registers(|caller, mut registers| {
                                assert!(shadow.flush(caller, &mut registers));
                                assert!(matches!(registers.stack_frame[1], Value::Integer(116)));
                                caller.prototype().opcodes[*registers.pc]
                                    .call_transition()
                                    .unwrap()
                            });
                            assert!(matches!(
                                transition,
                                crate::opcode::CallTransition::Return { .. }
                            ));
                            host.native_transition(ctx, transition, suffix).unwrap();
                            assert_eq!(
                                ctx.jit().0.borrow().stats.native_instructions - native_before,
                                u64::from(prefix + 3 + suffix)
                            );
                        } else {
                            ctx.jit().0.borrow_mut().config.mode = crate::JitMode::Off;
                            host.run(ctx, 3, 64, 4).result.unwrap();
                            ctx.jit().0.borrow_mut().config.mode = crate::JitMode::Auto;
                        }
                        trace(ctx, host, before)
                    })
                };
                let interpreted = run(false, 64);
                for budget in [1, 2, 3, 4, 8, 64] {
                    assert_eq!(run(true, budget), interpreted, "budget={budget}");
                }
            });
        },
    );
}

#[test]
fn suspended_caller_prefix_rejects_wrong_pc_host_source_and_overlap() {
    fixture(
        b"local n=7 local p=10 local function f(v) n=n+v end f(2) return n,p",
        |lua, closure, site, _code| {
            for case in 0..5 {
                lua.enter(|ctx| {
                    with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                        position(host, ctx, site.pc);
                        let caller = host.caller_frame().unwrap();
                        let count = usize::from(site.function.0);
                        assert!(host
                            .with_caller_prefix(&caller, count, |_, _, _| panic!(
                                "unconsumed call accepted"
                            ))
                            .is_none());
                        if case == 1 {
                            host.test_replace_closure(
                                Closure::load(ctx, None, b"return 1").unwrap(),
                            );
                        }
                        host.call(ctx, site.function, site.arguments).unwrap();
                        let mut before = stats(ctx);
                        let mut original = trace(ctx, host, before);
                        match case {
                            0 => assert_eq!(
                                host.with_caller_prefix(&caller, count, |_, _, values| values
                                    .len()),
                                Some(count)
                            ),
                            1 => assert!(host
                                .with_caller_prefix(&caller, count, |_, _, _| panic!(
                                    "changed source accepted"
                                ))
                                .is_none()),
                            2 => assert!(host
                                .with_caller_prefix(&caller, count + 1, |_, _, _| panic!(
                                    "callee overlap accepted"
                                ))
                                .is_none()),
                            3 => assert!(host
                                .with_caller_prefix(&caller, usize::MAX, |_, _, _| panic!(
                                    "oversized prefix accepted"
                                ))
                                .is_none()),
                            4 => with_test_thread(
                                ctx,
                                ctx.fetch(&closure),
                                &mut Fuel::with(10000),
                                |foreign| {
                                    position(foreign, ctx, site.pc);
                                    foreign.call(ctx, site.function, site.arguments).unwrap();
                                    before = stats(ctx);
                                    original = trace(ctx, host, before);
                                    assert!(foreign
                                        .with_caller_prefix(&caller, count, |_, _, _| panic!(
                                            "foreign host accepted"
                                        ))
                                        .is_none());
                                },
                            ),
                            _ => unreachable!(),
                        }
                        assert_eq!(trace(ctx, host, before), original);
                    });
                });
            }
        },
    );
}

#[test]
fn caller_shadow_recovers_pending_values_after_partial_calls_and_errors() {
    fixture(
        b"local n=7 local p=10 local function f(v) n=n+v end f(2) return n,p",
        |lua, closure, site, code| {
            assert!(site.function.0 > 1);
            for case in 0..6 {
                let run = |lua: &mut Lua, native: bool| {
                    lua.enter(|ctx| {
                        ctx.set_max_call_depth(if case == 4 { 1 } else { 1000 });
                        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                            position(host, ctx, site.pc);
                            let before = stats(ctx);
                            if native {
                                let caller = host.caller_frame().unwrap();
                                let (mut shadow, capture) =
                                    host.with_registers(|caller, registers| {
                                        let Value::Function(Function::Closure(callee)) =
                                            registers.stack_frame[usize::from(site.function.0)]
                                        else {
                                            panic!("expected closure");
                                        };
                                        (
                                            crate::jit::caller_shadow::Shadow::<256>::capture(
                                                caller, &registers,
                                            )
                                            .unwrap(),
                                            callee.upvalues()[usize::from(site.pattern.upvalue)]
                                                .get(),
                                        )
                                    });
                                shadow.slots_mut()[0] = Slot::from_value(Value::Integer(12));
                                shadow.slots_mut()[1] = Slot::from_value(Value::Integer(99));
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
                                let mut scratch = [MaybeUninit::uninit(); 256];
                                let mut session = Session::new(ctx, host, &site, &mut scratch);
                                assert!(session.preflight(64));
                                if case == 5 {
                                    session.host.test_variable_stack();
                                }
                                unsafe {
                                    code.invoke(
                                        std::ptr::addr_of_mut!(session).cast(),
                                        if case < 4 { case } else { 64 },
                                    );
                                }
                                let outcome = session.finish();
                                assert_eq!((outcome.calls, outcome.returns), (1, 0));
                                assert_eq!(outcome.result.is_err(), case == 5);
                                assert!(transfer.recover(host, &caller));
                            } else {
                                host.with_registers(|_, registers| {
                                    registers.stack_frame[0] = Value::Integer(12);
                                    registers.stack_frame[1] = Value::Integer(99);
                                    registers.stack_frame[usize::from(site.function.0) + 1] =
                                        Value::Integer(5);
                                });
                                if case == 5 {
                                    host.test_variable_stack();
                                }
                                assert_eq!(host.run(ctx, 1, 64, 4).result.is_err(), case == 5);
                                if case < 4 {
                                    assert!(host.run(ctx, 1, 0, 4).result.is_ok());
                                }
                            }
                            let suspended = trace(ctx, host, before);
                            let resumed = if case < 4 {
                                assert!(host.run(ctx, 2, 64, 4).result.is_ok());
                                Some(trace(ctx, host, before))
                            } else {
                                None
                            };
                            (suspended, resumed)
                        })
                    })
                };
                assert_eq!(run(lua, true), run(lua, false), "case={case}");
            }
        },
    );
}

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
    for bound in [false, true] {
        physical_call_replacements(bound);
    }
}

fn physical_call_replacements(bound: bool) {
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
                    let source = bound.then(|| {
                        admission::BoundSource::new(ctx, ctx.jit_registry().borrow(), site.callee)
                            .unwrap()
                    });
                    let mut scratch = [MaybeUninit::uninit(); 256];
                    let mut session = Session::new(ctx, host, &site, &mut scratch);
                    session.source = source.as_ref();
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
    for bound in [false, true] {
        changed_physical_prototype(bound);
    }
}

fn changed_physical_prototype(bound: bool) {
    fixture(ADD, |lua, closure, site, _code| {
        lua.enter(|ctx| {
            with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                position(host, ctx, site.pc);
                let replacement = Closure::load(ctx, None, b"return 0").unwrap();
                let source = bound.then(|| {
                    admission::BoundSource::new(ctx, ctx.jit_registry().borrow(), site.callee)
                        .unwrap()
                });
                let mut scratch = [MaybeUninit::uninit(); 256];
                let mut session = Session::new(ctx, host, &site, &mut scratch);
                session.source = source.as_ref();
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
fn resident_return_error_recovers_pending_values_and_callee_effects() {
    resident_failure(ResidentFailure::ReturnError, false);
}

#[test]
fn resident_enter_panic_recovers_pending_values_beneath_callee() {
    resident_failure(ResidentFailure::EnterPanic, false);
}

#[test]
fn resident_leave_panic_discards_uncommitted_callee_scratch() {
    resident_failure(ResidentFailure::LeavePanic, false);
}

#[test]
fn resident_post_return_panic_preserves_committed_callee_effects() {
    resident_failure(ResidentFailure::ReturnedPanic, false);
}

#[test]
fn native_region_return_error_preserves_caller_and_callee_effects() {
    resident_failure(ResidentFailure::ReturnError, true);
}

#[test]
fn native_region_enter_panic_preserves_caller_beneath_callee() {
    resident_failure(ResidentFailure::EnterPanic, true);
}

#[test]
fn native_region_leave_panic_discards_uncommitted_callee_scratch() {
    resident_failure(ResidentFailure::LeavePanic, true);
}

#[test]
fn native_region_post_return_panic_preserves_committed_effects() {
    resident_failure(ResidentFailure::ReturnedPanic, true);
}

#[test]
fn native_region_paired_fallback_preserves_errors_panics_and_leases() {
    for failure in [
        ResidentFailure::ReturnError,
        ResidentFailure::EnterPanic,
        ResidentFailure::LeavePanic,
        ResidentFailure::ReturnedPanic,
    ] {
        resident_failure_path(failure, true, true);
    }
}

#[derive(Clone, Copy)]
enum ResidentFailure {
    ReturnError,
    EnterPanic,
    LeavePanic,
    ReturnedPanic,
}

fn resident_failure(failure: ResidentFailure, connected: bool) {
    resident_failure_path(failure, connected, false);
}

fn resident_failure_path(failure: ResidentFailure, connected: bool, fallback: bool) {
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

    unsafe fn inject_panic(data: *mut c_void, generated: bool) {
        let session = unsafe { &mut *data.cast::<Session<'_, '_, '_>>() };
        session.panic = Some(
            catch_unwind(AssertUnwindSafe(|| {
                assert_eq!(session.calls, 1);
                if generated {
                    assert_eq!(session.frame.exit.instructions, 3);
                    assert_eq!((session.view.reads, session.view.writes), (1, 1));
                    assert_eq!(unsafe { (*session.view.cell).bits }, 17);
                }
                panic!("resident injected panic");
            }))
            .unwrap_err(),
        );
    }

    unsafe extern "C" fn panic_enter(
        data: *mut c_void,
        pc: u64,
        function: u32,
        arguments: u32,
    ) -> *mut NativeFrame {
        let frame = unsafe { enter(data, pc, function, arguments) };
        if !frame.is_null() {
            unsafe { inject_panic(data, false) };
        }
        std::ptr::null_mut()
    }

    unsafe extern "C" fn panic_leave(
        data: *mut c_void,
        _frame: *mut NativeFrame,
        _pc: u64,
        _start: u32,
    ) -> u32 {
        unsafe { inject_panic(data, true) };
        0
    }

    unsafe extern "C" fn panic_returned(
        data: *mut c_void,
        frame: *mut NativeFrame,
        pc: u64,
        start: u32,
    ) -> u32 {
        let result = unsafe { leave(data, frame, pc, start) };
        unsafe { inject_panic(data, true) };
        result
    }

    fixture(
        if fallback {
            &b"local n=7 local p=10 local function f(v) n=n+v end p=99 n=12 local divisor=2 local q=n%divisor f(5) local a,b,c,d,e=1,2,3,4,5 p=p+n return n,p,a,b,c,d,e"[..]
        } else {
            &b"local n=7 local p=10 local function f(v) n=n+v end p=99 n=12 f(5) local a,b,c,d,e=1,2,3,4,5 p=p+n return n,p,a,b,c,d,e"[..]
        },
        |lua, closure, site, code| {
            drop(code);
            lua.set_jit_config(JitConfig {
                mode: crate::JitMode::Auto,
                ..JitConfig::default()
            })
            .unwrap();
            lua.enter(|ctx| {
                let prototype = ctx.fetch(&closure).prototype();
                let (memory, metadata, limit, limits) = {
                    let manager = ctx.jit().0.borrow();
                    (
                        manager.memory.clone(),
                        manager.metadata.clone(),
                        manager.config.max_code_bytes,
                        work::Limits::from(&manager.config),
                    )
                };
                ctx.jit()
                    .compile(
                        site.caller,
                        Snapshot::new_in(&prototype, 4096, metadata.clone()).unwrap(),
                    )
                    .unwrap();
                let caller_code = ctx.jit().lookup(site.caller).unwrap();
                let start = prototype
                    .opcodes
                    .iter()
                    .position(|op| matches!(op.decode(), Operation::Closure { .. }))
                    .unwrap()
                    + 1;
                let caller = Snapshot::new_in(&prototype, 4096, metadata.clone()).unwrap();
                let callee =
                    Snapshot::new_in(&prototype.prototypes[0], 4096, metadata.clone()).unwrap();
                let plan = Plan::new(&caller, &callee, site.pc, limits).unwrap();
                if connected {
                    let key = crate::jit::pairs::Key {
                        caller: site.caller,
                        callee: site.callee,
                        pc: site.pc,
                    };
                    ctx.jit().test_call_pairs(true);
                    ctx.jit().observe_pair(key);
                    ctx.jit()
                        .compile_pair(
                            key,
                            Snapshot::new_in(&prototype, 4096, metadata.clone()).unwrap(),
                            Snapshot::new_in(&prototype.prototypes[0], 4096, metadata.clone())
                                .unwrap(),
                        )
                        .unwrap();
                }
                let mapped_before = memory.requested();
                let metadata_before = metadata.0.current();
                let code = compile(
                    &plan,
                    match failure {
                        ResidentFailure::ReturnError => Hooks {
                            enter: faulty_enter,
                            leave,
                        },
                        ResidentFailure::EnterPanic => Hooks {
                            enter: panic_enter,
                            leave,
                        },
                        ResidentFailure::LeavePanic => Hooks {
                            enter,
                            leave: panic_leave,
                        },
                        ResidentFailure::ReturnedPanic => Hooks {
                            enter,
                            leave: panic_returned,
                        },
                    },
                    memory.clone(),
                    limit,
                    metadata.clone(),
                    limits,
                    backend::Failure::None,
                    LinkFault::None,
                )
                .unwrap();
                let pair = crate::jit::PreparedPair {
                    program: crate::jit::owner::Shared::try_new(
                        Program {
                            site,
                            code,
                            origin: memory.clone(),
                        },
                        metadata.clone(),
                    )
                    .unwrap(),
                };
                let site = &pair.program.site;
                let replaced = connected.then(|| ctx.jit().test_replace_pair(pair.program.clone()));
                let region = connected.then(|| {
                    crate::jit::native_region::Region::new(
                        ctx,
                        ctx.fetch(&closure),
                        crate::jit::PreparedPair {
                            program: pair.program.clone(),
                        },
                    )
                    .unwrap()
                });
                assert!(start < site.pc);
                let run = |native| {
                    ctx.jit().0.borrow_mut().config.mode = crate::JitMode::Off;
                    with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                        position(host, ctx, start);
                        let before = stats(ctx);
                        let native_before = ctx.jit().0.borrow().stats.native_instructions;
                        let result = if native {
                            ctx.jit().0.borrow_mut().config.mode = crate::JitMode::Auto;
                            let mut shadow = (!connected).then(|| {
                                host.with_registers(|caller, registers| {
                                    crate::jit::caller_shadow::Shadow::<256>::capture(
                                        caller, &registers,
                                    )
                                    .unwrap()
                                })
                            });
                            let prefix = if let Some(shadow) = shadow.as_mut() {
                                shadow.invoke(ctx, &caller_code, host, 64).unwrap()
                            } else {
                                (site.pc - start) as u32
                            };
                            assert_eq!(prefix as usize, site.pc - start);
                            if !connected {
                                host.with_registers(|_, registers| {
                                    assert_eq!(*registers.pc, site.pc);
                                    assert!(matches!(registers.stack_frame[0], Value::Integer(7)));
                                    assert!(matches!(registers.stack_frame[1], Value::Integer(10)));
                                });
                            }
                            let leases = crate::jit::owner::Shared::strong_count(&pair.program);
                            let mut recoveries = 0;
                            let returned = catch_unwind(AssertUnwindSafe(|| {
                                if let Some(region) = &region {
                                    let outcome = region.run(ctx, host, 64, 64).unwrap();
                                    assert_eq!(
                                        (outcome.fragments, outcome.pairs, outcome.slices),
                                        (1, 1, 2)
                                    );
                                    outcome.result
                                } else {
                                    let outcome = crate::jit::chains::shadow_pair(
                                        ctx,
                                        host,
                                        &pair,
                                        &mut shadow,
                                        64,
                                        prefix,
                                        &mut recoveries,
                                    )
                                    .unwrap();
                                    assert_eq!((outcome.calls, outcome.returns), (1, 1));
                                    outcome.result
                                }
                            }));
                            let result = match failure {
                                ResidentFailure::ReturnError => returned.unwrap(),
                                _ => {
                                    let Err(payload) = returned else {
                                        panic!("resident driver swallowed panic");
                                    };
                                    assert_eq!(
                                        *payload.downcast::<&str>().unwrap(),
                                        "resident injected panic"
                                    );
                                    Ok(())
                                }
                            };
                            assert_eq!(recoveries, usize::from(!connected));
                            assert!(shadow.is_none());
                            assert!(ctx.jit().0.borrow().resume_scope.is_none());
                            assert_eq!(
                                crate::jit::owner::Shared::strong_count(&pair.program),
                                leases
                            );
                            assert_eq!(
                                ctx.jit().0.borrow().stats.native_instructions - native_before,
                                u64::from(
                                    prefix - u32::from(fallback)
                                        + if matches!(
                                            failure,
                                            ResidentFailure::ReturnError
                                                | ResidentFailure::ReturnedPanic
                                        ) {
                                            3
                                        } else {
                                            0
                                        }
                                )
                            );
                            result
                        } else {
                            host.run(ctx, 1, 64, 4).result.unwrap();
                            let result = match failure {
                                ResidentFailure::ReturnError => {
                                    host.test_variable_stack();
                                    host.run(ctx, 1, 64, 4).result
                                }
                                ResidentFailure::ReturnedPanic => host.run(ctx, 1, 64, 4).result,
                                _ => Ok(()),
                            };
                            assert_eq!(
                                ctx.jit().0.borrow().stats.native_instructions,
                                native_before
                            );
                            result
                        };
                        match failure {
                            ResidentFailure::ReturnError => assert!(matches!(
                                result,
                                Err(crate::thread::VMError::ExpectedVariableStack(false))
                            )),
                            _ => result.unwrap(),
                        }
                        assert!(ctx.jit().0.try_borrow_mut().is_ok());
                        assert!(ctx.jit_registry().try_borrow_mut(&ctx).is_ok());
                        let trace = trace(ctx, host, before);
                        let callee_instructions = match failure {
                            ResidentFailure::ReturnedPanic => 3,
                            ResidentFailure::ReturnError if native => 3,
                            _ => 0,
                        };
                        assert_eq!(
                            trace.instructions,
                            (site.pc - start) as u64 + callee_instructions
                        );
                        (
                            trace.frames,
                            trace.slots,
                            trace.open,
                            trace.fuel,
                            trace.dispatches,
                        )
                    })
                };
                assert_eq!(run(true), run(false));
                drop(region);
                if let Some(replaced) = replaced {
                    drop(ctx.jit().test_replace_pair(replaced));
                }
                drop(pair);
                assert_eq!(memory.requested(), mapped_before);
                assert_eq!(metadata.0.current(), metadata_before);
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
