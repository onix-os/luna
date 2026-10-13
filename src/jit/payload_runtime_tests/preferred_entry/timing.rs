use super::*;

struct Sample {
    elapsed_ns: u128,
    counters: [u64; 25],
    stats: JitStats,
}

fn measure(preferred: bool) -> Sample {
    let workload = workloads::WORKLOADS
        .iter()
        .find(|w| w.name == "rust_callbacks")
        .unwrap();
    let mut lua = configured(JitMode::Auto, preferred, false);
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
    assert_eq!(after.installed_regions, 1);
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert!(after.native_instructions > before.native_instructions);
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        assert_eq!(manager.code.len(), 1);
        assert!(manager
            .code
            .values()
            .all(|cached| cached.code.entry_kind == backend::EntryKind::Ordinary));
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
fn preferred_timing_preserves_frozen_callback_result_and_counters() {
    let ordinary = measure(false);
    let preferred = measure(true);
    assert_eq!(ordinary.counters, preferred.counters);
    assert!(ordinary.stats.code_bytes > 0 && preferred.stats.code_bytes > 0);
}

#[test]
#[ignore]
fn time_preferred_runtime() {
    assert_eq!(
        std::env::var("LUNA_PREFERRED_CASE").unwrap(),
        "rust_callbacks"
    );
    let preferred = match std::env::var("LUNA_PREFERRED_ENTRY").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("invalid preferred-entry selection"),
    };
    let sample = measure(preferred);
    println!(
        "\nTIMING_V1 case=rust_callbacks preferred={preferred} elapsed_ns={} counters={:?} code_bytes={} metadata_bytes={} metadata_peak_bytes={} snapshot_peak_bytes={}",
        sample.elapsed_ns, sample.counters, sample.stats.code_bytes,
        sample.stats.metadata_bytes, sample.stats.metadata_peak_bytes, sample.stats.snapshot_peak_bytes,
    );
}
