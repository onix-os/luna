use std::{error::Error, hint::black_box, time::Instant};

use luna::{Callback, CallbackReturn, Closure, Executor, JitConfig, JitMode, Lua, StashedClosure};

#[path = "jit_support/workloads.rs"]
mod workloads;
use workloads::{Workload, COLD_SOURCE, PREDICATE_SOURCE, WORKLOADS};

fn state(mode: &str) -> Lua {
    let mut lua = Lua::core();
    let mut config = JitConfig::default();
    match mode {
        "off" => {}
        "auto" => config.mode = JitMode::Auto,
        "force" => {
            config.mode = JitMode::Auto;
            config.hot_threshold = 1;
        }
        _ => unreachable!(),
    }
    lua.set_jit_config(config).unwrap();
    lua.enter(|ctx| {
        let increment = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value: i64 = stack.consume(ctx)?;
            stack.replace(ctx, value.wrapping_add(1));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("host_increment", increment);
    });
    lua
}

fn report(name: &str, mode: &str, mut timings: Vec<u128>, lua: &Lua) -> u128 {
    timings.sort_unstable();
    let stats = lua.jit_stats();
    println!(
        "case={name} mode={mode} samples={} median_ns={} min_ns={} max_ns={} native_entries={} code_lookups={} code_leases={} native_instructions={} interpreted_instructions={} interpreted_slices={} guard_exits={} helper_instructions={} helper_declines={} table_reads={} table_writes={} upvalue_reads={} upvalue_writes={} allocations={} compilation_requests={} compilation_failures={} cache_evictions={} cache_eviction_refusals={} code_bytes={} metadata_bytes={} metadata_peak_bytes={} metadata_refusals={} registration_refusals={} snapshot_bytes={} snapshot_peak_bytes={}",
        timings.len(),
        timings[timings.len() / 2],
        timings[0],
        timings[timings.len() - 1],
        stats.native_entries,
        stats.code_lookups,
        stats.code_leases,
        stats.native_instructions,
        stats.interpreted_instructions,
        stats.interpreted_slices,
        stats.guard_exits,
        stats.helper_instructions,
        stats.helper_declines,
        stats.native_table_reads,
        stats.native_table_writes,
        stats.native_upvalue_reads,
        stats.native_upvalue_writes,
        stats.native_allocations,
        stats.compilation_requests,
        stats.compilation_failures,
        stats.cache_evictions,
        stats.cache_eviction_refusals,
        stats.code_bytes,
        stats.metadata_bytes,
        stats.metadata_peak_bytes,
        stats.metadata_allocation_refusals,
        stats.registration_refusals,
        stats.snapshot_bytes,
        stats.snapshot_peak_bytes,
    );
    timings[timings.len() / 2]
}

fn warm(workload: &Workload, mode: &str, samples: usize) -> Result<(), Box<dyn Error>> {
    let mut lua = state(mode);
    let closure = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some(workload.name), workload.source)?;
        Ok(ctx.stash(closure))
    })?;
    if mode == "force" {
        lua.prepare_jit()?;
    }
    let mut timings = Vec::with_capacity(samples);
    for sample in 0..samples + 2 {
        let executor = lua
            .try_enter(|ctx| Ok(ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ()))))?;
        let start = Instant::now();
        let result = lua.execute::<i64>(&executor)?;
        let elapsed = start.elapsed().as_nanos();
        assert_eq!(black_box(result), workload.expected, "{}", workload.name);
        if sample >= 2 {
            timings.push(elapsed);
        }
    }
    if mode != "off" && lua.jit_capabilities().supported_target {
        assert!(
            lua.jit_stats().native_instructions > 0,
            "{} did not execute native code",
            workload.name
        );
    }
    report(workload.name, mode, timings, &lua);
    Ok(())
}

fn oslo_predicate(mode: &str, samples: usize) -> Result<(), Box<dyn Error>> {
    let mut lua = state(mode);
    let closure = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("oslo-row-predicate"), PREDICATE_SOURCE)?;
        Ok(ctx.stash(closure))
    })?;
    if mode == "force" {
        lua.prepare_jit()?;
    }
    let mut timings = Vec::with_capacity(samples);
    for sample in 0..samples + 2 {
        let start = Instant::now();
        let mut selected = 0;
        for row in 0..10000 {
            let executor = lua.try_enter(|ctx| {
                ctx.set_global(
                    "free",
                    if row % 2 == 0 {
                        500000000i64
                    } else {
                        1500000000i64
                    },
                );
                Ok(ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())))
            })?;
            selected += i64::from(lua.execute::<bool>(&executor)?);
        }
        let elapsed = start.elapsed().as_nanos();
        assert_eq!(black_box(selected), 5000);
        if sample >= 2 {
            timings.push(elapsed);
        }
    }
    if mode != "off" && lua.jit_capabilities().supported_target {
        assert!(
            lua.jit_stats().native_instructions > 0,
            "oslo predicate did not execute native code"
        );
    }
    report("oslo_predicate", mode, timings, &lua);
    Ok(())
}

fn cold(mode: &str, samples: usize) -> Result<(), Box<dyn Error>> {
    let mut timings = Vec::with_capacity(samples);
    let mut last = state(mode);
    for _ in 0..samples {
        let start = Instant::now();
        let mut lua = state(mode);
        let executor = lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, Some("config"), COLD_SOURCE)?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        assert_eq!(black_box(lua.execute::<i64>(&executor)?), 42);
        timings.push(start.elapsed().as_nanos());
        last = lua;
    }
    report("cold_config", mode, timings, &last);
    Ok(())
}

#[derive(Clone, Copy)]
enum PairedCase<'a> {
    Warm(&'a Workload),
    Predicate,
    Cold,
}

impl PairedCase<'_> {
    fn name(self) -> &'static str {
        match self {
            Self::Warm(workload) => workload.name,
            Self::Predicate => "oslo_predicate",
            Self::Cold => "cold_config",
        }
    }

    fn source(self) -> &'static [u8] {
        match self {
            Self::Warm(workload) => workload.source,
            Self::Predicate => PREDICATE_SOURCE,
            Self::Cold => COLD_SOURCE,
        }
    }

    fn minimum_speedup(self) -> Option<f64> {
        match self {
            Self::Warm(workload) => match workload.name {
                "integer_loop" | "float_loop" => Some(2.0),
                "array_table" | "closure_upvalue" => Some(1.25),
                "polymorphic_metamethod" | "rust_callbacks" | "allocation_gc" => Some(1.0 / 1.2),
                _ => unreachable!(),
            },
            Self::Predicate => None,
            Self::Cold => Some(1.0 / 1.15),
        }
    }

    fn measure(
        self,
        lua: &mut Lua,
        closure: Option<&StashedClosure>,
        mode: &str,
    ) -> Result<u128, Box<dyn Error>> {
        match self {
            Self::Warm(workload) => {
                let executor = lua.enter(|ctx| {
                    ctx.stash(Executor::start(ctx, ctx.fetch(closure.unwrap()).into(), ()))
                });
                let start = Instant::now();
                assert_eq!(
                    black_box(lua.execute::<i64>(&executor)?),
                    workload.expected,
                    "{}",
                    workload.name
                );
                Ok(start.elapsed().as_nanos())
            }
            Self::Predicate => {
                let start = Instant::now();
                let mut selected = 0;
                for row in 0..10000 {
                    let executor = lua.enter(|ctx| {
                        ctx.set_global(
                            "free",
                            if row % 2 == 0 {
                                500000000i64
                            } else {
                                1500000000i64
                            },
                        );
                        ctx.stash(Executor::start(ctx, ctx.fetch(closure.unwrap()).into(), ()))
                    });
                    selected += i64::from(lua.execute::<bool>(&executor)?);
                }
                assert_eq!(black_box(selected), 5000);
                Ok(start.elapsed().as_nanos())
            }
            Self::Cold => {
                let start = Instant::now();
                let mut fresh = state(mode);
                let executor = fresh.try_enter(|ctx| {
                    let closure = Closure::load(ctx, Some(self.name()), self.source())?;
                    Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
                })?;
                assert_eq!(black_box(fresh.execute::<i64>(&executor)?), 42);
                let elapsed = start.elapsed().as_nanos();
                *lua = fresh;
                Ok(elapsed)
            }
        }
    }
}

fn paired(case: PairedCase<'_>, samples: usize) -> Result<bool, Box<dyn Error>> {
    let modes = ["off", "auto"];
    let mut states = Vec::new();
    for mode in modes {
        let mut lua = state(mode);
        if !lua.jit_capabilities().supported_target {
            return Err("paired native performance requires a supported target".into());
        }
        let closure = if matches!(case, PairedCase::Cold) {
            None
        } else {
            Some(lua.try_enter(|ctx| {
                let closure = Closure::load(ctx, Some(case.name()), case.source())?;
                Ok(ctx.stash(closure))
            })?)
        };
        states.push((lua, closure));
    }
    let mut timings = [Vec::with_capacity(samples), Vec::with_capacity(samples)];
    let mut ratios = Vec::with_capacity(samples);
    for sample in 0..samples + 2 {
        let mut pair = [0; 2];
        for index in if sample % 2 == 0 { [0, 1] } else { [1, 0] } {
            let (lua, closure) = &mut states[index];
            pair[index] = case.measure(lua, closure.as_ref(), modes[index])?;
        }
        if sample >= 2 {
            for index in 0..2 {
                timings[index].push(pair[index]);
            }
            ratios.push(pair[0] as f64 / pair[1] as f64);
        }
    }
    if matches!(case, PairedCase::Cold) {
        assert_eq!(states[1].0.jit_stats().compilation_requests, 0);
        assert_eq!(states[1].0.jit_stats().native_entries, 0);
    } else {
        assert!(
            states[1].0.jit_stats().native_instructions > 0,
            "{} lacks native coverage",
            case.name()
        );
    }
    assert_eq!(states[0].0.jit_stats().native_entries, 0);
    let [off, auto] = timings;
    let off = report(case.name(), "off", off, &states[0].0);
    let auto = report(case.name(), "auto", auto, &states[1].0);
    let speedup = off as f64 / auto as f64;
    ratios.sort_by(f64::total_cmp);
    let threshold = case.minimum_speedup();
    let passed = threshold.is_none_or(|minimum| speedup >= minimum);
    println!("paired_case={} ratio_of_medians={speedup:.4} median_paired_speedup={:.4} min_paired_speedup={:.4} max_paired_speedup={:.4} minimum_speedup={} result={}", case.name(), ratios[ratios.len()/2], ratios[0], ratios[ratios.len()-1], threshold.map_or("unscored".to_owned(), |minimum| format!("{minimum:.4}")), if threshold.is_none() {"unscored"} else if passed {"passed"} else {"failed"});
    Ok(passed)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut mode = String::from("off");
    let mut samples = 11;
    let mut selected = None;
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => mode = args.next().ok_or("missing mode")?,
            "--samples" => samples = args.next().ok_or("missing samples")?.parse()?,
            "--case" => selected = Some(args.next().ok_or("missing case")?),
            "--check" => check = true,
            _ => return Err(format!("unexpected argument: {arg}").into()),
        }
    }
    if !matches!(mode.as_str(), "off" | "auto" | "force" | "paired")
        || !(3..=1000).contains(&samples)
    {
        return Err("mode must be off|auto|force|paired; samples must be 3..=1000".into());
    }
    if check && (mode != "paired" || samples < 11) {
        return Err("--check requires paired mode and at least 11 samples".into());
    }
    if let Some(name) = &selected {
        if !WORKLOADS.iter().any(|case| case.name == name)
            && name != "oslo_predicate"
            && name != "cold_config"
        {
            return Err(format!("unknown benchmark: {name}").into());
        }
    }
    println!(
        "luna={} target={}-{} opt_level={} mode={mode}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        std::env::consts::OS,
        option_env!("LUNA_BENCH_OPT_LEVEL").unwrap_or("unspecified")
    );
    if mode == "paired" {
        let mut failures = 0;
        for case in WORKLOADS
            .iter()
            .map(PairedCase::Warm)
            .chain([PairedCase::Predicate, PairedCase::Cold])
        {
            if selected.as_deref().is_none_or(|name| name == case.name()) {
                failures += usize::from(!paired(case, samples)?);
            }
        }
        if check && failures > 0 {
            return Err(format!("{failures} workload performance thresholds failed").into());
        }
        return Ok(());
    }
    for workload in WORKLOADS {
        if selected.as_deref().is_none_or(|name| name == workload.name) {
            warm(workload, &mode, samples)?;
        }
    }
    if selected
        .as_deref()
        .is_none_or(|name| name == "oslo_predicate")
    {
        oslo_predicate(&mode, samples)?;
    }
    if selected.as_deref().is_none_or(|name| name == "cold_config") {
        cold(&mode, samples)?;
    }
    Ok(())
}
