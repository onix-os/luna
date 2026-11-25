use std::{error::Error, time::Instant};

use luna::{Closure, Executor, ExecutorMode, Fuel, JitConfig, JitMode, JitStats, Lua};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Auto,
    Prepared,
}

#[derive(Default, Debug)]
pub struct Report {
    pub load_ns: u128,
    pub prepare_ns: u128,
    pub prepared: usize,
    pub elapsed_ns: u128,
    pub first_native_ns: Option<u128>,
    pub steps: usize,
    pub max_step_ns: u128,
    pub max_host_ns: u128,
    pub service_ns: u128,
    pub max_service_ns: u128,
    pub counted_work_max: u64,
    pub fuel_debit_max: i64,
    pub suspensions: usize,
    pub native_segments: Vec<u64>,
    pub native_table_write_segments: Vec<u64>,
    pub boundary_values: Vec<i64>,
    pub pending_polls: usize,
    pub ready_polls: usize,
    pub wakes: usize,
    pub poll_ns: u128,
    pub max_poll_ns: u128,
    pub forced_gc_ns: u128,
    pub forced_gc_cycles: usize,
    pub queue_peak: usize,
    pub code_peak: usize,
    pub gc_peak: usize,
    pub stats: JitStats,
}

impl Report {
    fn resources(&mut self, lua: &Lua) {
        let stats = lua.jit_stats();
        self.queue_peak = self.queue_peak.max(stats.queued_requests);
        self.code_peak = self.code_peak.max(stats.code_bytes);
        self.gc_peak = self.gc_peak.max(lua.total_memory());
    }
}

const COROUTINE: &[u8] = br#"
local t={sum=0} local identity=t
for i=1,200 do t.sum=t.sum+i end
coroutine.yield(t.sum)
assert(t==identity)
for i=1,200 do t.sum=t.sum+i end
coroutine.yield(t.sum)
assert(t==identity)
for i=1,200 do t.sum=t.sum+i end
return t.sum
"#;

#[cfg(feature = "async")]
const FOREIGN: &[u8] = br#"
local t={sum=0} local identity=t
for i=1,200 do t.sum=t.sum+i end
t.sum=t.sum+host_wait()
assert(t==identity)
for i=1,200 do t.sum=t.sum+i end
t.sum=t.sum+host_wait()
assert(t==identity)
for i=1,200 do t.sum=t.sum+i end
return t.sum
"#;

fn collect(lua: &mut Lua, report: &mut Report) {
    let before = lua.jit_stats();
    let start = Instant::now();
    lua.gc_collect();
    lua.gc_collect();
    report.forced_gc_cycles += 2;
    report.forced_gc_ns += start.elapsed().as_nanos();
    let after = lua.jit_stats();
    assert_eq!(before.native_instructions, after.native_instructions);
    assert_eq!(before.installed_regions, after.installed_regions);
    assert_eq!(before.compilation_failures, after.compilation_failures);
    report.resources(lua);
}

fn segment(lua: &Lua, report: &mut Report, previous: &mut (u64, u64), mode: Mode) {
    let stats = lua.jit_stats();
    let delta = stats.native_instructions - previous.0;
    let writes = stats.native_table_writes - previous.1;
    if mode == Mode::Off {
        assert_eq!(delta, 0);
        assert_eq!(writes, 0);
    } else if mode == Mode::Auto && report.native_segments.is_empty() {
        assert_eq!(delta > 0, writes > 0);
    } else {
        assert!(
            delta > 0,
            "no actual native work between suspension boundaries"
        );
        assert!(writes > 0, "no native table updates in suspension segment");
    }
    report.native_segments.push(delta);
    report.native_table_write_segments.push(writes);
    *previous = (stats.native_instructions, stats.native_table_writes);
}

pub fn run(case: &str, mode: Mode, fuel: i32) -> Result<Report> {
    if !(1..=65536).contains(&fuel) {
        return Err("suspension fuel must be 1..=65536".into());
    }
    let source = match case {
        "coroutine_resume" => COROUTINE,
        #[cfg(feature = "async")]
        "foreign_await" => FOREIGN,
        _ => return Err("unknown or unavailable suspension case".into()),
    };
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: if mode == Mode::Off {
            JitMode::Off
        } else {
            JitMode::Auto
        },
        hot_threshold: 1,
        ..Default::default()
    })?;
    #[cfg(feature = "async")]
    if case == "foreign_await" {
        foreign::install(&mut lua);
    }
    let start = Instant::now();
    let closure = lua.try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, Some(case), source)?)))?;
    let mut report = Report {
        load_ns: start.elapsed().as_nanos(),
        ..Default::default()
    };
    report.resources(&lua);
    if mode == Mode::Prepared {
        let preparation = Instant::now();
        report.prepared = lua.prepare_jit()?;
        report.prepare_ns = preparation.elapsed().as_nanos();
        assert!(report.prepared > 0);
    }
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
    let mut previous_native = (0, 0);
    loop {
        if report.steps >= 100_000 {
            return Err("suspension scenario exceeded its host slice cap".into());
        }
        let service = Instant::now();
        lua.service_jit()?;
        let service_ns = service.elapsed().as_nanos();
        report.service_ns += service_ns;
        report.max_service_ns = report.max_service_ns.max(service_ns);
        let before = lua.jit_stats();
        report.resources(&lua);
        let host = Instant::now();
        let (done, step_ns, remaining) = lua.enter(|ctx| {
            let mut budget = Fuel::with(fuel);
            let step = Instant::now();
            let done = ctx.fetch(&executor).step(ctx, &mut budget);
            (done, step.elapsed().as_nanos(), budget.remaining())
        });
        report.max_host_ns = report.max_host_ns.max(host.elapsed().as_nanos());
        report.max_step_ns = report.max_step_ns.max(step_ns);
        report.steps += 1;
        let after = lua.jit_stats();
        report.resources(&lua);
        assert_eq!(before.installed_regions, after.installed_regions);
        assert_eq!(before.compilation_failures, after.compilation_failures);
        let native = after.native_instructions - before.native_instructions;
        let interpreted = after.interpreted_instructions - before.interpreted_instructions;
        report.counted_work_max = report.counted_work_max.max(native + interpreted);
        report.fuel_debit_max = report
            .fuel_debit_max
            .max(i64::from(fuel) - i64::from(remaining));
        if native > 0 && report.first_native_ns.is_none() {
            report.first_native_ns = Some(start.elapsed().as_nanos());
        }
        let done = done?;
        #[cfg(feature = "async")]
        if let Some(future) = lua.enter(|ctx| ctx.fetch(&executor).take_pending_future(&ctx)) {
            assert_eq!(case, "foreign_await");
            segment(&lua, &mut report, &mut previous_native, mode);
            report.suspensions += 1;
            foreign::drive(&mut lua, &mut report, future)?;
            continue;
        }
        if !done {
            continue;
        }
        segment(&lua, &mut report, &mut previous_native, mode);
        let value = lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<i64>(ctx)?)?;
        report.boundary_values.push(value);
        let executor_mode = lua.enter(|ctx| ctx.fetch(&executor).mode());
        if executor_mode == ExecutorMode::Suspended {
            assert_eq!(case, "coroutine_resume");
            report.suspensions += 1;
            assert!(report.suspensions <= 2);
            assert_eq!(value, 20100 * report.suspensions as i64);
            collect(&mut lua, &mut report);
            lua.enter(|ctx| ctx.fetch(&executor).resume(ctx, ()))?;
        } else {
            assert_eq!(executor_mode, ExecutorMode::Stopped);
            assert_eq!(
                value,
                if case == "coroutine_resume" {
                    60300
                } else {
                    60384
                }
            );
            break;
        }
    }
    report.elapsed_ns = start.elapsed().as_nanos();
    report.stats = lua.jit_stats();
    assert_eq!(report.suspensions, 2);
    assert_eq!(report.native_segments.len(), 3);
    assert_eq!(report.stats.compilation_failures, 0);
    if mode == Mode::Off {
        assert_eq!(report.stats.native_entries, 0);
        assert_eq!(report.stats.compilation_requests, 0);
        assert_eq!(report.stats.code_bytes, 0);
        assert!(report.first_native_ns.is_none());
    } else {
        assert!(report.first_native_ns.is_some());
        assert!(report.stats.native_entries > 0);
    }
    Ok(report)
}

#[cfg(feature = "async")]
mod foreign {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Poll, Wake, Waker},
    };

    use super::*;

    struct Delayed {
        pending: usize,
    }

    impl Future for Delayed {
        type Output = i64;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<i64> {
            if self.pending == 0 {
                Poll::Ready(42)
            } else {
                self.pending -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn install(lua: &mut Lua) {
        lua.enter(|ctx| {
            ctx.set_global(
                "host_wait",
                luna::Callback::from_fn(&ctx, |ctx, _, _| {
                    Ok(luna::CallbackReturn::Sequence(luna::async_sequence(
                        &ctx,
                        |_, mut sequence| async move {
                            let value = sequence.await_future(Delayed { pending: 3 }).await;
                            sequence.enter(|ctx, _, _, mut stack| stack.replace(ctx, value));
                            Ok(luna::SequenceReturn::Return)
                        },
                    )))
                }),
            );
        });
    }

    pub(super) fn drive(
        lua: &mut Lua,
        report: &mut Report,
        future: luna::PendingFuture,
    ) -> Result<()> {
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        let before = lua.jit_stats();
        let mut pending = 0;
        for _ in 0..4 {
            let start = Instant::now();
            let polled = future.as_mut().poll(&mut context);
            let elapsed = start.elapsed().as_nanos();
            report.poll_ns += elapsed;
            report.max_poll_ns = report.max_poll_ns.max(elapsed);
            let after = lua.jit_stats();
            assert_eq!(before.native_instructions, after.native_instructions);
            assert_eq!(
                before.interpreted_instructions,
                after.interpreted_instructions
            );
            assert_eq!(before.installed_regions, after.installed_regions);
            match polled {
                Poll::Pending => {
                    pending += 1;
                    report.pending_polls += 1;
                    collect(lua, report);
                }
                Poll::Ready(()) => {
                    assert_eq!(pending, 3);
                    let wakes = counter.0.load(Ordering::Relaxed);
                    assert_eq!(wakes, 3);
                    report.wakes += wakes;
                    report.ready_polls += 1;
                    return Ok(());
                }
            }
        }
        Err("foreign future did not become ready within its poll cap".into())
    }
}
