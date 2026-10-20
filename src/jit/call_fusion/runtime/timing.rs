use super::*;
use crate::{Callback, CallbackReturn, Executor, JitConfig, JitMode, JitStats, Lua};

#[path = "../../../../examples/jit_support/workloads.rs"]
#[allow(dead_code)]
mod workloads;

struct Sample {
    elapsed_ns: u128,
    counters: [u64; 25],
    fused: u64,
    code_count: usize,
    images: usize,
    stats: JitStats,
}

fn counters(s: JitStats) -> [u64; 25] {
    [
        s.total_dispatches,
        s.interpreted_slices,
        s.interpreted_instructions,
        s.native_entries,
        s.native_pair_calls,
        s.native_pair_returns,
        s.native_pair_cache_hits,
        s.native_region_entries,
        s.native_region_fragments,
        s.code_lookups,
        s.code_leases,
        s.native_instructions,
        s.guard_exits,
        s.native_interpreter_exits,
        s.native_budget_exits,
        s.native_panic_exits,
        s.helper_calls,
        s.helper_instructions,
        s.helper_declines,
        s.native_table_reads,
        s.native_table_writes,
        s.native_upvalue_reads,
        s.native_upvalue_writes,
        s.native_allocations,
        s.hook_exits,
    ]
}

fn selected<R>(prepared: Option<&Lease>, run: impl FnOnce() -> R) -> R {
    if let Some(prepared) = prepared {
        with_prepared(prepared.clone(), run)
    } else {
        run()
    }
}

fn measure(name: &str, mode: &str, iterations: usize) -> Sample {
    assert!(matches!(mode, "off" | "ordinary" | "fusion"));
    assert!((1..=1000).contains(&iterations));
    let workload = workloads::WORKLOADS
        .iter()
        .find(|w| w.name == name)
        .unwrap();
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: if mode == "off" {
            JitMode::Off
        } else {
            JitMode::Auto
        },
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().array_kernels = true;
        ctx.set_global(
            "host_increment",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let value: i64 = stack.consume(ctx)?;
                stack.replace(ctx, value.wrapping_add(1));
                Ok(CallbackReturn::Return)
            }),
        );
    });
    let (closure, request) = lua.enter(|ctx| {
        let caller = Closure::load(ctx, Some(name), workload.source).unwrap();
        (
            ctx.stash(caller),
            if mode == "fusion" {
                Request::new(ctx, caller).unwrap()
            } else {
                None
            },
        )
    });
    let prepared = request.map(|r| r.compile().unwrap());
    let images = usize::from(prepared.is_some());
    assert_eq!(
        images,
        usize::from(mode == "fusion" && name == "closure_upvalue")
    );
    for _ in 0..2 {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        assert_eq!(
            selected(prepared.as_ref(), || lua.execute::<i64>(&executor)).unwrap(),
            workload.expected
        );
    }
    while lua.prepare_jit().unwrap() != 0 {}
    let code_count = lua.enter(|ctx| ctx.jit().0.borrow().code.len());
    assert_eq!(code_count == 0, mode == "off");
    let before = lua.jit_stats();
    let fused_before = prepared.as_ref().map_or(0, |p| p.executions());
    assert_eq!(before.snapshot_bytes, 0);
    let mut elapsed_ns = 0;
    selected(prepared.as_ref(), || {
        for _ in 0..iterations {
            let executor =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
            let start = std::time::Instant::now();
            let actual = std::hint::black_box(lua.execute::<i64>(&executor).unwrap());
            elapsed_ns += start.elapsed().as_nanos();
            assert_eq!(actual, workload.expected);
        }
    });
    let after = lua.jit_stats();
    let fused = prepared.as_ref().map_or(0, |p| p.executions()) - fused_before;
    assert!(elapsed_ns > 0);
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert_eq!(after.installed_regions, before.installed_regions);
    assert_eq!(after.compilation_failures, 0);
    assert_eq!(after.code_bytes, before.code_bytes);
    assert_eq!(after.code_requested_bytes, before.code_requested_bytes);
    assert_eq!(after.metadata_bytes, before.metadata_bytes);
    assert_eq!(after.snapshot_bytes, 0);
    assert_eq!(after.native_panic_exits, 0);
    assert_eq!(fused != 0, images != 0);
    if mode == "off" {
        assert_eq!(after.native_instructions, 0);
    } else {
        assert!(after.native_instructions > before.native_instructions);
    }
    let a = counters(after);
    let b = counters(before);
    let counters = std::array::from_fn(|i| a[i] - b[i]);
    lua.clear_jit_cache();
    drop(prepared);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    Sample {
        elapsed_ns,
        counters,
        fused,
        code_count,
        images,
        stats: after,
    }
}

#[test]
fn frozen_windows_prove_actual_fusion_effects_and_ineligible_counter_identity() {
    for name in ["closure_upvalue", "integer_loop", "rust_callbacks"] {
        let ordinary = measure(name, "ordinary", 1);
        let candidate = measure(name, "fusion", 1);
        let off = measure(name, "off", 1);
        assert_eq!(ordinary.counters[0], off.counters[0]);
        if name != "closure_upvalue" {
            assert_eq!(candidate.fused, 0);
            assert_eq!(ordinary.counters, candidate.counters, "{name}");
        } else {
            assert!(candidate.fused > 0);
            assert_eq!(
                ordinary.counters[0] - candidate.counters[0],
                2 * candidate.fused
            );
            assert_eq!(
                ordinary.counters[1] - candidate.counters[1],
                2 * candidate.fused
            );
            assert_eq!(
                ordinary.counters[9] - candidate.counters[9],
                candidate.fused
            );
            let saved_leases = ordinary.counters[10]
                .checked_sub(candidate.counters[10])
                .unwrap();
            assert!(saved_leases <= candidate.fused);
            assert_eq!(
                ordinary.counters[2] + ordinary.counters[11],
                candidate.counters[2] + candidate.counters[11]
            );
            for sample in [&ordinary, &candidate] {
                assert_eq!(sample.counters[4], sample.counters[5]);
                assert!(sample.counters[4] <= 10000);
                assert!(sample.counters[6] <= sample.counters[4]);
                assert_eq!(sample.counters[16], sample.counters[17]);
            }
            assert!(candidate.counters[16] <= ordinary.counters[16]);
            for i in [7, 8, 12, 15, 18, 19, 20, 21, 22, 23, 24] {
                assert_eq!(
                    ordinary.counters[i], candidate.counters[i],
                    "counter={i} ordinary={:?} candidate={:?}",
                    ordinary.counters, candidate.counters
                );
            }
        }
    }
}

#[test]
#[ignore]
fn time_frozen_runtime() {
    let name = std::env::var("LUNA_FUSION_CASE").unwrap();
    let mode = std::env::var("LUNA_FUSION_MODE").unwrap();
    let iterations: usize = std::env::var("LUNA_FUSION_ITERATIONS")
        .unwrap()
        .parse()
        .unwrap();
    let s = measure(&name, &mode, iterations);
    println!("\nFUSION_TIMING_V1 {{\"case\":\"{name}\",\"mode\":\"{mode}\",\"iterations\":{iterations},\"elapsed_ns\":{},\"counters\":{:?},\"fused_regions\":{},\"code_count\":{},\"fusion_images\":{},\"code_bytes\":{},\"code_requested_bytes\":{},\"metadata_bytes\":{},\"metadata_peak_bytes\":{},\"snapshot_peak_bytes\":{},\"snapshot_bytes\":0,\"compilation_delta\":0,\"validated\":true}}", s.elapsed_ns,s.counters,s.fused,s.code_count,s.images,s.stats.code_bytes,s.stats.code_requested_bytes,s.stats.metadata_bytes,s.stats.metadata_peak_bytes,s.stats.snapshot_peak_bytes);
}
