#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

#[path = "../examples/jit_support/suspension.rs"]
mod suspension;

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExecutorMode, ExternError, Fuel, Function,
    JitConfig, JitMode, Lua, StashedExecutor, Variadic,
};

fn mixed_call_state(
    caller_native: bool,
    callee_native: bool,
    tail: bool,
) -> Result<(Lua, StashedExecutor), ExternError> {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: if caller_native || callee_native {
            JitMode::Auto
        } else {
            JitMode::Off
        },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        ctx.set_global(
            "park",
            Callback::from_fn(&ctx, |_, _, _| {
                Ok(CallbackReturn::Yield {
                    to_thread: None,
                    then: None,
                })
            }),
        );
    });
    let caller = format!(
        r#"
        local t={{sum=0}}
        for i=1,100 do t.sum=t.sum+i end
        park(1,t.sum)
        {}
        "#,
        if tail {
            "return callee(t.sum)"
        } else {
            r#"
            local a,b,c,d=callee(t.sum)
            park(4,a,b,c,d)
            for i=1,100 do t.sum=t.sum+i end
            return 5,t.sum,a,b,c,d
            "#
        }
    );
    let callee = br#"
        local initial=...
        park(2,initial)
        local t={sum=initial}
        for i=1,100 do t.sum=t.sum+i end
        park(3,t.sum)
        return t.sum,nil,17,nil
    "#;
    for prepare in [true, false] {
        for (name, script, native) in [
            ("caller", caller.as_bytes(), caller_native),
            ("callee", callee.as_slice(), callee_native),
        ] {
            if native == prepare {
                lua.try_enter(|ctx| {
                    let closure = Closure::load(ctx, Some(name), script)?;
                    ctx.set_global(name, closure);
                    Ok(())
                })?;
            }
        }
        if prepare {
            assert_eq!(
                lua.prepare_jit().unwrap(),
                usize::from(caller_native) + usize::from(callee_native)
            );
        }
    }
    let executor = lua.try_enter(|ctx| {
        let caller: Function = ctx.get_global("caller")?;
        Ok(ctx.stash(Executor::start(ctx, caller, ())))
    })?;
    Ok((lua, executor))
}

fn call_step(lua: &mut Lua, executor: &StashedExecutor, fuel: i32) -> (bool, ExecutorMode, i32) {
    lua.enter(|ctx| {
        let executor = ctx.fetch(executor);
        let mut fuel = Fuel::with(fuel);
        (
            executor.step(ctx, &mut fuel).unwrap(),
            executor.mode(),
            fuel.remaining(),
        )
    })
}

fn call_result(lua: &mut Lua, executor: &StashedExecutor) -> Result<Vec<Option<i64>>, ExternError> {
    lua.try_enter(|ctx| {
        Ok(ctx
            .fetch(executor)
            .take_result::<Variadic<Vec<Option<i64>>>>(ctx)??
            .0)
    })
}

#[test]
fn selective_caller_callee_tiers_preserve_calls_tail_calls_and_suspension(
) -> Result<(), ExternError> {
    for caller_native in [false, true] {
        for callee_native in [false, true] {
            for tail in [false, true] {
                for fuel in [1, 64, 65536] {
                    let case = format!(
                        "caller={caller_native} callee={callee_native} tail={tail} fuel={fuel}"
                    );
                    let (mut reference, left) = mixed_call_state(false, false, tail)?;
                    let (mut candidate, right) =
                        mixed_call_state(caller_native, callee_native, tail)?;
                    let installed = candidate.jit_stats().installed_regions;
                    let mut boundary = 0;
                    let mut previous_native = 0;
                    let mut previous_writes = 0;
                    let mut stopped = false;
                    for _ in 0..1000 {
                        let expected = call_step(&mut reference, &left, fuel);
                        let actual = call_step(&mut candidate, &right, fuel);
                        assert_eq!(actual, expected, "{case}");
                        reference.gc_collect();
                        candidate.gc_collect();
                        let stats = candidate.jit_stats();
                        assert_eq!(stats.installed_regions, installed, "{case}");
                        assert_eq!(stats.queued_requests, 0, "{case}");
                        if !actual.0 {
                            continue;
                        }
                        boundary += 1;
                        let expected = call_result(&mut reference, &left)?;
                        let result = call_result(&mut candidate, &right)?;
                        assert_eq!(result, expected, "{case} boundary={boundary}");
                        let values = match boundary {
                            1 => vec![Some(1), Some(5050)],
                            2 => vec![Some(2), Some(5050)],
                            3 => vec![Some(3), Some(10100)],
                            4 if tail => vec![Some(10100), None, Some(17), None],
                            4 => vec![Some(4), Some(10100), None, Some(17), None],
                            5 => vec![Some(5), Some(10100), Some(10100), None, Some(17), None],
                            _ => panic!("unexpected boundary: {case}"),
                        };
                        assert_eq!(result, values, "{case}");
                        let tier = match boundary {
                            1 | 5 => Some(caller_native),
                            3 => Some(callee_native),
                            _ => None,
                        };
                        if let Some(native) = tier {
                            assert_eq!(
                                stats.native_instructions > previous_native,
                                native,
                                "{case}"
                            );
                            assert_eq!(
                                stats.native_table_writes > previous_writes,
                                native,
                                "{case}"
                            );
                        }
                        previous_native = stats.native_instructions;
                        previous_writes = stats.native_table_writes;
                        let final_boundary = if tail { 4 } else { 5 };
                        for (lua, executor) in [(&mut reference, &left), (&mut candidate, &right)] {
                            let mode = lua.enter(|ctx| ctx.fetch(executor).mode());
                            if boundary == final_boundary {
                                assert_eq!(mode, ExecutorMode::Stopped, "{case}");
                            } else {
                                assert_eq!(mode, ExecutorMode::Suspended, "{case}");
                                lua.enter(|ctx| ctx.fetch(executor).resume(ctx, ()))
                                    .unwrap();
                            }
                        }
                        if boundary == final_boundary {
                            stopped = true;
                            break;
                        }
                    }
                    assert!(stopped, "slice bound exceeded: {case}");
                    assert_eq!(reference.jit_stats().native_instructions, 0);
                }
            }
        }
    }
    Ok(())
}

fn check_observation(report: &suspension::Report, mode: suspension::Mode, fuel: i32) {
    assert!(report.steps >= 3);
    if fuel <= 64 {
        assert!(report.steps > 3);
    }
    assert!(report.counted_work_max <= fuel as u64 + 63);
    assert!(report.fuel_debit_max <= i64::from(fuel) + 67);
    assert!(report.elapsed_ns >= report.load_ns);
    assert!(report.poll_ns >= report.max_poll_ns);
    assert!(report.gc_peak > 0);
    assert!(report.code_peak >= report.stats.code_bytes);
    assert!(report.queue_peak <= 16);
    assert_eq!(report.native_table_write_segments.len(), 3);
    if mode != suspension::Mode::Off {
        let first = usize::from(mode == suspension::Mode::Auto && fuel == 65536);
        if first == 1 {
            assert_eq!(report.native_segments[0], 0);
            assert_eq!(report.native_table_write_segments[0], 0);
        }
        assert!(report.native_segments[first..]
            .iter()
            .all(|count| *count > 0));
        assert!(report.native_table_write_segments[first..]
            .iter()
            .all(|count| *count > 0));
    }
}

#[test]
fn public_coroutine_slices_resume_with_verified_tiered_heap_work() {
    for mode in [
        suspension::Mode::Off,
        suspension::Mode::Auto,
        suspension::Mode::Prepared,
    ] {
        for fuel in [1, 64, 65536] {
            let report = suspension::run("coroutine_resume", mode, fuel).unwrap();
            assert_eq!(report.boundary_values, [20100, 40200, 60300]);
            assert_eq!(report.suspensions, 2);
            assert_eq!(report.pending_polls, 0);
            assert_eq!(report.ready_polls, 0);
            assert_eq!(report.wakes, 0);
            assert_eq!(report.forced_gc_cycles, 4);
            check_observation(&report, mode, fuel);
        }
    }
}

#[cfg(feature = "async")]
#[test]
fn public_foreign_futures_park_outside_arena_and_resume_native_heap_work() {
    for mode in [
        suspension::Mode::Off,
        suspension::Mode::Auto,
        suspension::Mode::Prepared,
    ] {
        for fuel in [1, 64, 65536] {
            let report = suspension::run("foreign_await", mode, fuel).unwrap();
            assert_eq!(report.boundary_values, [60384]);
            assert_eq!(report.suspensions, 2);
            assert_eq!(report.pending_polls, 6);
            assert_eq!(report.ready_polls, 2);
            assert_eq!(report.wakes, 6);
            assert_eq!(report.forced_gc_cycles, 12);
            check_observation(&report, mode, fuel);
        }
    }
}
