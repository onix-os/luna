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
        let closure = Closure::load(ctx, Some("set-list"), source).unwrap();
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
    panic!("list workload failed to finish");
}

#[test]
fn fixed_and_variable_lists_preserve_fuel_slices_results_and_gc() {
    let large = format!(
        "local sum=0 for i=1,10 do local t={{{}}} sum=sum+t[1]+t[130]+t[260] end return sum",
        vec!["1"; 260].join(",")
    );
    let cases = [
        ("local sum=0 for i=1,100 do local t={i,i+1} sum=sum+t[1] end return sum", 5050),
        ("local sum=0 local ref={} for i=1,100 do local t={i,nil,ref,-0.0,'x'} assert(t[3]==ref and t[5]=='x') sum=sum+t[1] end return sum", 5050),
        ("local function values(i) return i,i+1 end local sum=0 for i=1,100 do local t={values(i)} sum=sum+t[1]+t[2] end return sum", 10200),
        ("local function values(i) return i,i+1 end local sum=0 for i=1,100 do local t={i,values(i)} sum=sum+t[1]+t[2]+t[3] end return sum", 15250),
        (large.as_str(), 30),
        ("local sum=0 local function add(i) sum=sum+i end for i=1,200 do local t={i,i+1} add(t[1]) end return sum", 20100),
    ];
    for (source, expected) in cases {
        for fuel in [i32::MIN, -1, 0, 1, 7, 64, 10000] {
            let reference = run(source.as_bytes(), false, fuel);
            let native = run(source.as_bytes(), true, fuel);
            assert_eq!(native.0, reference.0, "fuel {fuel}: {source}");
            assert_eq!((reference.1, native.1), (expected, expected));
            assert_eq!(reference.2.native_instructions, 0);
            assert!(native.2.native_instructions > 0);
        }
    }
}

#[test]
fn fixed_lists_stay_native_across_the_first_full_slice() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local sum=0 for i=1,100 do local t={i,i+1} sum=sum+t[1] end return sum",
        )
        .unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    lua.enter(|ctx| {
        ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap();
    });
    let stats = lua.jit_stats();
    assert_eq!(
        (stats.code_lookups, stats.code_leases, stats.native_entries),
        (1, 1, 1)
    );
    assert_eq!(
        (stats.native_instructions, stats.interpreted_instructions),
        (64, 0)
    );
    assert!(stats.native_table_writes > 0);
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 5050);
    assert_eq!(lua.jit_stats().native_table_writes, 200);
}

#[test]
fn fixed_lists_execute_inside_native_call_regions() {
    let source = b"local sum=0 local function add(i) sum=sum+i end for i=1,2000 do local t={i,i+1} add(t[1]) end return sum";
    let reference = run(source, false, 1000);
    let native = run(source, true, 1000);
    assert_eq!(reference.0, native.0);
    assert_eq!((reference.1, native.1), (2001000, 2001000));
    assert!(native.2.native_region_entries > 0);
    assert_eq!(native.2.native_table_writes, 4000);
}
