use super::*;

#[path = "../../../examples/jit_support/workloads.rs"]
#[allow(dead_code)]
mod workloads;

fn configured(failure: backend::Failure) -> Lua {
    let mut lua = state(JitMode::Auto, false);
    lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().memory_failure = failure;
        let increment = crate::Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value: i64 = stack.consume(ctx)?;
            stack.replace(ctx, value.wrapping_add(1));
            Ok(crate::CallbackReturn::Return)
        });
        ctx.set_global("host_increment", increment);
    });
    lua
}

#[inline(never)]
fn profile_execute(lua: &mut Lua, executor: &StashedExecutor) -> i64 {
    std::hint::black_box(lua.execute::<i64>(executor).unwrap())
}

fn run(
    workload: &workloads::Workload,
    failure: backend::Failure,
    profile: bool,
) -> ([u64; 25], JitStats) {
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
    let actual = if profile {
        profile_execute(&mut lua, &executor)
    } else {
        lua.execute::<i64>(&executor).unwrap()
    };
    assert_eq!(actual, workload.expected);
    let after = lua.jit_stats();
    assert_eq!(after.compilation_failures, 0);
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert!(after.native_instructions > before.native_instructions);
    let expected = if failure == backend::Failure::RequireStrided {
        backend::EntryKind::Strided
    } else {
        backend::EntryKind::Ordinary
    };
    lua.enter(|ctx| {
        assert!(ctx
            .jit()
            .0
            .borrow()
            .code
            .values()
            .all(|cached| cached.code.entry_kind == expected))
    });
    let delta = std::array::from_fn(|i| counters(after)[i] - counters(before)[i]);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    (delta, after)
}

#[test]
fn strided_runtime_frozen_workloads_preserve_results_and_coverage() {
    for workload in workloads::WORKLOADS {
        let (expected, _) = run(workload, backend::Failure::None, false);
        let (actual, _) = run(workload, backend::Failure::RequireStrided, false);
        assert_eq!(actual, expected, "{}", workload.name);
    }
}

#[test]
#[ignore]
fn profile_strided_runtime() {
    let name = std::env::var("LUNA_STRIDED_CASE").unwrap();
    let strided = match std::env::var("LUNA_STRIDED").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("invalid transport"),
    };
    let workload = workloads::WORKLOADS
        .iter()
        .find(|w| w.name == name)
        .unwrap();
    let failure = if strided {
        backend::Failure::RequireStrided
    } else {
        backend::Failure::None
    };
    let (delta, stats) = run(workload, failure, true);
    println!("case={name} strided={strided} validated=true counters={delta:?} code_bytes={} metadata_bytes={} metadata_peak_bytes={} snapshot_peak_bytes={}",
        stats.code_bytes, stats.metadata_bytes, stats.metadata_peak_bytes, stats.snapshot_peak_bytes);
}

#[test]
fn universal_memory_runtime_frozen_workloads_preserve_results_and_coverage() {
    for workload in workloads::WORKLOADS {
        let (expected, _) = run(workload, backend::Failure::None, false);
        let (actual, _) = run(workload, backend::Failure::RequireUniversalMemory, false);
        assert_eq!(actual, expected, "{}", workload.name);
    }
}

#[test]
#[ignore]
fn profile_universal_memory_runtime() {
    let name = std::env::var("LUNA_MEMORY_CASE").unwrap();
    let selected = match std::env::var("LUNA_UNIVERSAL_MEMORY").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("invalid memory selection"),
    };
    let workload = workloads::WORKLOADS
        .iter()
        .find(|w| w.name == name)
        .unwrap();
    let failure = if selected {
        backend::Failure::RequireUniversalMemory
    } else {
        backend::Failure::None
    };
    let (delta, stats) = run(workload, failure, true);
    println!("case={name} universal_memory={selected} validated=true counters={delta:?} code_bytes={} metadata_bytes={} metadata_peak_bytes={} snapshot_peak_bytes={}",
        stats.code_bytes, stats.metadata_bytes, stats.metadata_peak_bytes, stats.snapshot_peak_bytes);
}
