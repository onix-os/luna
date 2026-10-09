use crate::{Closure, Executor, Fuel, JitConfig, JitMode, JitStats, Lua};

struct Run {
    result: i64,
    slices: Vec<(bool, crate::ExecutorMode, i32, u64)>,
    stats: JitStats,
    direct: (u64, u64),
}

fn run(source: &str, mode: JitMode, arrays: bool, budget: i32) -> Run {
    run_with_readonly(source, mode, arrays, budget, false)
}

fn run_with_readonly(
    source: &str,
    mode: JitMode,
    arrays: bool,
    budget: i32,
    readonly: bool,
) -> Run {
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode,
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        ctx.jit().test_array_kernels(arrays);
        let closure = Closure::load(ctx, None, source.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    if mode == JitMode::Auto {
        while lua.prepare_jit().unwrap() != 0 {}
    }
    let mut slices = Vec::new();
    for step in 0..50_000 {
        let (done, mode, remaining) = lua.enter(|ctx| {
            let executor = ctx.fetch(&executor);
            let mut fuel = Fuel::with(budget);
            (
                executor.step(ctx, &mut fuel).unwrap(),
                executor.mode(),
                fuel.remaining(),
            )
        });
        slices.push((done, mode, remaining, lua.jit_stats().total_dispatches));
        if readonly && step == 10 {
            lua.enter(|ctx| {
                let crate::Value::Table(table) = ctx.globals().get_value(ctx, "exported") else {
                    panic!("missing exported table");
                };
                table.set_readonly(&ctx, true);
            });
        }
        lua.gc_collect();
        if done {
            return Run {
                result: lua
                    .try_enter(|ctx| ctx.fetch(&executor).take_result::<i64>(ctx)?)
                    .unwrap(),
                slices,
                stats: lua.jit_stats(),
                direct: lua.enter(|ctx| ctx.jit().test_array_counts()),
            };
        }
    }
    panic!("array execution did not finish");
}

#[test]
fn host_readonly_mutation_between_gc_steps_stops_previously_native_writes() {
    let source = "local t={} exported=t local ok=pcall(function() local alias=t for i=1,5000 do alias[i]=i end end) return ok and 1 or 0";
    let interpreted = run_with_readonly(source, JitMode::Off, false, 1, true);
    let ordinary = run_with_readonly(source, JitMode::Auto, false, 1, true);
    let candidate = run_with_readonly(source, JitMode::Auto, true, 1, true);
    assert_eq!(candidate.result, 0);
    assert_eq!(candidate.result, interpreted.result);
    assert_eq!(candidate.slices, ordinary.slices);
    assert_eq!(candidate.slices, interpreted.slices);
    assert!(candidate.direct.1 > 0);
    assert_eq!(
        candidate.stats.native_table_writes,
        ordinary.stats.native_table_writes
    );
}

#[test]
fn auto_array_kernels_preserve_executor_slices_and_remove_table_helpers() {
    let source = "local t={} for i=1,5000 do t[i]=i end local sum=0 for i=1,5000 do sum=sum+t[i] end return sum";
    for budget in [0, 1, 17, 65536] {
        let interpreted = run(source, JitMode::Off, false, budget);
        let ordinary = run(source, JitMode::Auto, false, budget);
        let candidate = run(source, JitMode::Auto, true, budget);
        assert_eq!(candidate.result, 12_502_500);
        assert_eq!(candidate.result, interpreted.result);
        assert_eq!(candidate.slices, ordinary.slices);
        assert_eq!(candidate.slices, interpreted.slices);
        assert_eq!(
            candidate.stats.native_instructions,
            ordinary.stats.native_instructions
        );
        assert_eq!(
            candidate.stats.interpreted_instructions,
            ordinary.stats.interpreted_instructions
        );
        assert_eq!(
            candidate.stats.native_entries,
            ordinary.stats.native_entries
        );
        assert_eq!(
            candidate.stats.native_table_reads,
            ordinary.stats.native_table_reads
        );
        assert_eq!(
            candidate.stats.native_table_writes,
            ordinary.stats.native_table_writes
        );
        assert!(
            candidate.direct.0 > 4000 && candidate.direct.1 > 4000,
            "budget={budget} direct={:?}",
            candidate.direct
        );
        assert!(candidate.stats.helper_calls + 8000 < ordinary.stats.helper_calls);
        println!(
            "array_runtime fuel={budget} helper_calls={}->{} direct={:?}",
            ordinary.stats.helper_calls, candidate.stats.helper_calls, candidate.direct
        );
    }
}

#[test]
fn auto_array_fallback_preserves_aliases_callbacks_and_host_observable_changes() {
    for source in [
        "local t={} local alias=t for i=1,50 do t[i]=i end local s=0 for i=50,1,-3 do s=s+alias[i] end return s",
        "local t=setmetatable({}, {__index=function(_,i) return i*3 end}) local s=0 for i=1,50 do s=s+t[i] end return s",
        "local t={} for i=1,50 do t[i]=i end local s=0 for i=1,50 do s=s+t[i] end setmetatable(t,{__index=function() return 77 end}) for i=51,75 do s=s+t[i] end return s",
        "local t={} for i=1,50 do t[i]=i end t[25]='oops' local ok=pcall(function() local s=0 for i=1,50 do s=s+t[i] end return s end) return ok and 1 or 0",
        "local t=setmetatable({}, {__mode='v'}) for i=1,50 do t[i]=i end local s=0 for i=1,50 do s=s+t[i] end return s",
    ] {
        for budget in [1, 65536] {
            let interpreted = run(source, JitMode::Off, false, budget);
            let ordinary = run(source, JitMode::Auto, false, budget);
            let candidate = run(source, JitMode::Auto, true, budget);
            assert_eq!(candidate.result, interpreted.result, "{source}");
            assert_eq!(candidate.slices, ordinary.slices, "{source}");
            assert_eq!(candidate.slices, interpreted.slices, "{source}");
            assert_eq!(candidate.stats.native_instructions, ordinary.stats.native_instructions);
            assert_eq!(candidate.stats.interpreted_instructions, ordinary.stats.interpreted_instructions);
        }
    }
}
