use std::{error::Error, fs::OpenOptions, hint::black_box};

use luna::{Callback, CallbackReturn, Closure, Executor, JitConfig, JitMode, JitStats, Lua};

#[path = "jit_support/profile_control.rs"]
mod profile_control;
#[path = "jit_support/workloads.rs"]
#[allow(dead_code)]
mod workloads;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn counts(stats: JitStats) -> [u64; 25] {
    [
        stats.total_dispatches,
        stats.interpreted_slices,
        stats.interpreted_instructions,
        stats.native_entries,
        stats.native_pair_calls,
        stats.native_pair_returns,
        stats.native_pair_cache_hits,
        stats.native_region_entries,
        stats.native_region_fragments,
        stats.code_lookups,
        stats.code_leases,
        stats.native_instructions,
        stats.guard_exits,
        stats.native_interpreter_exits,
        stats.native_budget_exits,
        stats.native_panic_exits,
        stats.helper_calls,
        stats.helper_instructions,
        stats.helper_declines,
        stats.native_table_reads,
        stats.native_table_writes,
        stats.native_upvalue_reads,
        stats.native_upvalue_writes,
        stats.native_allocations,
        stats.hook_exits,
    ]
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 || !matches!(args[0].as_str(), "off" | "auto") {
        return Err("usage: jit_cycles off|auto CASE ITERATIONS CONTROL_FIFO ACK_FIFO".into());
    }
    let workload = workloads::WORKLOADS
        .iter()
        .find(|case| case.name == args[1])
        .ok_or("unknown workload")?;
    let iterations = args[2].parse::<usize>()?;
    if !(1..=10000).contains(&iterations) {
        return Err("iterations must be 1..=10000".into());
    }
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: if args[0] == "auto" {
            JitMode::Auto
        } else {
            JitMode::Off
        },
        ..Default::default()
    })?;
    lua.enter(|ctx| {
        let increment = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value: i64 = stack.consume(ctx)?;
            stack.replace(ctx, value.wrapping_add(1));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("host_increment", increment);
    });
    let closure = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some(workload.name), workload.source)?;
        Ok(ctx.stash(closure))
    })?;
    for _ in 0..2 {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        assert_eq!(lua.execute::<i64>(&executor)?, workload.expected);
    }
    let before = lua.jit_stats();
    let commands = OpenOptions::new().write(true).open(&args[3])?;
    let acknowledgements = OpenOptions::new().read(true).open(&args[4])?;
    let mut control = profile_control::Control::new(commands, acknowledgements);
    control.enabled(|| -> Result<()> {
        for _ in 0..iterations {
            let executor =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
            assert_eq!(black_box(lua.execute::<i64>(&executor)?), workload.expected);
        }
        Ok(())
    })?;
    let after = lua.jit_stats();
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert_eq!(after.compilation_failures, 0);
    assert_eq!(after.code_bytes, before.code_bytes);
    let start = counts(before);
    let end = counts(after);
    let delta: [u64; 25] = std::array::from_fn(|i| end[i] - start[i]);
    assert_eq!(delta[11] > 0, args[0] == "auto");
    println!(
        "cycles_protocol=1 mode={} case={} warmups=2 iterations={iterations} verified=1 compilation_delta=0 counters={delta:?}",
        args[0], workload.name,
    );
    println!("scope=warmed_execution_lifecycle elapsed_acceptance=false");
    Ok(())
}
