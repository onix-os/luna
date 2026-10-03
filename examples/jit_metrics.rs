use std::{error::Error, hint::black_box, time::Instant};

use luna::{Callback, CallbackReturn, Closure, Executor, Fuel, JitConfig, JitMode, JitStats, Lua};

#[path = "jit_support/churn.rs"]
mod churn;
#[path = "jit_support/suspension.rs"]
mod suspension;
#[path = "jit_support/workloads.rs"]
mod workloads;
use workloads::{Workload, COLD_SOURCE, PREDICATE_SOURCE, WORKLOADS};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const COLD: Workload = Workload {
    name: "cold_config",
    source: COLD_SOURCE,
    expected: 42,
};
const OSLO: Workload = Workload {
    name: "oslo_predicate",
    source: PREDICATE_SOURCE,
    expected: 5000,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Off,
    Auto,
    Prepared,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Prepared => "prepared",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    modes: Vec<Mode>,
    samples: usize,
    fuel: i32,
    case: Option<String>,
}

fn options(args: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut options = Options {
        modes: vec![Mode::Off, Mode::Auto, Mode::Prepared],
        samples: 3,
        fuel: 64,
        case: None,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("missing argument value")?;
        match arg.as_str() {
            "--mode" => {
                options.modes = match value.as_str() {
                    "off" => vec![Mode::Off],
                    "auto" => vec![Mode::Auto],
                    "prepared" => vec![Mode::Prepared],
                    "all" => vec![Mode::Off, Mode::Auto, Mode::Prepared],
                    _ => return Err("mode must be off|auto|prepared|all".into()),
                }
            }
            "--samples" => options.samples = value.parse()?,
            "--fuel" => options.fuel = value.parse()?,
            "--case" => options.case = Some(value),
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if !(1..=100).contains(&options.samples) || !(1..=65536).contains(&options.fuel) {
        return Err("samples must be 1..=100; fuel must be 1..=65536".into());
    }
    if options.fuel > 64
        && options
            .case
            .as_ref()
            .is_none_or(|name| name == "cache_churn")
    {
        return Err("cache_churn requires fuel 1..=64; select another case for larger fuel".into());
    }
    if options.case.as_ref().is_some_and(|name| {
        name != COLD.name
            && name != OSLO.name
            && name != "cache_churn"
            && name != "coroutine_resume"
            && name != "foreign_await"
            && !WORKLOADS.iter().any(|case| case.name == name)
    }) {
        return Err("unknown metrics case".into());
    }
    if options.case.as_deref() == Some("foreign_await") && !cfg!(feature = "async") {
        return Err("foreign_await requires the async feature".into());
    }
    Ok(options)
}

#[derive(Default, Debug)]
struct Observation {
    steps: usize,
    step_total_ns: u128,
    max_step_ns: u128,
    host_total_ns: u128,
    max_host_ns: u128,
    max_logical_work: u64,
    max_fuel_debit: i64,
    first_native_ns: Option<u128>,
    service_calls: usize,
    service_total_ns: u128,
    max_service_ns: u128,
    queued_service_calls: usize,
    queued_service_total_ns: u128,
    queue_peak: usize,
    code_peak: usize,
    gc_peak: usize,
}

impl Observation {
    fn resources(&mut self, stats: JitStats, gc_bytes: usize) {
        self.queue_peak = self.queue_peak.max(stats.queued_requests);
        self.code_peak = self.code_peak.max(stats.code_bytes);
        self.gc_peak = self.gc_peak.max(gc_bytes);
    }

    fn step(&mut self, before: JitStats, after: JitStats, times: [u128; 3], fuel_debit: i64) {
        assert_eq!(before.installed_regions, after.installed_regions);
        assert_eq!(before.compilation_failures, after.compilation_failures);
        self.steps += 1;
        let [step_ns, host_ns, elapsed_ns] = times;
        self.step_total_ns += step_ns;
        self.max_step_ns = self.max_step_ns.max(step_ns);
        self.host_total_ns += host_ns;
        self.max_host_ns = self.max_host_ns.max(host_ns);
        let native = after.native_instructions - before.native_instructions;
        let interpreted = after.interpreted_instructions - before.interpreted_instructions;
        self.max_logical_work = self.max_logical_work.max(native + interpreted);
        self.max_fuel_debit = self.max_fuel_debit.max(fuel_debit);
        if native > 0 && self.first_native_ns.is_none() {
            self.first_native_ns = Some(elapsed_ns);
        }
    }

    fn service(&mut self, queued: usize, elapsed_ns: u128) {
        self.service_calls += 1;
        self.service_total_ns += elapsed_ns;
        self.max_service_ns = self.max_service_ns.max(elapsed_ns);
        if queued > 0 {
            self.queued_service_calls += 1;
            self.queued_service_total_ns += elapsed_ns;
        }
    }
}

fn measure(workload: &Workload, mode: Mode, sample: usize, fuel: i32) -> Result<()> {
    let mut lua = Lua::core();
    if mode != Mode::Off {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        })?;
    }
    lua.enter(|ctx| {
        ctx.set_global(
            "host_increment",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let value: i64 = stack.consume(ctx)?;
                stack.replace(ctx, value.wrapping_add(1));
                Ok(CallbackReturn::Return)
            }),
        );
    });
    let start = Instant::now();
    let closure = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some(workload.name), workload.source)?;
        Ok(ctx.stash(closure))
    })?;
    let load_ns = start.elapsed().as_nanos();
    let mut observation = Observation::default();
    observation.resources(lua.jit_stats(), lua.total_memory());
    let prepare_start = Instant::now();
    let prepared = if mode == Mode::Prepared {
        lua.prepare_jit()?
    } else {
        0
    };
    let prepare_batch_ns = if mode == Mode::Prepared {
        prepare_start.elapsed().as_nanos()
    } else {
        0
    };
    observation.resources(lua.jit_stats(), lua.total_memory());
    let executions = if workload.name == OSLO.name { 10000 } else { 1 };
    let mut result = 0;
    for row in 0..executions {
        let executor = lua.enter(|ctx| {
            if workload.name == OSLO.name {
                ctx.set_global(
                    "free",
                    if row % 2 == 0 {
                        500000000i64
                    } else {
                        1500000000i64
                    },
                );
            }
            ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ()))
        });
        loop {
            if observation.steps >= 1_000_000 {
                return Err("metrics execution exceeded the host step cap".into());
            }
            let queued = lua.jit_stats().queued_requests;
            let service_start = Instant::now();
            lua.service_jit()?;
            observation.service(queued, service_start.elapsed().as_nanos());
            let before = lua.jit_stats();
            observation.resources(before, lua.total_memory());
            let host_start = Instant::now();
            let (done, step_ns, remaining) = lua.enter(|ctx| {
                let mut budget = Fuel::with(fuel);
                let step_start = Instant::now();
                let done = ctx.fetch(&executor).step(ctx, &mut budget);
                (done, step_start.elapsed().as_nanos(), budget.remaining())
            });
            let host_ns = host_start.elapsed().as_nanos();
            let observed_ns = start.elapsed().as_nanos();
            let after = lua.jit_stats();
            observation.step(
                before,
                after,
                [step_ns, host_ns, observed_ns],
                i64::from(fuel) - i64::from(remaining),
            );
            observation.resources(after, lua.total_memory());
            if done? {
                break;
            }
        }
        let returned = if workload.name == OSLO.name {
            i64::from(lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<bool>(ctx)?)?)
        } else {
            lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<i64>(ctx)?)?
        };
        if workload.name == OSLO.name {
            assert_eq!(returned, i64::from(row % 2 == 0));
        }
        result += returned;
    }
    assert_eq!(black_box(result), workload.expected, "{}", workload.name);
    let total_ns = start.elapsed().as_nanos();
    let stats = lua.jit_stats();
    if mode == Mode::Off {
        assert_eq!(stats.native_instructions, 0);
        assert_eq!(stats.compilation_requests, 0);
        assert!(observation.first_native_ns.is_none());
    } else if workload.name != COLD.name || mode == Mode::Prepared {
        assert!(stats.native_instructions > 0, "{}", workload.name);
        assert!(observation.first_native_ns.is_some());
    } else {
        assert_eq!(stats.native_instructions, 0);
        assert_eq!(stats.compilation_requests, 0);
    }
    let native_coverage = stats.native_instructions as f64
        / (stats.native_instructions + stats.interpreted_instructions).max(1) as f64;
    println!(
        "metrics_case={} mode={} sample={sample} verified=1 executions={executions} fuel={fuel} load_ns={load_ns} prepare_batch_ns={prepare_batch_ns} prepared={prepared} total_session_ns={total_ns} first_native_observed_ns={} steps={} step_total_ns={} max_step_ns={} host_enter_total_ns={} max_host_enter_ns={} max_logical_vm_work={} max_fuel_debit={} service_calls={} service_total_ns={} max_service_ns={} queued_service_calls={} queued_service_total_ns={} native_coverage={native_coverage:.6} native_instructions={} interpreted_instructions={} native_entries={} guard_exits={} native_interpreter_exits={} native_budget_exits={} native_panic_exits={} helper_declines={} installed_regions={} compilation_requests={} compilation_failures={} cache_evictions={} observed_queue_peak={} queue_current={} observed_code_peak_bytes={} code_current_bytes={} metadata_current_bytes={} metadata_peak_bytes={} snapshot_current_bytes={} snapshot_peak_bytes={} observed_gc_peak_bytes={} gc_current_bytes={}",
        workload.name,
        mode.name(),
        observation.first_native_ns.map_or("none".to_owned(), |ns| ns.to_string()),
        observation.steps,
        observation.step_total_ns,
        observation.max_step_ns,
        observation.host_total_ns,
        observation.max_host_ns,
        observation.max_logical_work,
        observation.max_fuel_debit,
        observation.service_calls,
        observation.service_total_ns,
        observation.max_service_ns,
        observation.queued_service_calls,
        observation.queued_service_total_ns,
        stats.native_instructions,
        stats.interpreted_instructions,
        stats.native_entries,
        stats.guard_exits,
        stats.native_interpreter_exits,
        stats.native_budget_exits,
        stats.native_panic_exits,
        stats.helper_declines,
        stats.installed_regions,
        stats.compilation_requests,
        stats.compilation_failures,
        stats.cache_evictions,
        observation.queue_peak,
        stats.queued_requests,
        observation.code_peak,
        stats.code_bytes,
        stats.metadata_bytes,
        stats.metadata_peak_bytes,
        stats.snapshot_bytes,
        stats.snapshot_peak_bytes,
        observation.gc_peak,
        lua.total_memory(),
    );
    Ok(())
}

fn main() -> Result<()> {
    let options = options(std::env::args().skip(1))?;
    if !luna::JitCapabilities::current().supported_target {
        return Err("native scheduling metrics require a supported target".into());
    }
    println!(
        "jit_metrics_protocol=1 target_arch={} target_os={} opt_level={} samples={} async_enabled={} slice_scope=executor_step host_scope=enter_including_gc preparation_scope=registered_batch service_scope=maintenance_snapshot_backend native_timestamp=post_host_slice memory_excludes=compiler_fixed_owners_allocator_overhead_rss",
        std::env::consts::ARCH,
        std::env::consts::OS,
        option_env!("LUNA_METRICS_OPT_LEVEL").unwrap_or("unrecorded"),
        options.samples,
        cfg!(feature = "async"),
    );
    for workload in WORKLOADS.iter().chain([&OSLO, &COLD]) {
        if options
            .case
            .as_ref()
            .is_some_and(|name| name != workload.name)
        {
            continue;
        }
        for sample in 0..options.samples {
            for &mode in &options.modes {
                measure(workload, mode, sample, options.fuel)?;
            }
        }
    }
    if options
        .case
        .as_ref()
        .is_none_or(|name| name == "cache_churn")
    {
        for sample in 0..options.samples {
            for &mode in &options.modes {
                let churn_mode = match mode {
                    Mode::Off => churn::Mode::Off,
                    Mode::Auto => churn::Mode::Auto,
                    Mode::Prepared => churn::Mode::Prepared,
                };
                let report = churn::run(churn_mode, options.fuel)?;
                println!("churn_report=cache_churn mode={} sample={sample} calibration_ns={} module_bytes={} quota_bytes={} prepare_ns={} sources=8 attempts_per_source=2 queue_limit=1 counter_scope=completed_nontransition_vm_work", mode.name(), report.calibration_ns, report.module_bytes, report.quota_bytes, report.prepare_ns);
                for pass in &report.passes {
                    println!("churn_pass={} mode={} sample={sample} verified=1 fuel={} elapsed_ns={} native_executions={} steps={} max_step_ns={} max_host_enter_ns={} service_total_ns={} max_service_ns={} service_errors={} counted_vm_work_max={} fuel_debit_max={} observed_queue_peak={} native_instructions={} interpreted_instructions={} requests={} failures={} installed={} evictions={} eviction_refusals={} code_bytes={} metadata_bytes={} metadata_peak_bytes={} snapshot_peak_bytes={}", pass.name, mode.name(), options.fuel, pass.elapsed_ns, pass.native_executions, pass.steps, pass.max_step_ns, pass.max_host_ns, pass.service_ns, pass.max_service_ns, pass.service_errors, pass.counted_work_max, pass.fuel_debit_max, pass.queue_peak, pass.after.native_instructions - pass.before.native_instructions, pass.after.interpreted_instructions - pass.before.interpreted_instructions, pass.after.compilation_requests - pass.before.compilation_requests, pass.after.compilation_failures - pass.before.compilation_failures, pass.after.installed_regions - pass.before.installed_regions, pass.after.cache_evictions - pass.before.cache_evictions, pass.after.cache_eviction_refusals - pass.before.cache_eviction_refusals, pass.after.code_bytes, pass.after.metadata_bytes, pass.after.metadata_peak_bytes, pass.after.snapshot_peak_bytes);
                }
                println!("churn_cleanup=cache_churn mode={} sample={sample} registered={} code_bytes={} metadata_bytes={} snapshot_bytes={}", mode.name(), report.final_stats.registered_prototypes, report.final_stats.code_bytes, report.final_stats.metadata_bytes, report.final_stats.snapshot_bytes);
            }
        }
    }
    for case in ["coroutine_resume", "foreign_await"] {
        if (case == "foreign_await" && !cfg!(feature = "async"))
            || options.case.as_ref().is_some_and(|name| name != case)
        {
            continue;
        }
        for sample in 0..options.samples {
            for &mode in &options.modes {
                let suspension_mode = match mode {
                    Mode::Off => suspension::Mode::Off,
                    Mode::Auto => suspension::Mode::Auto,
                    Mode::Prepared => suspension::Mode::Prepared,
                };
                let report = suspension::run(case, suspension_mode, options.fuel)?;
                let stats = report.stats;
                let coverage = stats.native_instructions as f64
                    / (stats.native_instructions + stats.interpreted_instructions).max(1) as f64;
                let native_segments = report
                    .native_segments
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let table_segments = report
                    .native_table_write_segments
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let values = report
                    .boundary_values
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let wait_scope = if case == "foreign_await" {
                    "synthetic_three_pending_polls"
                } else {
                    "host_resume"
                };
                println!("suspension_case={case} mode={} sample={sample} verified=1 fuel={} hot_threshold=1 load_ns={} prepare_batch_ns={} prepared={} total_session_ns={} first_native_observed_ns={} suspensions={} native_segments={native_segments} native_table_write_segments={table_segments} boundary_values={values} steps={} max_step_ns={} max_host_enter_ns={} service_total_ns={} max_service_ns={} counted_vm_work_max={} fuel_debit_max={} pending_polls={} ready_polls={} wakes={} foreign_poll_total_ns={} max_foreign_poll_ns={} forced_gc_total_ns={} forced_gc_cycles={} native_coverage={coverage:.6} native_instructions={} interpreted_instructions={} native_entries={} observed_queue_peak={} observed_code_peak_bytes={} observed_gc_peak_bytes={} metadata_peak_bytes={} snapshot_peak_bytes={} counter_scope=completed_nontransition_vm_work wait_scope={wait_scope}", mode.name(), options.fuel, report.load_ns, report.prepare_ns, report.prepared, report.elapsed_ns, report.first_native_ns.map_or("none".to_owned(), |ns| ns.to_string()), report.suspensions, report.steps, report.max_step_ns, report.max_host_ns, report.service_ns, report.max_service_ns, report.counted_work_max, report.fuel_debit_max, report.pending_polls, report.ready_polls, report.wakes, report.poll_ns, report.max_poll_ns, report.forced_gc_ns, report.forced_gc_cycles, stats.native_instructions, stats.interpreted_instructions, stats.native_entries, report.queue_peak, report.code_peak, report.gc_peak, stats.metadata_peak_bytes, stats.snapshot_peak_bytes);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options> {
        options(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn options_select_modes_cases_and_bounded_sampling() {
        assert_eq!(
            parse(&[]).unwrap().modes,
            [Mode::Off, Mode::Auto, Mode::Prepared]
        );
        for (name, mode) in [
            ("off", Mode::Off),
            ("auto", Mode::Auto),
            ("prepared", Mode::Prepared),
        ] {
            let options = parse(&[
                "--mode",
                name,
                "--samples",
                "1",
                "--fuel",
                "1",
                "--case",
                "cold_config",
            ])
            .unwrap();
            assert_eq!(options.modes, [mode]);
            assert_eq!(options.samples, 1);
            assert_eq!(options.fuel, 1);
            assert_eq!(options.case.as_deref(), Some("cold_config"));
        }
        assert_eq!(
            parse(&[
                "--samples",
                "100",
                "--fuel",
                "65536",
                "--case",
                "integer_loop"
            ])
            .unwrap()
            .samples,
            100
        );
        assert_eq!(
            parse(&["--case", "oslo_predicate"])
                .unwrap()
                .case
                .as_deref(),
            Some("oslo_predicate")
        );
    }

    #[test]
    fn invalid_options_do_not_silently_skip_workloads() {
        for args in [
            vec!["--mode", "force"],
            vec!["--mode"],
            vec!["--samples", "0"],
            vec!["--samples", "101"],
            vec!["--samples", "bad"],
            vec!["--fuel", "0"],
            vec!["--fuel", "-1"],
            vec!["--fuel", "65537"],
            vec!["--fuel", "65"],
            vec!["--fuel", "65", "--case", "cache_churn"],
            vec!["--case", "absent"],
            vec!["--unknown", "value"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn suspension_options_select_available_cases_without_enabling_async_implicitly() {
        let options = parse(&["--case", "coroutine_resume", "--fuel", "65536"]).unwrap();
        assert_eq!(options.case.as_deref(), Some("coroutine_resume"));
        let foreign = parse(&["--case", "foreign_await"]);
        assert_eq!(foreign.is_ok(), cfg!(feature = "async"));
    }

    #[test]
    fn observation_keeps_first_native_timestamp_and_independent_maxima() {
        let mut observation = Observation::default();
        let before = JitStats::default();
        let first = JitStats {
            interpreted_instructions: 4,
            ..before
        };
        observation.step(before, first, [7, 11, 15], 9);
        assert_eq!(observation.first_native_ns, None);
        let second = JitStats {
            native_instructions: 3,
            interpreted_instructions: 5,
            ..first
        };
        observation.step(first, second, [5, 13, 42], 7);
        let third = JitStats {
            native_instructions: 8,
            ..second
        };
        observation.step(second, third, [9, 12, 99], 11);
        assert_eq!(observation.first_native_ns, Some(42));
        assert_eq!(observation.steps, 3);
        assert_eq!(observation.step_total_ns, 21);
        assert_eq!(observation.host_total_ns, 36);
        assert_eq!(observation.max_step_ns, 9);
        assert_eq!(observation.max_host_ns, 13);
        assert_eq!(observation.max_logical_work, 5);
        assert_eq!(observation.max_fuel_debit, 11);
        observation.service(0, 8);
        observation.service(2, 17);
        observation.resources(
            JitStats {
                queued_requests: 2,
                code_bytes: 64,
                ..third
            },
            128,
        );
        observation.resources(
            JitStats {
                queued_requests: 0,
                code_bytes: 0,
                ..third
            },
            32,
        );
        assert_eq!(observation.service_calls, 2);
        assert_eq!(observation.service_total_ns, 25);
        assert_eq!(observation.max_service_ns, 17);
        assert_eq!(observation.queued_service_calls, 1);
        assert_eq!(observation.queued_service_total_ns, 17);
        assert_eq!(
            (
                observation.queue_peak,
                observation.code_peak,
                observation.gc_peak
            ),
            (2, 64, 128)
        );
    }

    #[test]
    #[should_panic]
    fn observation_rejects_installation_inside_executor_step() {
        let mut observation = Observation::default();
        observation.step(
            JitStats::default(),
            JitStats {
                installed_regions: 1,
                ..Default::default()
            },
            [1; 3],
            1,
        );
    }

    #[test]
    #[should_panic]
    fn observation_rejects_failed_compilation_inside_executor_step() {
        Observation::default().step(
            JitStats::default(),
            JitStats {
                compilation_failures: 1,
                ..Default::default()
            },
            [1; 3],
            1,
        );
    }
}
