#![cfg(all(
    feature = "jit",
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{Callback, CallbackReturn, Closure, Executor, Fuel, JitConfig, JitMode, JitStats, Lua};

fn run(source: &[u8], mode: JitMode, available: i32) -> (i64, Vec<(i32, u64)>, JitStats) {
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    lua.set_jit_config(JitConfig {
        mode,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |_, _, mut stack| {
            stack.clear();
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("cb", callback);
        let closure = Closure::load(ctx, Some("region-integration"), source).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    lua.prepare_jit().unwrap();
    let mut trace = Vec::new();
    let mut done = false;
    for step in 0..20000 {
        let installed = lua.jit_stats().installed_regions;
        let remaining;
        (done, remaining) = lua.enter(|ctx| {
            let mut fuel = Fuel::with(available);
            (
                ctx.fetch(&executor).step(ctx, &mut fuel).unwrap(),
                fuel.remaining(),
            )
        });
        assert_eq!(lua.jit_stats().installed_regions, installed);
        trace.push((remaining, lua.jit_stats().total_dispatches));
        if done {
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
    assert!(done);
    let result = lua
        .try_enter(|ctx| ctx.fetch(&executor).take_result::<i64>(ctx).unwrap())
        .unwrap();
    let stats = lua.jit_stats();
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().registered_prototypes, 0);
    assert_eq!(lua.jit_stats().metadata_bytes, 0);
    (result, trace, stats)
}

#[test]
fn production_regions_preserve_interrupted_traces_and_native_call_coverage() {
    for (source, expected, repeats) in [
        (&b"local n=0 local function f(v) n=n+v end for i=1,1000 do f(i) end return n"[..], 500500, true),
        (&b"local n=0 local function f(v) n=n+v end for i=1,1000 do local q=i%3 f(i) end return n"[..], 500500, true),
        (&b"local n=0 local function f(v) n=n+v end local a='a' local b='b' for i=1,1000 do local q=a<b f(i) end return n"[..], 500500, true),
        (&b"local n=0 local function f(v) n=n+v end for i=1,1000 do f(i) f(i) end return n"[..], 1001000, true),
        (&b"local n=0 local function f(v) n=n+v end for i=1,1000 do f(i) if i%73==0 then cb() end end return n"[..], 500500, false),
    ] {
        for fuel in [1, 20, 127] {
            let off = run(source, JitMode::Off, fuel);
            let auto = run(source, JitMode::Auto, fuel);
            assert_eq!(off.0, expected);
            assert_eq!((auto.0, &auto.1), (off.0, &off.1), "fuel={fuel}");
            assert_eq!(off.2.native_region_entries, 0);
            assert_eq!(off.2.native_region_fragments, 0);
            assert!(auto.2.native_instructions > 0);
            if fuel == 127 {
                assert!(auto.2.native_region_entries > 0);
                if repeats {
                    assert!(auto.2.native_region_fragments > auto.2.native_region_entries);
                } else {
                    assert_eq!(auto.2.native_region_fragments, auto.2.native_region_entries);
                }
                assert!(auto.2.native_pair_returns > 500);
            }
        }
    }
}

#[test]
fn production_region_leases_release_before_nested_callback_and_unwind() {
    use std::{
        cell::Cell,
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
    };
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let panic = Rc::new(Cell::new(false));
    let calls = Rc::new(Cell::new(0));
    let caller = lua.enter(|ctx| {
        let panic = panic.clone();
        let calls = calls.clone();
        let callback = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            assert!(!panic.get(), "region callback panic");
            let function: luna::Function = stack.consume(ctx)?;
            let nested = Executor::start(ctx, function, (1,));
            while !nested.step(ctx, &mut Fuel::with(64)).unwrap() {}
            nested.take_result::<()>(ctx).unwrap()?;
            calls.set(calls.get() + 1);
            stack.clear();
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("cb", callback);
        let closure = Closure::load(
            ctx,
            None,
            b"local n=0 local function f(v) n=n+v end for i=1,2000 do f(i) end cb(f) return n",
        )
        .unwrap();
        ctx.stash(closure)
    });
    lua.prepare_jit().unwrap();
    let start = |lua: &mut Lua| {
        lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&caller).into(), ())))
    };
    for _ in 0..2 {
        let executor = start(&mut lua);
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 2001001);
    }
    assert_eq!(calls.get(), 2);
    assert!(lua.jit_stats().native_region_entries > 0);
    panic.set(true);
    let executor = start(&mut lua);
    assert!(catch_unwind(AssertUnwindSafe(|| lua.execute::<i64>(&executor))).is_err());
    lua.clear_jit_cache();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    panic.set(false);
    let executor = start(&mut lua);
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 2001001);
    assert_eq!(calls.get(), 3);
}
