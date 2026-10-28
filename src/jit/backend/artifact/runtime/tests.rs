use super::*;
use crate::{Callback, CallbackReturn, Closure, Executor, Fuel, JitConfig, JitMode, JitStats, Lua};

#[test]
fn worker_artifact_matches_in_process_frames_counters_and_live_resources() {
    let run = |worker: bool, scenario: u8, budget: i32| {
        with_enabled(worker, || {
            crate::jit::readonly_lookup::binding::integer_resume::with_enabled(true, || {
                let mut lua = Lua::empty();
                lua.set_jit_config(JitConfig { mode: JitMode::Auto, hot_threshold: 1, ..Default::default() }).unwrap();
                let executor = lua.enter(|ctx| {
                    ctx.jit().test_array_kernels(true);
                    ctx.set_global("host_increment", Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                        let value: i64 = stack.consume(ctx)?;
                        if scenario == 3 && value == 7 {
                            ctx.set_global("host_increment", Callback::from_fn(&ctx, |ctx, _, mut stack| {
                                let value: i64 = stack.consume(ctx)?;
                                stack.replace(ctx, value + 2);
                                Ok(CallbackReturn::Return)
                            }));
                        }
                        if scenario == 4 && value == 7 { stack.replace(ctx, value as f64 + 0.5); }
                        else { stack.replace(ctx, value + 1); }
                        Ok(CallbackReturn::Return)
                    }));
                    let text: &[u8] = match scenario {
                        0 => b"local sum=0 for i=1,31 do sum=sum+i end return sum",
                        1 => b"local t={} for i=1,31 do t[i]=i end local sum=0 for i=1,31 do sum=sum+t[i] end return sum",
                        _ => b"local sum=0 for i=1,31 do sum=sum+host_increment(i) end return sum",
                    };
                    let closure = Closure::load(ctx, None, text).unwrap();
                    ctx.stash(Executor::start(ctx, closure.into(), ()))
                });
                let mut trace = Vec::new();
                let mut finished = false;
                for step in 0..4096 {
                    crate::jit::readonly_lookup::runtime::with_enabled(true, || lua.prepare_jit()).unwrap();
                    let report = lua.enter(|ctx| {
                        let executor = ctx.fetch(&executor);
                        let mut fuel = Fuel::with(if step == 0 { 0 } else { budget });
                        let done = executor.step(ctx, &mut fuel).unwrap();
                        (done, executor.mode(), fuel.remaining(), executor.test_frame_snapshot())
                    });
                    finished = report.0;
                    trace.push(report);
                    lua.gc_collect();
                    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
                    if finished { break; }
                }
                assert!(finished);
                let result = lua.execute::<f64>(&executor).unwrap().to_bits();
                let mut stats: JitStats = lua.jit_stats();
                assert!(stats.native_instructions > 0 && stats.installed_regions > 0);
                stats.snapshot_peak_bytes = 0;
                stats.metadata_peak_bytes = 0;
                stats.accounted_jit_peak_bytes = 0;
                let diagnostics = lua.enter(|ctx| {
                    let manager = ctx.jit().0.borrow();
                    (manager.activation_counts, manager.scalar_native_counts, manager.array_native_counts, manager.scalar_activation_entries)
                });
                lua.clear_jit_cache();
                assert_eq!(lua.jit_stats().code_bytes, 0);
                assert_eq!(lua.jit_stats().code_requested_bytes, 0);
                (trace, result, stats, diagnostics)
            }).0
        })
    };
    for scenario in 0..5 {
        for budget in [1, 2, 64, 65, 1024] {
            let control = run(false, scenario, budget);
            let candidate = run(true, scenario, budget);
            assert_eq!(control.1, 0);
            assert!(candidate.1 > 0);
            assert_eq!(
                candidate.0, control.0,
                "scenario={scenario} budget={budget}"
            );
        }
    }
}
