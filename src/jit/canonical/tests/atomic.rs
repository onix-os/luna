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
    let mut stats = ctx.jit().interpreter_stats();
    stats.dispatches = 2;
    stats.reported_instructions = Some(0);
    drop(stats);
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
                                    (trace(ctx, host, before), host.fuel().is_interrupted())
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
