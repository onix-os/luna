use super::*;

struct Sample {
    elapsed_ns: u128,
    counters: [u64; 25],
    stats: JitStats,
}

fn measure(workload: &workloads::Workload, strided: bool) -> Sample {
    let failure = if strided {
        backend::Failure::RequireStrided
    } else {
        backend::Failure::None
    };
    let mut lua = configured(failure);
    let closure = lua
        .enter(|ctx| ctx.stash(Closure::load(ctx, Some(workload.name), workload.source).unwrap()));
    for _ in 0..2 {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), workload.expected);
    }
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
    let before = lua.jit_stats();
    let start = std::time::Instant::now();
    let actual = profile_execute(&mut lua, &executor);
    let elapsed_ns = start.elapsed().as_nanos();
    let after = lua.jit_stats();
    assert!(elapsed_ns > 0);
    assert_eq!(actual, workload.expected);
    assert_eq!(after.compilation_failures, 0);
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert!(after.native_instructions > before.native_instructions);
    let expected = if strided {
        backend::EntryKind::Strided
    } else {
        backend::EntryKind::Ordinary
    };
    lua.enter(|ctx| {
        let jit = ctx.jit().0.borrow();
        assert!(!jit.code.is_empty());
        assert!(jit
            .code
            .values()
            .all(|cached| cached.code.entry_kind == expected));
    });
    let counters = std::array::from_fn(|i| counters(after)[i] - counters(before)[i]);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    Sample {
        elapsed_ns,
        counters,
        stats: after,
    }
}

#[test]
fn strided_timing_checks_all_frozen_results_and_counter_parity() {
    for workload in workloads::WORKLOADS {
        let ordinary = measure(workload, false);
        let strided = measure(workload, true);
        assert_eq!(ordinary.counters, strided.counters, "{}", workload.name);
        assert!(ordinary.stats.code_bytes > 0 && strided.stats.code_bytes > 0);
    }
}

#[test]
#[ignore]
fn time_strided_runtime() {
    let name = std::env::var("LUNA_STRIDED_CASE").unwrap();
    let strided = match std::env::var("LUNA_STRIDED").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("invalid transport"),
    };
    let workload = workloads::WORKLOADS
        .iter()
        .find(|workload| workload.name == name)
        .unwrap();
    let sample = measure(workload, strided);
    println!(
        "\nTIMING_V1 case={name} strided={strided} elapsed_ns={} counters={:?} code_bytes={} metadata_bytes={} metadata_peak_bytes={} snapshot_peak_bytes={}",
        sample.elapsed_ns,
        sample.counters,
        sample.stats.code_bytes,
        sample.stats.metadata_bytes,
        sample.stats.metadata_peak_bytes,
        sample.stats.snapshot_peak_bytes,
    );
}
