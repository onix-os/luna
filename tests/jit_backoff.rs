//! Native entries that stop at a call after only a few bytecodes cost more than they save, so
//! the frame backs off from native dispatch for a while. Long native runs are unaffected.

#![cfg(feature = "jit")]

use luna::{Callback, CallbackReturn, Closure, Executor, JitConfig, JitMode, JitStats, Lua};

fn run(source: &str) -> Option<(i64, JitStats)> {
    let mut lua = Lua::core();
    if !lua.jit_capabilities().supported_target {
        return None;
    }
    let mut config = JitConfig::default();
    config.mode = JitMode::Auto;
    config.hot_threshold = 1;
    lua.set_jit_config(config).unwrap();
    let executor = lua
        .try_enter(|ctx| {
            ctx.set_global(
                "host_increment",
                Callback::from_fn(&ctx, |ctx, _, mut stack| {
                    let value: i64 = stack.consume(ctx)?;
                    stack.replace(ctx, value + 1);
                    Ok(CallbackReturn::Return)
                }),
            );
            let closure = Closure::load(ctx, None, source.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    lua.prepare_jit().unwrap();
    let value = lua.execute::<i64>(&executor).unwrap();
    Some((value, lua.jit_stats()))
}

#[test]
fn a_loop_around_a_host_call_backs_off_native_entry() {
    let Some((value, stats)) =
        run("local s = 0 for i = 1, 5000 do s = s + host_increment(i) end return s")
    else {
        return;
    };
    assert_eq!(value, 12_507_500);
    assert!(stats.native_entries > 0, "{stats:?}");
    assert!(stats.native_entries < 200, "{stats:?}");
}

#[test]
fn a_numeric_loop_stays_native() {
    let Some((value, stats)) = run("local s = 0 for i = 1, 100000 do s = s + i end return s")
    else {
        return;
    };
    assert_eq!(value, 5_000_050_000);
    assert!(stats.native_instructions > 100_000, "{stats:?}");
}

#[test]
fn long_native_work_before_each_call_stays_native() {
    let Some((value, stats)) = run(
        "local s = 0 for i = 1, 2000 do for j = 1, 50 do s = s + j end s = s + host_increment(0) end return s",
    ) else {
        return;
    };
    assert_eq!(value, 2000 * (1275 + 1));
    assert!(stats.native_instructions > 100_000, "{stats:?}");
}
