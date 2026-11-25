use std::{error::Error, time::Instant};

use luna::{Closure, Executor, Fuel, JitConfig, JitError, JitMode, JitStats, Lua, StashedClosure};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const SOURCES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Auto,
    Prepared,
}

#[derive(Debug)]
pub struct Pass {
    pub name: &'static str,
    pub elapsed_ns: u128,
    pub native_executions: usize,
    pub steps: usize,
    pub max_step_ns: u128,
    pub max_host_ns: u128,
    pub service_ns: u128,
    pub max_service_ns: u128,
    pub service_errors: usize,
    pub counted_work_max: u64,
    pub fuel_debit_max: i64,
    pub queue_peak: usize,
    pub before: JitStats,
    pub after: JitStats,
}

#[derive(Debug)]
pub struct Report {
    pub calibration_ns: u128,
    pub module_bytes: usize,
    pub quota_bytes: usize,
    pub prepare_ns: u128,
    pub passes: Vec<Pass>,
    pub final_stats: JitStats,
}

fn load(lua: &mut Lua, seed: usize) -> Result<StashedClosure> {
    Ok(lua.try_enter(|ctx| {
        let text = format!("local sum={seed} for i=1,100 do sum=sum+i end return sum");
        Ok(ctx.stash(Closure::load(ctx, Some("cache-churn"), text.as_bytes())?))
    })?)
}

fn config(mode: Mode, quota: usize) -> JitConfig {
    JitConfig {
        mode: if mode == Mode::Off {
            JitMode::Off
        } else {
            JitMode::Auto
        },
        hot_threshold: 1,
        max_queue_entries: 1,
        max_code_bytes: quota,
        ..Default::default()
    }
}

fn calibrate() -> Result<usize> {
    let mut lua = Lua::empty();
    lua.set_jit_config(config(Mode::Auto, JitConfig::default().max_code_bytes))?;
    let closure = load(&mut lua, 0)?;
    assert_eq!(lua.prepare_jit()?, 1);
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor)?, 5050);
    assert!(lua.jit_stats().native_instructions > 0);
    let bytes = lua.jit_stats().code_bytes;
    assert!(bytes > 0);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    Ok(bytes)
}

fn pass(lua: &mut Lua, closures: &[StashedClosure], name: &'static str, fuel: i32) -> Result<Pass> {
    let mut report = Pass {
        name,
        elapsed_ns: 0,
        native_executions: 0,
        steps: 0,
        max_step_ns: 0,
        max_host_ns: 0,
        service_ns: 0,
        max_service_ns: 0,
        service_errors: 0,
        counted_work_max: 0,
        fuel_debit_max: 0,
        queue_peak: 0,
        before: lua.jit_stats(),
        after: JitStats::default(),
    };
    let start = Instant::now();
    for (seed, closure) in closures.iter().enumerate() {
        let native_before = lua.jit_stats().native_instructions;
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
        loop {
            if report.steps >= 10000 {
                return Err("cache churn exceeded its host step cap".into());
            }
            let service_start = Instant::now();
            match lua.service_jit() {
                Ok(count) => assert!(count <= 1),
                Err(JitError::ResourceLimit("native mappings")) => report.service_errors += 1,
                Err(error) => return Err(error.into()),
            }
            let service_ns = service_start.elapsed().as_nanos();
            report.service_ns += service_ns;
            report.max_service_ns = report.max_service_ns.max(service_ns);
            let before = lua.jit_stats();
            let host_start = Instant::now();
            let (done, step_ns, remaining) = lua.enter(|ctx| {
                let mut budget = Fuel::with(fuel);
                let step_start = Instant::now();
                let done = ctx.fetch(&executor).step(ctx, &mut budget);
                (done, step_start.elapsed().as_nanos(), budget.remaining())
            });
            report.max_host_ns = report.max_host_ns.max(host_start.elapsed().as_nanos());
            report.max_step_ns = report.max_step_ns.max(step_ns);
            let after = lua.jit_stats();
            assert_eq!(before.installed_regions, after.installed_regions);
            assert_eq!(before.compilation_failures, after.compilation_failures);
            assert!(after.code_bytes <= lua.jit_config().max_code_bytes);
            assert!(after.metadata_bytes <= lua.jit_config().max_metadata_bytes);
            assert!(after.queued_requests <= 1);
            report.queue_peak = report.queue_peak.max(after.queued_requests);
            let work = after.native_instructions - before.native_instructions
                + after.interpreted_instructions
                - before.interpreted_instructions;
            report.counted_work_max = report.counted_work_max.max(work);
            report.fuel_debit_max = report
                .fuel_debit_max
                .max(i64::from(fuel) - i64::from(remaining));
            report.steps += 1;
            if done? {
                break;
            }
        }
        let actual = lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<i64>(ctx)?)?;
        assert_eq!(actual, 5050 + seed as i64);
        report.native_executions +=
            usize::from(lua.jit_stats().native_instructions > native_before);
        lua.gc_collect();
    }
    report.elapsed_ns = start.elapsed().as_nanos();
    report.after = lua.jit_stats();
    assert_eq!(report.after.registered_prototypes, SOURCES);
    Ok(report)
}

pub fn run(mode: Mode, fuel: i32) -> Result<Report> {
    if !(1..=64).contains(&fuel) {
        return Err("cache churn fuel must be 1..=64".into());
    }
    let calibration_start = Instant::now();
    let module_bytes = calibrate()?;
    let calibration_ns = calibration_start.elapsed().as_nanos();
    let quota_bytes = module_bytes
        .checked_mul(2)
        .ok_or("mapping quota overflow")?;
    let mut lua = Lua::empty();
    lua.set_jit_config(config(mode, quota_bytes))?;
    let mut closures = vec![load(&mut lua, 0)?];
    let prepare_start = Instant::now();
    if mode == Mode::Prepared {
        assert_eq!(lua.prepare_jit()?, 1);
    }
    let prepare_ns = if mode == Mode::Prepared {
        prepare_start.elapsed().as_nanos()
    } else {
        0
    };
    for seed in 1..SOURCES {
        closures.push(load(&mut lua, seed)?);
    }
    let mut passes = Vec::new();
    for name in ["warm", "revisit", "steady"] {
        passes.push(pass(&mut lua, &closures, name, fuel)?);
    }
    if mode != Mode::Off {
        let [warm, revisit, steady] = passes.as_slice() else {
            unreachable!()
        };
        assert_eq!(warm.native_executions, SOURCES);
        assert_eq!(warm.after.installed_regions, SOURCES as u64);
        assert_eq!(warm.after.cache_evictions, (SOURCES - 2) as u64);
        assert_eq!(warm.after.compilation_failures, (SOURCES - 2) as u64);
        assert_eq!(revisit.native_executions, 2);
        assert_eq!(revisit.service_errors, 2);
        assert_eq!(steady.native_executions, 2);
        assert_eq!(steady.service_errors, 0);
        assert_eq!(
            steady.after.compilation_requests,
            revisit.after.compilation_requests
        );
        assert_eq!(
            steady.after.compilation_failures,
            revisit.after.compilation_failures
        );
        assert_eq!(steady.after.cache_evictions, revisit.after.cache_evictions);
        assert_eq!(
            steady.after.installed_regions,
            revisit.after.installed_regions
        );
        assert_eq!(steady.after.code_bytes, quota_bytes);
        assert_eq!(
            steady.after.installed_regions + steady.after.compilation_failures,
            (2 * SOURCES) as u64
        );
    }
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().registered_prototypes, SOURCES);
    passes.push(pass(&mut lua, &closures, "reset", fuel)?);
    if mode != Mode::Off {
        assert_eq!(passes[3].native_executions, SOURCES);
        assert_eq!(
            passes[3].after.installed_regions - passes[3].before.installed_regions,
            SOURCES as u64
        );
    } else {
        for pass in &passes {
            assert_eq!(pass.native_executions, 0);
            assert_eq!(pass.after.compilation_requests, 0);
            assert_eq!(pass.after.code_bytes, 0);
        }
    }
    drop(closures);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.service_jit()?, 0);
    let final_stats = lua.jit_stats();
    assert_eq!(final_stats.registered_prototypes, 0);
    assert_eq!(final_stats.code_bytes, 0);
    assert_eq!(final_stats.metadata_bytes, 0);
    assert_eq!(final_stats.snapshot_bytes, 0);
    Ok(Report {
        calibration_ns,
        module_bytes,
        quota_bytes,
        prepare_ns,
        passes,
        final_stats,
    })
}
