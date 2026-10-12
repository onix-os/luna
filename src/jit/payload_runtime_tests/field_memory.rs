use super::*;
use crate::{Callback, CallbackReturn, Table, Thread, Value};

#[test]
fn field_memory_runtime_panic_materializes_scalars_and_recovers() {
    let mut previous = None;
    for failure in [backend::Failure::None, backend::Failure::RequireFieldMemory] {
        let mut lua = state(JitMode::Auto, false);
        lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = failure);
        let (executor, thread) = lua.enter(|ctx| {
            let target = Table::new(&ctx);
            target.set(ctx, "x", 2).unwrap();
            ctx.set_global("target", target);
            let closure = Closure::load(
                ctx,
                None,
                b"local n=1 n=n+39 local t={} t.value=n+2 local alias=target return alias.x+n",
            )
            .unwrap();
            let thread = Thread::new(ctx);
            thread.start(ctx, closure.into(), ()).unwrap();
            let executor = Executor::run(&ctx, thread).unwrap();
            (ctx.stash(executor), ctx.stash(thread))
        });
        lua.prepare_jit().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lua.enter(|ctx| {
                let target: Table = ctx.get_global("target").unwrap();
                let _borrow = target.into_inner().borrow_mut(&ctx);
                ctx.fetch(&executor)
                    .step(ctx, &mut Fuel::with(100))
                    .unwrap();
            });
        }));
        assert!(result.is_err());
        lua.enter(|ctx| {
            let thread = ctx.fetch(&thread).into_inner().borrow();
            let stack = thread.stack().borrow();
            assert!(stack.iter().any(|value| matches!(value, Value::Integer(40))));
            assert!(stack.iter().any(|value| matches!(value, Value::Table(table) if matches!(table.get_value(ctx, "value"), Value::Integer(42)))));
            drop(stack);
            drop(thread);
            ctx.fetch(&executor).stop(&ctx);
        });
        let stats = lua.jit_stats();
        assert_eq!(stats.compilation_failures, 0);
        assert_eq!(stats.native_panic_exits, 1);
        assert_eq!(stats.native_entries, 1);
        if let Some(expected) = previous {
            assert_eq!(counters(stats), expected);
        }
        previous = Some(counters(stats));
        lua.gc_collect();
        let next = source(&mut lua, "local n=40 return n+2");
        assert_eq!(lua.execute::<i64>(&next).unwrap(), 42);
        assert_eq!(lua.jit_stats().native_panic_exits, 1);
    }
}

#[test]
fn field_memory_runtime_retained_alias_and_relocated_stack_match_every_step() {
    for (script, expected) in [
        ("local n=0 local s=0 capture_stack() for i=1,80 do n=n+1 mutate_stack(i*3) s=s+n end return s", 9720),
        ("local n=0 local s=0 capture_stack() for i=1,80 do n=n+1 mutate_stack(i+0.5) s=s+n end return s", 3280),
        ("local n=0 local s=0 capture_stack() for i=1,80 do n=n+1 mutate_stack({n=i*3}) s=s+n.n n=0 end return s", 9720),
        ("local n=0 local s=0 capture_stack() for i=1,80 do n=n+1 mutate_stack(i%2==0) if n then s=s+1 end n=0 end return s", 40),
    ] {
        for budget in [0, 1, 7, 64, 10000] {
            let mut traces = Vec::new();
            let mut native_counters = None;
            for mode in 0..3 {
                let mut lua = state(if mode == 0 { JitMode::Off } else { JitMode::Auto }, false);
                lua.enter(|ctx| {
                    if mode == 2 { ctx.jit().0.borrow_mut().memory_failure = backend::Failure::RequireFieldMemory; }
                    ctx.set_global("relocated", false);
                    ctx.set_global("capture_stack", Callback::from_fn(&ctx, |ctx, exec, _| {
                        let base = exec.upper_lua_frame().unwrap().base;
                        let values = exec.current_thread().thread.into_inner().borrow().stack();
                        ctx.set_global("mutate_stack", Callback::from_fn_with(&ctx, (values, base), |root, ctx, _, mut args| {
                            let value: Value = args.consume(ctx)?;
                            if !ctx.get_global::<bool>("relocated")? {
                                let mut values = root.0.borrow_mut(&ctx);
                                let mut replacement = Vec::with_capacity_in(values.len() + 1024, values.allocator().clone());
                                replacement.extend_from_slice(&values);
                                assert_ne!(replacement.as_ptr(), values.as_ptr());
                                *values = replacement;
                                drop(values);
                                ctx.set_global("relocated", true);
                            }
                            root.0.borrow_mut(&ctx)[root.1] = value;
                            Ok(CallbackReturn::Return)
                        }));
                        Ok(CallbackReturn::Return)
                    }));
                });
                let executor = source(&mut lua, script);
                lua.prepare_jit().unwrap();
                let mut trace = Vec::new();
                for _ in 0..10000 {
                    let step = lua.enter(|ctx| {
                        let mut fuel = Fuel::with(budget);
                        let done = ctx.fetch(&executor).step(ctx, &mut fuel).unwrap();
                        (done, fuel.remaining())
                    });
                    trace.push(step);
                    lua.gc_collect();
                    lua.prepare_jit().unwrap();
                    if step.0 { break; }
                }
                assert!(trace.last().unwrap().0, "{script}, budget {budget}");
                assert_eq!(lua.execute::<i64>(&executor).unwrap(), expected);
                assert!(lua.enter(|ctx| ctx.get_global::<bool>("relocated").unwrap()));
                if mode != 0 {
                    let stats = lua.jit_stats();
                    assert_eq!(stats.compilation_failures, 0);
                    assert!(stats.native_entries >= 80);
                    if let Some(expected) = native_counters { assert_eq!(counters(stats), expected); }
                    native_counters = Some(counters(stats));
                }
                traces.push(trace);
            }
            assert_eq!(traces[0], traces[1]);
            assert_eq!(traces[0], traces[2]);
        }
    }
}
