use super::*;

fn attempt<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    site: &Site,
    code: &CallCode,
    budget: u32,
    prefix: u32,
) -> bool {
    if budget < 4 || site.returns != 0 {
        return false;
    }
    let spec = host.with_registers(|caller, registers| {
        if *registers.pc != site.pc
            || ctx
                .jit_registry()
                .borrow()
                .identity(ctx, caller.prototype())
                != Some(site.caller)
        {
            return None;
        }
        let Value::Function(Function::Closure(callee)) =
            *registers.stack_frame.get(usize::from(site.function.0))?
        else {
            return None;
        };
        if !matches_callee(ctx, site, None, callee)
            || usize::from(callee.prototype().stack_size) != site.registers
        {
            return None;
        }
        let capture = match registers.projection_origin(
            callee
                .upvalues()
                .get(usize::from(site.pattern.upvalue))?
                .get(),
        )? {
            Origin::Upper(index, Value::Integer(_)) => (true, index),
            Origin::Register(index, Value::Integer(_)) => (false, index),
            _ => return None,
        };
        Some(crate::thread::activation::atomic_call::Spec {
            callee,
            function: site.function,
            arguments: site.arguments,
            capture,
            read: site.pattern.read,
            result: site.pattern.result,
            start: site.start,
            prefix,
        })
    });
    let Some(spec) = spec else {
        return false;
    };
    if !host.atomic_call(spec, |arguments, capture| {
        let mut frame = code.compact().prepare_arguments(arguments, capture)?;
        let output = code.compact().invoke(&mut frame).unwrap();
        Some((output.read, output.result, output.capture))
    }) {
        return false;
    }
    for _ in 0..2 {
        let mut stats = ctx.jit().interpreter_stats();
        stats.dispatches = 1;
        stats.reported_instructions = Some(0);
    }
    let mut manager = ctx.jit().0.borrow_mut();
    manager.stats.native_upvalue_reads = manager.stats.native_upvalue_reads.saturating_add(1);
    manager.stats.native_upvalue_writes = manager.stats.native_upvalue_writes.saturating_add(1);
    manager.stats.record_native_exit(&Exit {
        pc: 3,
        instructions: 3,
        reason: Kind::Interpreter as u32,
    });
    true
}

fn counters(ctx: Context<'_>) -> [u64; 8] {
    let stats = ctx.jit().0.borrow().stats;
    [
        stats.interpreted_slices,
        stats.interpreted_instructions,
        stats.native_entries,
        stats.native_instructions,
        stats.total_dispatches,
        stats.native_upvalue_reads,
        stats.native_upvalue_writes,
        stats.native_interpreter_exits,
    ]
}

#[test]
fn atomic_native_pairs_match_complete_physical_state_and_fuel() {
    let mut completed = 0;
    let mut covered = [false; 8];
    for (case, source) in [
        ADD,
        &b"local n=7 local function f(v) n=n-v end f(2) return n"[..],
        &b"local n=7 local function f(v) n=n*v end f(2) return n"[..],
        &b"local n=9223372036854775807 local function f(v) n=n+v end f(2) return n"[..],
        &b"local n=7 local function f() n=n+2 end f() return n"[..],
        &b"local n=7 local function f(v,...) n=n+v end f(2,3) return n"[..],
        &b"local n=7 local function f(v,w) n=n+v end f(2,{}) return n"[..],
        &b"local n=7 local function f(v) n=n+v end f(2.5) return n"[..],
    ]
    .into_iter()
    .enumerate()
    {
        fixture(source, |lua, closure, site, code| {
            lua.enter(|ctx| {
                for budget in [0, 3, 4, 64] {
                    for fuel in [-1, 0, 8, 9, 10, 13, 64, i32::MAX] {
                        for prefix in [0, 4, 64] {
                            for interrupted in [false, true] {
                                let mut run = |atomic| with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
                                    position(host, ctx, site.pc);
                                    let mut available = Fuel::with(fuel);
                                    if interrupted { available.interrupt(); }
                                    host.test_fuel(available);
                                    let before = stats(ctx);
                                    let counts = counters(ctx);
                                    let prior = trace(ctx, host, before);
                                    if atomic && attempt(ctx, host, &site, &code, budget, prefix) {
                                        completed += 1;
                                        covered[case] = true;
                                    } else {
                                        assert_eq!(trace(ctx, host, before), prior);
                                        let mut scratch = [MaybeUninit::uninit(); 256];
                                        let mut session = Session::new(ctx, host, &site, &mut scratch);
                                        session.prefix = prefix;
                                        session.invoke_compact(&code, budget);
                                        session.finish().result.unwrap();
                                    }
                                    let after = counters(ctx);
                                    (trace(ctx, host, before), host.fuel().is_interrupted(), std::array::from_fn::<_, 8, _>(|i| after[i] - counts[i]))
                                });
                                assert_eq!(run(true), run(false), "budget={budget},fuel={fuel},prefix={prefix},interrupt={interrupted},source={source:?}");
                            }
                        }
                    }
                }
            });
        });
    }
    assert_eq!(covered, [true, true, true, true, true, false, true, false]);
    assert_eq!(completed, 86);
}

#[test]
fn atomic_pairs_preserve_captures_in_an_upper_physical_frame() {
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let root = Closure::load(ctx, None, &b"local n=7 local function outer() local function f(v) n=n+v end f(2) return n end local result=outer() return result"[..]).unwrap();
        let caller = root.prototype().prototypes[0];
        let callee = caller.prototypes[0];
        let caller_snapshot = Snapshot::new(&caller, 4096, 2 * 1024 * 1024).unwrap();
        let callee_snapshot = Snapshot::new(&callee, 4096, 2 * 1024 * 1024).unwrap();
        let pc = caller_snapshot.operations.iter().position(|op| matches!(op, Operation::Call { .. })).unwrap();
        let limits = work::Limits::from(&JitConfig::default());
        let plan = Plan::new(&caller_snapshot, &callee_snapshot, pc, limits).unwrap();
        let Operation::Return { start, count } = callee_snapshot.operations[3] else { panic!() };
        let registry = ctx.jit_registry().borrow();
        let site = Site {
            caller: registry.identity(ctx, caller).unwrap(),
            callee: registry.identity(ctx, callee).unwrap(),
            pc,
            function: plan.function,
            arguments: plan.arguments,
            pattern: plan.pattern,
            registers: callee_snapshot.registers,
            start,
            returns: count.to_constant().unwrap(),
        };
        drop(registry);
        let ledger = Ledger::new(8 * 1024 * 1024);
        let code = compile(&plan, Hooks { enter, leave }, MappingCounter::new(Ledger::child(2 * 1024 * 1024, ledger.clone())), 2 * 1024 * 1024, BudgetAllocator(Ledger::child(2 * 1024 * 1024, ledger)), limits, backend::Failure::None, LinkFault::None).unwrap();
        let run = |atomic| with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
            host.run(ctx, 1, 64, 4).result.unwrap();
            host.with_registers(|closure, registers| {
                assert!(Gc::ptr_eq(closure.prototype(), caller));
                assert_eq!(*registers.pc, 0);
            });
            position(host, ctx, pc);
            host.with_registers(|_, registers| {
                let Value::Function(Function::Closure(callee)) = registers.stack_frame[usize::from(site.function.0)] else { panic!() };
                assert!(matches!(registers.projection_origin(callee.upvalues()[0].get()), Some(Origin::Upper(_, Value::Integer(7)))));
            });
            let before = stats(ctx);
            if atomic {
                assert!(attempt(ctx, host, &site, &code, 64, 0));
            } else {
                let mut scratch = [MaybeUninit::uninit(); 256];
                let mut session = Session::new(ctx, host, &site, &mut scratch);
                session.invoke_compact(&code, 64);
                session.finish().result.unwrap();
            }
            trace(ctx, host, before)
        });
        assert_eq!(run(true), run(false));
    });
}

#[test]
fn atomic_capacity_and_tail_capture_refusals_preserve_physical_fallback() {
    for width in [None, Some(256)] {
        fixture_layout(
            ADD,
            Hooks { enter, leave },
            None,
            width,
            |lua, closure, site, code| {
                lua.enter(|ctx| {
                    for tail_capture in [false, true] {
                        if width.is_none() && !tail_capture {
                            continue;
                        }
                        let run = |try_atomic| {
                            with_test_thread(
                                ctx,
                                ctx.fetch(&closure),
                                &mut Fuel::with(10000),
                                |host| {
                                    position(host, ctx, site.pc);
                                    if tail_capture {
                                        host.with_registers(|_, mut registers| {
                                            let Value::Function(Function::Closure(callee)) =
                                                registers.stack_frame[usize::from(site.function.0)]
                                            else {
                                                panic!()
                                            };
                                            let upvalue = registers.open_test_upvalue(
                                                &ctx,
                                                RegisterIndex(site.function.0 + 1),
                                            );
                                            callee.set_upvalue(
                                                &ctx,
                                                usize::from(site.pattern.upvalue),
                                                upvalue,
                                            );
                                        });
                                    }
                                    let before = stats(ctx);
                                    if try_atomic {
                                        let prior = trace(ctx, host, before);
                                        assert!(!attempt(ctx, host, &site, &code, 64, 0));
                                        assert_eq!(trace(ctx, host, before), prior);
                                    }
                                    let mut scratch = [MaybeUninit::uninit(); 256];
                                    let mut session = Session::new(ctx, host, &site, &mut scratch);
                                    session.invoke_compact(&code, 64);
                                    session.finish().result.unwrap();
                                    trace(ctx, host, before)
                                },
                            )
                        };
                        assert_eq!(run(true), run(false));
                    }
                });
            },
        );
    }
}
