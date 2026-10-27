use super::*;
use crate::{Executor, Fuel, JitConfig, JitMode};

fn routed<R>(enabled: bool, run: impl FnOnce() -> R) -> (R, u64) {
    with_enabled(enabled, || {
        crate::jit::readonly_lookup::runtime::with_enabled(true, run)
    })
}

fn state(scenario: u8) -> Lua {
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        ctx.set_global(
            "host_increment",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let value: i64 = stack.consume(ctx)?;
                if scenario == 1 && value == 7 {
                    ctx.set_global(
                        "host_increment",
                        Callback::from_fn(&ctx, |ctx, _, mut stack| {
                            let value: i64 = stack.consume(ctx)?;
                            stack.replace(ctx, value.wrapping_add(2));
                            Ok(CallbackReturn::Return)
                        }),
                    );
                }
                if scenario == 2 && value == 7 {
                    stack.replace(ctx, value as f64 + 0.5);
                } else {
                    stack.replace(ctx, value.wrapping_add(1));
                }
                if scenario == 3 && value == 7 {
                    return Ok(CallbackReturn::Yield {
                        to_thread: None,
                        then: None,
                    });
                }
                Ok(CallbackReturn::Return)
            }),
        );
    });
    lua
}

type Snapshot = (Vec<Vec<String>>, Vec<Vec<String>>);

fn execute_slices(
    enabled: bool,
    scenario: u8,
    fuel: i32,
) -> (Vec<(i32, String, Snapshot)>, f64, u64, u64) {
    let ((trace, result, native), entries) = routed(enabled, || {
        let mut lua = state(scenario);
        let executor = lua.enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                b"local sum=0 for i=1,31 do sum=sum+host_increment(i) end return sum",
            )
            .unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        lua.prepare_jit().unwrap();
        let mut trace = Vec::new();
        loop {
            let mut slice = Fuel::with(fuel);
            let (finished, mode, snapshot) = lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                let finished = executor.step(ctx, &mut slice).unwrap();
                (
                    finished,
                    format!("{:?}", executor.mode()),
                    executor.test_frame_snapshot(),
                )
            });
            trace.push((slice.remaining(), mode, snapshot));
            lua.gc_collect();
            if finished {
                break;
            }
            assert!(trace.len() < 10000);
        }
        let result = lua.execute::<f64>(&executor).unwrap();
        (trace, result, lua.jit_stats().native_instructions)
    });
    (trace, result, native, entries)
}

#[test]
fn canonical_resume_preserves_actual_callback_slices_gc_and_binding_changes() {
    for scenario in 0..3 {
        for fuel in [1, 2, 64, 1024] {
            let control = execute_slices(false, scenario, fuel);
            let candidate = execute_slices(true, scenario, fuel);
            assert_eq!(candidate.0, control.0, "scenario={scenario} fuel={fuel}");
            assert_eq!(candidate.1.to_bits(), control.1.to_bits());
            assert_eq!(candidate.1, [527.0, 551.0, 526.5][scenario as usize]);
            assert!(candidate.2 > 0 && control.2 > 0);
            assert!(
                candidate.3 > 0,
                "scenario={scenario} fuel={fuel} native={}",
                candidate.2
            );
            assert_eq!(control.3, 0);
        }
    }
}

#[test]
fn canonical_resume_preserves_callback_yield_and_gc_before_resumption() {
    for fuel in [1, 2, 64, 1024] {
        let run = |enabled| {
            routed(enabled, || {
                let mut lua = state(3);
                let executor = lua.enter(|ctx| {
                    let closure = Closure::load(
                        ctx,
                        None,
                        b"local sum=0 for i=1,31 do sum=sum+host_increment(i) end return sum",
                    )
                    .unwrap();
                    ctx.stash(Executor::start(ctx, closure.into(), ()))
                });
                lua.prepare_jit().unwrap();
                let mut trace = Vec::new();
                let mut yields = 0;
                let result = loop {
                    let mut slice = Fuel::with(fuel);
                    let (finished, mode, snapshot) = lua.enter(|ctx| {
                        let executor = ctx.fetch(&executor);
                        (
                            executor.step(ctx, &mut slice).unwrap(),
                            executor.mode(),
                            executor.test_frame_snapshot(),
                        )
                    });
                    trace.push((slice.remaining(), format!("{mode:?}"), snapshot));
                    lua.gc_collect();
                    if finished {
                        let (value, mode) = lua.enter(|ctx| {
                            let executor = ctx.fetch(&executor);
                            let value = executor.take_result::<i64>(ctx).unwrap().unwrap();
                            (value, executor.mode())
                        });
                        if mode == crate::ExecutorMode::Suspended {
                            assert_eq!(value, 8);
                            lua.gc_collect();
                            lua.enter(|ctx| {
                                let executor = ctx.fetch(&executor);
                                executor.resume(ctx, 8).unwrap();
                            });
                            yields += 1;
                        } else {
                            break value;
                        }
                    }
                    assert!(trace.len() < 10000);
                };
                assert_eq!(yields, 1);
                assert_eq!(result, 527);
                assert!(lua.jit_stats().native_instructions > 0);
                trace
            })
        };
        let (control, zero) = run(false);
        let (candidate, entries) = run(true);
        assert_eq!(control, candidate, "fuel={fuel}");
        assert_eq!(zero, 0);
        assert!(entries > 0);
    }
}

#[test]
#[ignore]
fn canonical_resume_callback_profile() {
    let candidate = match std::env::var("LUNA_CANONICAL_RESUME").unwrap().as_str() {
        "control" => false,
        "candidate" => true,
        _ => panic!("unknown canonical resume profile mode"),
    };
    let (stats, entries) = routed(candidate, || {
        let mut lua = state(0);
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        })
        .unwrap();
        let closure = lua.enter(|ctx| {
            let closure = Closure::load(
                ctx,
                Some("rust_callbacks"),
                b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum",
            )
            .unwrap();
            ctx.stash(closure)
        });
        for _ in 0..13 {
            let executor =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
            let result = lua.execute::<i64>(&executor).unwrap();
            assert_eq!(std::hint::black_box(result), 12507500);
        }
        lua.jit_stats()
    });
    assert!(stats.native_instructions > 0);
    assert_eq!(entries > 0, candidate);
    println!("canonical_profile candidate={candidate} canonical_entries={entries} result=12507500 executions=13");
    println!("case=rust_callbacks mode=auto samples=11 native_entries={} region_entries={} region_fragments={} code_lookups={} code_leases={} native_instructions={} interpreted_instructions={} interpreted_slices={} guard_exits={} helper_instructions={} helper_declines={} table_reads={} table_writes={} upvalue_reads={} upvalue_writes={} allocations={} compilation_requests={} compilation_failures={} cache_evictions={} cache_eviction_refusals={} code_bytes={} metadata_bytes={} metadata_peak_bytes={} metadata_refusals={} registration_refusals={} snapshot_bytes={} snapshot_peak_bytes={}",
        stats.native_entries, stats.native_region_entries, stats.native_region_fragments,
        stats.code_lookups, stats.code_leases, stats.native_instructions, stats.interpreted_instructions,
        stats.interpreted_slices, stats.guard_exits, stats.helper_instructions, stats.helper_declines,
        stats.native_table_reads, stats.native_table_writes, stats.native_upvalue_reads, stats.native_upvalue_writes,
        stats.native_allocations, stats.compilation_requests, stats.compilation_failures, stats.cache_evictions,
        stats.cache_eviction_refusals, stats.code_bytes, stats.metadata_bytes, stats.metadata_peak_bytes,
        stats.metadata_allocation_refusals, stats.registration_refusals, stats.snapshot_bytes, stats.snapshot_peak_bytes);
}
