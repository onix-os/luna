#![cfg(all(
    feature = "jit",
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExternError, Fuel, Function, JitConfig, JitMode,
    JitStats, Lua, StashedExecutor,
};

#[allow(dead_code)]
#[path = "../examples/jit_support/workloads.rs"]
mod workloads;

fn run(
    source: &[u8],
    mode: JitMode,
    prepare: bool,
    available: i32,
) -> (i64, Vec<(i32, u64)>, JitStats) {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("paired-runtime"), source).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    if prepare {
        lua.prepare_jit().unwrap();
    }
    let (value, trace, stats) = finish(lua, &executor, available);
    (value.unwrap(), trace, stats)
}

fn finish(
    mut lua: Lua,
    executor: &StashedExecutor,
    available: i32,
) -> (Result<i64, ExternError>, Vec<(i32, u64)>, JitStats) {
    let mut trace = Vec::new();
    let mut finished = false;
    for step in 0..20000 {
        let before = lua.jit_stats().installed_regions;
        let (done, remaining) = lua.enter(|ctx| {
            let mut fuel = Fuel::with(available);
            (
                ctx.fetch(executor).step(ctx, &mut fuel).unwrap(),
                fuel.remaining(),
            )
        });
        assert_eq!(
            lua.jit_stats().installed_regions,
            before,
            "compiled inside executor step"
        );
        trace.push((remaining, lua.jit_stats().total_dispatches));
        if done {
            finished = true;
            break;
        }
        if step % 7 == 0 {
            lua.gc_collect();
        }
        for _ in 0..16 {
            if lua.jit_stats().queued_requests == 0 {
                break;
            }
            lua.service_jit().unwrap();
        }
    }
    assert!(finished, "executor did not finish within step bound");
    let value = lua.try_enter(|ctx| ctx.fetch(executor).take_result::<i64>(ctx).unwrap());
    (value, trace, lua.jit_stats())
}

#[test]
fn production_host_releases_parent_stack_before_foreign_capture_callback() {
    use std::{cell::Cell, rc::Rc};
    let source = b"local cb=... local n=0 local function f(v) n=n+v end for i=1,300 do f(i) end cb(f) return n";
    for mode in [JitMode::Off, JitMode::Auto] {
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        let calls = Rc::new(Cell::new(0));
        let saved = calls.clone();
        let executor = lua.enter(|ctx| {
            let callback = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let function: Function = stack.consume(ctx)?;
                let nested = Executor::start(ctx, function, (1,));
                while !nested.step(ctx, &mut Fuel::with(64)).unwrap() {}
                nested.take_result::<()>(ctx).unwrap()?;
                saved.set(saved.get() + 1);
                stack.clear();
                Ok(CallbackReturn::Return)
            });
            let closure = Closure::load(ctx, Some("paired-callback"), source).unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), (callback,)))
        });
        lua.prepare_jit().unwrap();
        let (value, _, stats) = finish(lua, &executor, 64);
        assert_eq!(value.unwrap(), 45151);
        assert_eq!(calls.get(), 1);
        assert_eq!(stats.native_pair_returns > 0, mode == JitMode::Auto);
    }
}

#[test]
fn production_error_fallback_preserves_error_position_and_unsuccessful_slice_fuel() {
    for source in [
        &b"local n=0 local function f(v) n=n+v end for i=1,100 do f(1) end f({}) return n"[..],
        &b"local n=0 local function f(v) n=n+v end for i=1,100 do f(1) end return n+{}"[..],
    ] {
        let execute = |mode| {
            let mut lua = Lua::empty();
            lua.set_jit_config(JitConfig {
                mode,
                hot_threshold: 1,
                ..Default::default()
            })
            .unwrap();
            let executor = lua.enter(|ctx| {
                let closure = Closure::load(ctx, Some("paired-errors"), source).unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            });
            lua.prepare_jit().unwrap();
            let (result, trace, stats) = finish(lua, &executor, 64);
            (result.unwrap_err().to_string(), trace, stats)
        };
        let reference = execute(JitMode::Off);
        let native = execute(JitMode::Auto);
        assert_eq!((native.0, native.1), (reference.0, reference.1));
        assert!(native.2.native_pair_returns > 0);
    }
}

#[test]
fn production_hooks_prevent_pair_execution_and_preserve_hooked_slice_traces() {
    let source = b"local ticks=0 debug.sethook(function() ticks=ticks+1 end,'',7) local n=0 local function f(v) n=n+v end for i=1,300 do f(i) end debug.sethook() return n==45150 and ticks>0 and 42 or -1";
    let execute = |mode| {
        let mut lua = Lua::core();
        lua.load_debug();
        lua.set_jit_config(JitConfig {
            mode,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        let executor = lua.enter(|ctx| {
            let closure = Closure::load(ctx, Some("paired-hooks"), source).unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        lua.prepare_jit().unwrap();
        let (result, trace, stats) = finish(lua, &executor, 64);
        (result.unwrap(), trace, stats)
    };
    let reference = execute(JitMode::Off);
    let native = execute(JitMode::Auto);
    assert_eq!(native.0, 42);
    assert_eq!((native.0, native.1), (reference.0, reference.1));
    assert_eq!(native.2.native_pair_returns, 0);
    assert!(native.2.hook_exits > 0);
}

#[test]
fn production_auto_to_off_clears_pairs_and_resumes_without_native_work() {
    let workload = workloads::WORKLOADS
        .iter()
        .find(|case| case.name == "closure_upvalue")
        .unwrap();
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, workload.source).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    for _ in 0..100 {
        let done = lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut Fuel::with(64)).unwrap());
        assert!(!done);
        for _ in 0..16 {
            if lua.jit_stats().queued_requests == 0 {
                break;
            }
            lua.service_jit().unwrap();
        }
        if lua.jit_stats().native_pair_returns > 0 {
            break;
        }
    }
    let before = lua.jit_stats();
    assert!(before.native_pair_returns > 0);
    let mut config = lua.jit_config();
    config.mode = JitMode::Off;
    lua.set_jit_config(config).unwrap();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().queued_requests, 0);
    let (value, _, after) = finish(lua, &executor, 64);
    assert_eq!(value.unwrap(), workload.expected);
    assert_eq!(after.native_pair_calls, before.native_pair_calls);
    assert_eq!(after.native_pair_returns, before.native_pair_returns);
    assert_eq!(after.native_pair_cache_hits, before.native_pair_cache_hits);
    assert_eq!(after.native_entries, before.native_entries);
    assert_eq!(after.compilation_requests, before.compilation_requests);
}

#[test]
fn production_auto_pairs_execute_the_frozen_corpus_with_exact_off_slice_traces() {
    let workload = workloads::WORKLOADS
        .iter()
        .find(|case| case.name == "closure_upvalue")
        .unwrap();
    for prepare in [false, true] {
        for fuel in [24, 64, 1000] {
            let reference = run(workload.source, JitMode::Off, prepare, fuel);
            let native = run(workload.source, JitMode::Auto, prepare, fuel);
            assert_eq!(native.0, workload.expected);
            assert_eq!(
                (native.0, &native.1),
                (reference.0, &reference.1),
                "prepare={prepare}, fuel={fuel}"
            );
            assert!(
                native.2.native_pair_returns > 0,
                "production tier was not selected"
            );
            assert!(native.2.native_pair_calls >= native.2.native_pair_returns);
            if fuel >= 64 {
                assert!(native.2.native_pair_cache_hits > 0);
            }
            assert_eq!(reference.2.native_pair_cache_hits, 0);
            assert_eq!(
                (
                    reference.2.native_pair_calls,
                    reference.2.native_pair_returns
                ),
                (0, 0)
            );
            assert_eq!(reference.2.native_entries, 0);
        }
    }
}

#[test]
fn scoped_pair_cache_preserves_alternating_callees_at_one_callsite() {
    let source = b"local n=0 local function add(v) n=n+v end local function sub(v) n=n-v end for i=1,600 do local f=i%2==0 and add or sub f(i) end return n";
    for prepare in [false, true] {
        for fuel in [24, 64, 1000] {
            let reference = run(source, JitMode::Off, prepare, fuel);
            let native = run(source, JitMode::Auto, prepare, fuel);
            assert_eq!(native.0, 300);
            assert_eq!((native.0, &native.1), (reference.0, &reference.1));
            if fuel >= 64 {
                assert!(
                    native.2.native_pair_returns > 0,
                    "prepare={prepare}, fuel={fuel}"
                );
            }
            assert_eq!(native.2.native_pair_cache_hits, 0);
            assert_eq!(native.2.compilation_failures, 0);
        }
    }
}

#[test]
fn discarded_callee_results_preserve_canonical_return_values_and_fuel() {
    let source =
        b"local n=7 local function f(v) n=n+v return v end for i=1,100 do f(2) end return n";
    for prepare in [false, true] {
        for fuel in [24, 64, 1000] {
            let reference = run(source, JitMode::Off, prepare, fuel);
            let native = run(source, JitMode::Auto, prepare, fuel);
            assert_eq!(native.0, 207);
            assert_eq!((native.0, native.1), (reference.0, reference.1));
            assert_eq!(native.2.native_pair_returns, 0);
        }
    }
}

#[test]
fn production_pair_fallbacks_preserve_noninteger_upvalues_and_arguments() {
    for source in [
        &b"local n=7.5 local function f(v) n=n+v end for i=1,100 do f(2) end return n==207.5 and 42 or -1"[..],
        &b"local n=7 local function f(v) n=n+v end for i=1,100 do f(2.5) end return n==257.0 and 42 or -1"[..],
        &b"local n=7 local function f() n=n+0.5 end for i=1,100 do f() end return n==57.0 and 42 or -1"[..],
    ] {
        for prepare in [false, true] {
            let reference = run(source, JitMode::Off, prepare, 64);
            let native = run(source, JitMode::Auto, prepare, 64);
            assert_eq!((native.0, &native.1), (reference.0, &reference.1));
            assert_eq!(native.0, 42);
            assert_eq!(native.2.native_pair_returns, 0);
            assert_eq!(native.2.compilation_failures, 0);
        }
    }
}
