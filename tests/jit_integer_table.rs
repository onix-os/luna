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
        let closure = Closure::load(ctx, Some("integer-table"), source).unwrap();
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
    panic!("integer-table workload failed to finish");
}

#[test]
fn integer_and_generic_reads_preserve_fuel_gc_and_metamethods() {
    let cases = [
        ("local t={} for i=1,80 do t[i]=i end local s=0 for i=1,80 do s=s+t[i] end return s", 3240),
        ("local t={10,20,30} t[0]=40 t[-1]=50 t[1000000]=60 local s=0 for i=1,80 do s=s+t[1.0]+t[0]+t[-1]+t[1000000] end return s", 12800),
        ("local calls=0 local t=setmetatable({10,nil,30},{__index=function(_,k) calls=calls+1 return k end}) local s=0 for i=1,80 do s=s+t[1]+t[2]+t[3] end return s+calls", 3440),
        ("local kept={value=7} local t=setmetatable({kept},{__mode='v'}) local s=0 for i=1,80 do assert(t[1]==kept) s=s+t[1].value end return s", 560),
        ("local t={11,22} local s=0 local function read(i) s=s+t[i] end for i=1,80 do read(1) end return s", 880),
    ];
    for (source, expected) in cases {
        for budget in [i32::MIN, -1, 0, 1, 7, 64, 10000] {
            let reference = run(source.as_bytes(), false, budget);
            let native = run(source.as_bytes(), true, budget);
            assert_eq!(native.0, reference.0, "budget {budget}: {source}");
            assert_eq!((reference.1, native.1), (expected, expected));
            assert_eq!(reference.2.native_table_reads, 0);
            assert!(native.2.native_table_reads > 0);
        }
    }
}

#[test]
fn string_reads_preserve_traces_mutation_weak_values_and_fallback() {
    let cases = [
        ("local t={field=7} local s=0 for i=1,80 do s=s+t.field end return s", 560),
        ("local key=string.char(102,105,101,108,100) local t={field=7,[1]=9,['1']=11} local s=0 for i=1,80 do s=s+t[key]+t['1']+t[1] end return s", 2160),
        ("local t={field=false} setmetatable(t,{__index=function() error('unexpected fallback') end}) local s=0 for i=1,80 do if t.field==false then s=s+1 end end return s", 80),
        ("local t={} local s=0 for i=1,80 do t.field=i s=s+t.field if i==40 then t.field=nil setmetatable(t,{__index=function(_,k) assert(k=='field') return 100 end}) s=s+t.field setmetatable(t,nil) end end return s", 3340),
        ("local kept={n=7} local t=setmetatable({field=kept},{__mode='kv'}) local s=0 for i=1,80 do assert(t.field==kept) s=s+t.field.n end return s", 560),
    ];
    for (source, expected) in cases {
        for budget in [i32::MIN, -1, 0, 1, 7, 64, 10000] {
            let reference = run(source.as_bytes(), false, budget);
            let native = run(source.as_bytes(), true, budget);
            assert_eq!(native.0, reference.0, "budget {budget}: {source}");
            assert_eq!((reference.1, native.1), (expected, expected));
            assert_eq!(reference.2.native_table_reads, 0);
            assert!(
                native.2.native_table_reads >= 80,
                "{source}: {:?}",
                native.2
            );
        }
    }
}
