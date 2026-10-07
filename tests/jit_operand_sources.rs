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
        let closure = Closure::load(ctx, Some("operand-sources"), source).unwrap();
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
    panic!("operand-source workload failed to finish");
}

#[test]
fn mixed_table_operand_sources_match_every_fuel_and_collection_boundary() {
    let cases = [
        ("local t={} local n=0 for i=1,80 do local k=i local v=i+1 t[k]=v n=n+t[k] t[k]=42 n=n+t[k] t.field=v n=n+t.field t.field=43 n=n+t.field end return n", 13440, false),
        ("local t={} local function update(i) local k=i local v=i+1 local n=0 t[k]=v n=n+t[k] t[k]=42 n=n+t[k] t.field=v n=n+t.field t.field=43 return n+t.field end local n=0 for i=1,80 do n=n+update(i) end return n", 13440, true),
        ("local t={} local n=0 for i=1,80 do local v={value=i} t[i]=v t.field=v n=n+t[i].value+t.field.value t[i]='x' t.field='y' assert(t[i]=='x' and t.field=='y') end return n", 6480, false),
    ];
    for (source, expected, closed) in cases {
        for budget in [i32::MIN, -1, 0, 1, 7, 64, 10000] {
            let reference = run(source.as_bytes(), false, budget);
            let native = run(source.as_bytes(), true, budget);
            assert_eq!(native.0, reference.0, "budget {budget}: {source}");
            assert_eq!((reference.1, native.1), (expected, expected));
            assert_eq!(reference.2.native_instructions, 0);
            assert!(native.2.native_table_reads > 0);
            assert!(native.2.native_table_writes > 0);
            if closed {
                assert!(native.2.native_upvalue_reads > 0);
            }
        }
    }
}
