use std::{error::Error, hint::black_box, path::PathBuf, process::Command, time::Instant};

use luna::{Callback, CallbackReturn, Closure, Executor, Lua};

#[path = "jit_support/workloads.rs"]
mod workloads;
use workloads::{COLD_SOURCE, PREDICATE_SOURCE, WORKLOADS};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const CASES: [&str; 9] = [
    "integer_loop",
    "float_loop",
    "array_table",
    "closure_upvalue",
    "polymorphic_metamethod",
    "rust_callbacks",
    "allocation_gc",
    "oslo_predicate",
    "cold_config",
];

fn state(mode: &str) -> Result<Lua> {
    let mut lua = Lua::core();
    #[cfg(feature = "jit")]
    if mode == "auto" {
        lua.set_jit_config(luna::JitConfig {
            mode: luna::JitMode::Auto,
            ..Default::default()
        })?;
    }
    #[cfg(not(feature = "jit"))]
    if mode != "off" {
        return Err("Auto requires the jit feature".into());
    }
    lua.enter(|ctx| {
        let increment = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value: i64 = stack.consume(ctx)?;
            stack.replace(ctx, value.wrapping_add(1));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("host_increment", increment);
    });
    Ok(lua)
}

fn native_work(lua: &Lua, mode: &str, cold: bool) -> u64 {
    #[cfg(feature = "jit")]
    {
        let stats = lua.jit_stats();
        if mode == "off" || cold {
            assert_eq!(stats.compilation_requests, 0);
            assert_eq!(stats.native_entries, 0);
            assert_eq!(stats.native_instructions, 0);
        } else {
            assert!(stats.native_instructions > 0);
        }
        stats.native_instructions
    }
    #[cfg(not(feature = "jit"))]
    {
        let _ = (lua, mode, cold);
        0
    }
}

fn worker(mode: &str, iterations: usize, selected: Option<&str>) -> Result<()> {
    println!(
        "feature_cost_protocol=1 jit_compiled={} mode={mode}",
        usize::from(cfg!(feature = "jit"))
    );
    for workload in WORKLOADS {
        if selected.is_some_and(|name| name != workload.name) {
            continue;
        }
        let mut lua = state(mode)?;
        let closure = lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, Some(workload.name), workload.source)?;
            Ok(ctx.stash(closure))
        })?;
        let mut elapsed = 0;
        for iteration in 0..iterations + 2 {
            let executor =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
            let start = Instant::now();
            let result = lua.execute::<i64>(&executor)?;
            let duration = start.elapsed().as_nanos();
            assert_eq!(black_box(result), workload.expected, "{}", workload.name);
            if iteration >= 2 {
                elapsed += duration;
            }
        }
        println!(
            "case={} ns={elapsed} iterations={iterations} verified=1 native_instructions={}",
            workload.name,
            native_work(&lua, mode, false)
        );
    }

    if selected.is_some() {
        return Ok(());
    }
    let mut lua = state(mode)?;
    let closure = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("oslo-row-predicate"), PREDICATE_SOURCE)?;
        Ok(ctx.stash(closure))
    })?;
    let mut elapsed = 0;
    for iteration in 0..iterations + 2 {
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
                ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ()))
            });
            selected += i64::from(lua.execute::<bool>(&executor)?);
        }
        let duration = start.elapsed().as_nanos();
        assert_eq!(black_box(selected), 5000);
        if iteration >= 2 {
            elapsed += duration;
        }
    }
    println!("case=oslo_predicate ns={elapsed} iterations={iterations} verified=1 native_instructions={}", native_work(&lua, mode, false));

    let mut elapsed = 0;
    for _ in 0..iterations {
        let start = Instant::now();
        let mut fresh = state(mode)?;
        let executor = fresh.try_enter(|ctx| {
            let closure = Closure::load(ctx, Some("config"), COLD_SOURCE)?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        let result = fresh.execute::<i64>(&executor)?;
        elapsed += start.elapsed().as_nanos();
        assert_eq!(black_box(result), 42);
        native_work(&fresh, mode, true);
    }
    println!(
        "case=cold_config ns={elapsed} iterations={iterations} verified=1 native_instructions=0"
    );
    Ok(())
}

fn parse_worker(output: &str, compiled: bool, iterations: usize) -> Result<[u128; 9]> {
    let mut lines = output.lines();
    let header = format!(
        "feature_cost_protocol=1 jit_compiled={} mode=off",
        usize::from(compiled)
    );
    if lines.next() != Some(header.as_str()) {
        return Err("wrong feature/mode/protocol in cost worker".into());
    }
    let mut times = [0; 9];
    for (index, name) in CASES.iter().enumerate() {
        let fields: Vec<_> = lines
            .next()
            .ok_or("missing cost case")?
            .split_whitespace()
            .collect();
        if fields.len() != 5
            || fields[0] != format!("case={name}")
            || fields[2] != format!("iterations={iterations}")
            || fields[3] != "verified=1"
            || fields[4] != "native_instructions=0"
        {
            return Err("invalid, reordered, unverified or native cost worker case".into());
        }
        times[index] = fields[1]
            .strip_prefix("ns=")
            .ok_or("missing worker duration")?
            .parse()?;
        if times[index] == 0 {
            return Err("zero worker duration".into());
        }
    }
    if lines.next().is_some() {
        return Err("unexpected cost worker output".into());
    }
    Ok(times)
}

fn within_limit(baseline: u128, compiled_off: u128) -> bool {
    compiled_off as f64 / baseline as f64 <= 1.05
}

fn compare(paths: [PathBuf; 2], samples: usize, iterations: usize, check: bool) -> Result<()> {
    let sizes = [
        std::fs::metadata(&paths[0])?.len(),
        std::fs::metadata(&paths[1])?.len(),
    ];
    for (variant, path) in ["no_jit", "jit_off"].into_iter().zip(&paths) {
        println!(
            "binary={variant} path={} bytes={}",
            path.display(),
            std::fs::metadata(path)?.len()
        );
    }
    println!(
        "binary_delta_bytes={} binary_size_ratio={:.4}",
        i128::from(sizes[1]) - i128::from(sizes[0]),
        sizes[1] as f64 / sizes[0] as f64
    );
    println!("comparison=compiled_disabled samples={samples} iterations={iterations} warmups_per_child=2 max_overhead=0.05 process_startup_timed=false");
    let mut timings: [Vec<[u128; 9]>; 2] = [Vec::new(), Vec::new()];
    for sample in 0..samples {
        for index in if sample % 2 == 0 { [0, 1] } else { [1, 0] } {
            let output = Command::new(&paths[index])
                .args(["--worker", "--iterations", &iterations.to_string()])
                .output()?;
            if !output.status.success() {
                return Err(format!(
                    "cost worker {} failed: {} {}",
                    paths[index].display(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
                .into());
            }
            let times = parse_worker(std::str::from_utf8(&output.stdout)?, index == 1, iterations)?;
            println!(
                "pair={sample} variant={} durations_ns={times:?}",
                if index == 0 { "no_jit" } else { "jit_off" }
            );
            timings[index].push(times);
        }
    }
    let mut failures = 0;
    for (case, name) in CASES.iter().enumerate() {
        let mut baseline: Vec<_> = timings[0].iter().map(|sample| sample[case]).collect();
        let mut compiled: Vec<_> = timings[1].iter().map(|sample| sample[case]).collect();
        let mut paired: Vec<_> = timings[0]
            .iter()
            .zip(&timings[1])
            .map(|(base, jit)| jit[case] as f64 / base[case] as f64)
            .collect();
        baseline.sort_unstable();
        compiled.sort_unstable();
        paired.sort_by(f64::total_cmp);
        let median = samples / 2;
        let ratio = compiled[median] as f64 / baseline[median] as f64;
        let passed = within_limit(baseline[median], compiled[median]);
        failures += usize::from(!passed);
        println!("feature_cost_case={name} no_jit_median_ns={} no_jit_min_ns={} no_jit_max_ns={} jit_off_median_ns={} jit_off_min_ns={} jit_off_max_ns={} ratio_of_medians={ratio:.4} overhead_percent={:.2} median_paired_ratio={:.4} min_paired_ratio={:.4} max_paired_ratio={:.4} limit_ratio=1.0500 result={}", baseline[median], baseline[0], baseline[samples-1], compiled[median], compiled[0], compiled[samples-1], (ratio-1.0)*100.0, paired[median], paired[0], paired[samples-1], if passed { "passed" } else { "failed" });
    }
    if check && failures > 0 {
        return Err(format!("{failures} compiled-disabled overhead controls failed").into());
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut samples = 11;
    let mut iterations = 20;
    let mut mode = String::from("off");
    let mut paths = None;
    let mut check = false;
    let mut worker_requested = false;
    let mut selected = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--compare" => {
                paths = Some([
                    PathBuf::from(args.next().ok_or("missing baseline path")?),
                    PathBuf::from(args.next().ok_or("missing jit path")?),
                ])
            }
            "--worker" => worker_requested = true,
            "--mode" => mode = args.next().ok_or("missing mode")?,
            "--case" => selected = Some(args.next().ok_or("missing case")?),
            "--samples" => samples = args.next().ok_or("missing samples")?.parse()?,
            "--iterations" => iterations = args.next().ok_or("missing iterations")?.parse()?,
            "--check" => check = true,
            _ => return Err(format!("unexpected argument: {arg}").into()),
        }
    }
    if !(3..=1000).contains(&samples)
        || !(1..=1000).contains(&iterations)
        || !matches!(mode.as_str(), "off" | "auto")
        || (check && (paths.is_none() || samples < 11))
        || (paths.is_some() && (worker_requested || mode != "off" || selected.is_some()))
        || selected
            .as_deref()
            .is_some_and(|name| !WORKLOADS.iter().any(|case| case.name == name))
    {
        return Err("invalid mode/counts; --check requires --compare and >=11 pairs".into());
    }
    if let Some(paths) = paths {
        compare(paths, samples, iterations, check)
    } else {
        worker(&mode, iterations, selected.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(compiled: bool) -> String {
        let mut output = format!(
            "feature_cost_protocol=1 jit_compiled={} mode=off\n",
            usize::from(compiled)
        );
        for name in CASES {
            output.push_str(&format!(
                "case={name} ns=100 iterations=2 verified=1 native_instructions=0\n"
            ));
        }
        output
    }

    #[test]
    fn exact_worker_protocol_roundtrips() {
        for compiled in [false, true] {
            assert_eq!(
                parse_worker(&output(compiled), compiled, 2).unwrap(),
                [100; 9]
            );
        }
    }

    #[test]
    fn mismatched_unverified_missing_and_extra_records_are_refused() {
        let valid = output(true);
        for invalid in [
            valid.replace("jit_compiled=1", "jit_compiled=0"),
            valid.replace("mode=off", "mode=auto"),
            valid.replace("verified=1", "verified=0"),
            valid.replace("native_instructions=0", "native_instructions=1"),
            valid.replace("iterations=2", "iterations=3"),
            valid.replace("ns=100", "ns=0"),
            valid.replace("case=integer_loop", "case=float_loop"),
            valid.lines().take(9).collect::<Vec<_>>().join("\n"),
            format!("{valid}extra\n"),
        ] {
            assert!(parse_worker(&invalid, true, 2).is_err(), "{invalid}");
        }
    }

    #[test]
    fn disabled_limit_is_frozen_at_five_percent() {
        assert!(within_limit(1000, 1050));
        assert!(!within_limit(1000, 1051));
        assert!(within_limit(1000, 999));
    }

    #[test]
    fn shared_corpus_order_matches_the_worker_protocol() {
        assert_eq!(
            WORKLOADS
                .iter()
                .map(|workload| workload.name)
                .collect::<Vec<_>>(),
            CASES[..7]
        );
    }
}
