//! Native entries that leave native code after only a few bytecodes cost more than they save, so
//! the frame backs off from native dispatch for a while. Long native runs are unaffected.

#![cfg(feature = "jit")]

use luna::{Callback, CallbackReturn, Closure, Executor, JitConfig, JitMode, JitStats, Lua};

fn run(source: &str) -> Option<(i64, JitStats)> {
    let mut lua = Lua::core();
    if !lua.jit_capabilities().supported_target {
        return None;
    }
    let config = JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    };
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
fn a_loop_around_a_metamethod_backs_off_native_entry() {
    let Some((value, stats)) = run(
        "local mt = {} mt.__add = function(a, b) return setmetatable({n = a.n + b.n}, mt) end \
         local sum = setmetatable({n = 0}, mt) local one = setmetatable({n = 1}, mt) \
         for i = 1, 4000 do sum = sum + one end return sum.n",
    ) else {
        return;
    };
    assert_eq!(value, 4000);
    assert!(stats.native_entries > 0, "{stats:?}");
    assert!(stats.native_entries < 400, "{stats:?}");
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
fn a_loop_that_ends_with_a_short_run_stays_native_on_later_runs() {
    let mut lua = Lua::core();
    if !lua.jit_capabilities().supported_target {
        return;
    }
    let config = JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    };
    lua.set_jit_config(config).unwrap();
    let closure = lua
        .try_enter(|ctx| {
            let source = b"local s = 0 for i = 1, 20000 do s = s + i end return s";
            Ok(ctx.stash(Closure::load(ctx, None, &source[..])?))
        })
        .unwrap();
    let run = |lua: &mut Lua| {
        let executor = lua.enter(|ctx| {
            let closure = ctx.fetch(&closure);
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        let before = lua.jit_stats().native_instructions;
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 200_010_000);
        lua.jit_stats().native_instructions - before
    };
    run(&mut lua);
    lua.prepare_jit().unwrap();
    let first = run(&mut lua);
    assert!(first > 30_000, "{first}");
    for later in 0..3 {
        let native = run(&mut lua);
        assert!(native * 10 >= first * 9, "run {later}: {native} of {first}");
    }
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
