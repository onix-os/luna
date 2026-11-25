use super::tests::counters;
use super::*;
use crate::jit::{JitConfig, JitMode, JitStats};
use crate::{Callback, CallbackReturn, Closure, Executor, Lua};

#[path = "../../../../examples/jit_support/workloads.rs"]
#[allow(dead_code)]
mod workloads;

struct Sample {
    elapsed_ns: u128,
    counters: [u64; 25],
    stats: JitStats,
    code_count: usize,
    child_count: usize,
}

pub(super) struct Prepared {
    pub lua: Lua,
    pub closure: crate::StashedClosure,
    pub expected: i64,
    pub code_count: usize,
    pub child_count: usize,
}

pub(super) fn prepare(name: &str, mode: &str) -> Prepared {
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
    let closure =
        lua.enter(|ctx| ctx.stash(Closure::load(ctx, Some(name), workload.source).unwrap()));
    for _ in 0..2 {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        assert_eq!(
            with_enabled(mode == "readonly", || lua.execute::<i64>(&executor)).unwrap(),
            workload.expected
        );
    }
    while with_enabled(mode == "readonly", || lua.prepare_jit()).unwrap() != 0 {}
    let (code_count, child_count) = lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        (
            manager.code.len(),
            manager
                .code
                .values()
                .filter(|c| c.code.readonly_lookup.is_some())
                .count(),
        )
    });
    assert_eq!(
        child_count,
        usize::from(mode == "readonly" && name == "rust_callbacks")
    );
    assert_eq!(code_count == 0, mode == "off");
    Prepared {
        lua,
        closure,
        expected: workload.expected,
        code_count,
        child_count,
    }
}

fn measure(name: &str, mode: &str, iterations: usize) -> Sample {
    assert!((1..=1000).contains(&iterations));
    let Prepared {
        mut lua,
        closure,
        expected,
        code_count,
        child_count,
    } = prepare(name, mode);
    let before = lua.jit_stats();
    let mut elapsed_ns = 0;
    for _ in 0..iterations {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        let start = std::time::Instant::now();
        let actual = std::hint::black_box(lua.execute::<i64>(&executor).unwrap());
        elapsed_ns += start.elapsed().as_nanos();
        assert_eq!(actual, expected);
    }
    let after = lua.jit_stats();
    assert!(elapsed_ns > 0);
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert_eq!(after.installed_regions, before.installed_regions);
    assert_eq!(after.compilation_failures, 0);
    assert_eq!(after.code_bytes, before.code_bytes);
    assert_eq!(after.code_requested_bytes, before.code_requested_bytes);
    if mode == "off" {
        assert_eq!(after.native_instructions, 0)
    } else {
        assert!(after.native_instructions > before.native_instructions)
    }
    let a = counters(after);
    let b = counters(before);
    let counters = std::array::from_fn(|i| a[i] - b[i]);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
    Sample {
        elapsed_ns,
        counters,
        stats: after,
        code_count,
        child_count,
    }
}

#[test]
fn frozen_workload_windows_prove_selection_counter_effects_and_no_compilation() {
    for workload in workloads::WORKLOADS {
        let ordinary = measure(workload.name, "ordinary", 1);
        let readonly = measure(workload.name, "readonly", 1);
        let off = measure(workload.name, "off", 1);
        assert_eq!(ordinary.counters[0], off.counters[0]);
        for i in (0..25).filter(|i| ![16, 17].contains(i)) {
            assert_eq!(
                ordinary.counters[i], readonly.counters[i],
                "{} counter={i}",
                workload.name
            )
        }
        let removed = if workload.name == "rust_callbacks" {
            5000
        } else {
            0
        };
        assert_eq!(ordinary.counters[16] - readonly.counters[16], removed);
        assert_eq!(ordinary.counters[17] - readonly.counters[17], removed);
    }
}

#[test]
#[ignore]
fn time_frozen_runtime() {
    let name = std::env::var("LUNA_READONLY_CASE").unwrap();
    let mode = std::env::var("LUNA_READONLY_MODE").unwrap();
    assert!(["off", "ordinary", "readonly"].contains(&mode.as_str()));
    let iterations = std::env::var("LUNA_READONLY_ITERATIONS")
        .unwrap()
        .parse()
        .unwrap();
    let sample = measure(&name, &mode, iterations);
    println!("\nREADONLY_TIMING_V1 case={name} mode={mode} iterations={iterations} elapsed_ns={} counters={:?} code_count={} child_count={} code_bytes={} code_requested_bytes={} metadata_bytes={} metadata_peak_bytes={} snapshot_peak_bytes={} compilation_delta=0 validated=true",sample.elapsed_ns,sample.counters,sample.code_count,sample.child_count,sample.stats.code_bytes,sample.stats.code_requested_bytes,sample.stats.metadata_bytes,sample.stats.metadata_peak_bytes,sample.stats.snapshot_peak_bytes);
}
