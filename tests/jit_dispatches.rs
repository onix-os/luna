#![cfg(feature = "jit")]

use luna::{opcode::Operation, Closure, Executor, Fuel, JitConfig, JitMode, Lua};

#[test]
fn native_head_guard_fallback_does_not_retry_the_first_native_attempt() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let factory = lua.enter(|ctx| {
        let closure = Closure::load(
            ctx,
            Some("native-head-guard"),
            b"return function(x) return x+1 end",
        )
        .unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    lua.finish(&factory).unwrap();
    let function = lua.enter(|ctx| {
        let function = ctx
            .fetch(&factory)
            .take_result::<luna::Function>(ctx)
            .unwrap()
            .unwrap();
        ctx.stash(function)
    });
    if lua.jit_capabilities().supported_target {
        lua.prepare_jit().unwrap();
    } else {
        assert!(matches!(
            lua.prepare_jit(),
            Err(luna::JitError::Unavailable(_))
        ));
    }
    let before = lua.jit_stats();
    let executor = lua.enter(|ctx| {
        let value = luna::Table::new(&ctx);
        ctx.stash(Executor::start(ctx, ctx.fetch(&function), (value,)))
    });
    assert!(lua.execute::<()>(&executor).is_err());
    let after = lua.jit_stats();
    if lua.jit_capabilities().supported_target {
        assert_eq!(after.guard_exits - before.guard_exits, 1);
        assert_eq!(after.code_lookups - before.code_lookups, 1);
        assert_eq!(after.native_entries - before.native_entries, 1);
    } else {
        assert_eq!(after.guard_exits - before.guard_exits, 0);
        assert_eq!(after.code_lookups - before.code_lookups, 0);
        assert_eq!(after.native_entries - before.native_entries, 0);
    }
    assert_eq!(after.total_dispatches - before.total_dispatches, 1);
}

#[test]
fn straight_line_dispatches_include_returns_and_failing_opcodes() {
    for (source, fails, arithmetic) in [
        ("return", false, false),
        ("local a,b=... return a+b", false, true),
        ("local a,b=... local c=a+b return c.x", true, true),
        ("local a,b=... local c=a+b return c()", true, true),
        ("local a,b=... local c=a+b local r=c() return r", true, true),
    ] {
        for budget in [-1, 0, 1, 64, 65536] {
            for mode in [JitMode::Off, JitMode::Auto] {
                let mut lua = Lua::empty();
                lua.set_jit_config(JitConfig {
                    mode,
                    hot_threshold: u32::MAX,
                    ..Default::default()
                })
                .unwrap();
                let (executor, expected) = lua.enter(|ctx| {
                    let closure =
                        Closure::load(ctx, Some("dispatches"), source.as_bytes()).unwrap();
                    let final_pc = closure
                        .prototype()
                        .opcodes
                        .iter()
                        .position(|op| {
                            matches!(
                                op.decode(),
                                Operation::GetTable { .. }
                                    | Operation::Call { .. }
                                    | Operation::TailCall { .. }
                                    | Operation::Return { .. }
                            )
                        })
                        .unwrap();
                    (
                        ctx.stash(Executor::start(ctx, closure.into(), (3, 4))),
                        final_pc as u64 + 1,
                    )
                });
                let native = mode == JitMode::Auto && lua.jit_capabilities().supported_target;
                if native {
                    assert_eq!(lua.prepare_jit().unwrap(), 1);
                }
                let mut finished = false;
                for _ in 0..100 {
                    finished = lua.enter(|ctx| {
                        ctx.fetch(&executor)
                            .step(ctx, &mut Fuel::with(budget))
                            .unwrap()
                    });
                    lua.gc_collect();
                    if finished {
                        break;
                    }
                }
                assert!(finished);
                let result = lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<()>(ctx)?);
                assert_eq!(result.is_err(), fails);
                let stats = lua.jit_stats();
                assert_eq!(
                    stats.total_dispatches, expected,
                    "{source}, {mode:?}, {budget}"
                );
                assert!(stats.total_dispatches > stats.native_instructions);
                if native && arithmetic {
                    assert!(stats.native_instructions > 0);
                } else if !native {
                    assert_eq!(stats.native_instructions, 0);
                }
                lua.clear_jit_cache();
                lua.set_jit_config(JitConfig::default()).unwrap();
                assert_eq!(lua.jit_stats().total_dispatches, expected);
            }
        }
    }
}

#[test]
fn loops_calls_tail_calls_and_caught_errors_match_reference_dispatches() {
    for source in [
        "local function f(x) return x+1 end local n=0 for i=1,100 do n=f(n) end return n",
        "local function f(n) if n==0 then return 100 end return f(n-1) end return f(100)",
        "local n=0 for i=1,100 do local ok=pcall(function() return i.x end) if not ok then n=n+1 end end return n",
    ] {
        for budget in [0, 1, 64, 65536] {
            let run = |mode| {
                let mut lua = Lua::core();
                lua.set_jit_config(JitConfig {
                    mode,
                    hot_threshold: u32::MAX,
                    ..Default::default()
                })
                .unwrap();
                let executor = lua.enter(|ctx| {
                    let closure = Closure::load(ctx, Some("dispatches"), source.as_bytes())
                        .unwrap();
                    ctx.stash(Executor::start(ctx, closure.into(), ()))
                });
                let native = mode == JitMode::Auto && lua.jit_capabilities().supported_target;
                if native {
                    while lua.prepare_jit().unwrap() != 0 {}
                }
                let mut trace = Vec::new();
                for _ in 0..10_000 {
                    let slice = lua.enter(|ctx| {
                        let mut fuel = Fuel::with(budget);
                        let executor = ctx.fetch(&executor);
                        let done = executor.step(ctx, &mut fuel).unwrap();
                        (done, executor.mode(), fuel.remaining())
                    });
                    trace.push(slice);
                    lua.gc_collect();
                    if slice.0 {
                        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 100);
                        let stats = lua.jit_stats();
                        assert!(stats.total_dispatches > stats.native_instructions);
                        if native {
                            assert!(stats.native_instructions > 0);
                        }
                        return (stats.total_dispatches, trace);
                    }
                }
                panic!("dispatch scenario did not finish");
            };
            assert_eq!(run(JitMode::Off), run(JitMode::Auto), "{source}, {budget}");
        }
    }
}
