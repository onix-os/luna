#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{Closure, Executor, Fuel, JitConfig, JitMode, JitStats, Lua};

fn run(source: &[u8], native: bool, budget: i32) -> (Vec<(bool, i32)>, i64, JitStats) {
    let mut lua = Lua::core();
    if native {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
    }
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("typed-table-loop"), source).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
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
        if step.0 {
            return (
                trace,
                lua.execute::<i64>(&executor).unwrap(),
                lua.jit_stats(),
            );
        }
    }
    panic!("typed-table-loop workload failed to finish");
}

#[test]
fn typed_store_loops_preserve_fuel_gc_and_metamethod_fallback() {
    let cases = [
        ("local t={} for i=1,80 do t[i]=i end return t[1]+t[80]", 81, true),
        ("local t={} local x=0.5 for i=1,80 do x=x+0.5 t[i]=x end return t[80]*2", 81, true),
        ("local t={} for i=80,1,-1 do t[3]=i t[i]=0.5 end return t[3]", 1, true),
        ("local t=setmetatable({},{__mode='v'}) for i=1,80 do t[i]=i end return t[80]", 80, true),
        ("local calls=0 local t=setmetatable({},{__newindex=function(t,k,v) calls=calls+1 rawset(t,k,v) end}) for i=1,80 do t[i]=i end return calls+t[80]", 160, false),
        ("local target={} local t=setmetatable({},{__newindex=target}) for i=1,80 do t[i]=i end return target[80]", 80, false),
    ];
    for (source, expected, writes) in cases {
        for budget in [i32::MIN, -1, 0, 1, 7, 64, 10000] {
            let reference = run(source.as_bytes(), false, budget);
            let native = run(source.as_bytes(), true, budget);
            assert_eq!(native.0, reference.0, "budget {budget}: {source}");
            assert_eq!((reference.1, native.1), (expected, expected));
            assert_eq!(reference.2.native_table_writes, 0);
            if writes {
                assert!(native.2.native_table_writes > 0);
            } else {
                assert!(native.2.helper_declines > 0);
            }
        }
    }
}
