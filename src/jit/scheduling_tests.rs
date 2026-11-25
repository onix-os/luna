use std::{sync::mpsc, thread, time::Duration};

use crate::{Closure, Executor, Fuel, Lua, StashedClosure, StashedExecutor};

use super::{JitConfig, JitMode};

struct BlockedCompiler {
    release: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl BlockedCompiler {
    fn start() -> Self {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut lua = Lua::empty();
            lua.set_jit_config(JitConfig {
                mode: JitMode::Auto,
                ..Default::default()
            })
            .unwrap();
            let closure = lua.enter(|ctx| {
                let closure =
                    Closure::load(ctx, None, b"local s=0 for i=1,1000 do s=s+i end return s")
                        .unwrap();
                ctx.jit().0.borrow_mut().before_compile = Some(Box::new(move || {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(Duration::from_secs(30))
                        .expect("compiler release timed out");
                }));
                ctx.stash(closure)
            });
            assert_eq!(lua.prepare_jit().unwrap(), 1);
            let executor =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), 500500);
            assert!(lua.jit_stats().native_instructions > 0);
        });
        let blocked = Self {
            release: Some(release_tx),
            worker: Some(worker),
        };
        entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("compiler did not enter gate");
        blocked
    }

    fn finish(mut self) {
        self.release.take().unwrap().send(()).unwrap();
        self.worker.take().unwrap().join().unwrap();
    }
}

impl Drop for BlockedCompiler {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn state(mode: JitMode) -> (Lua, [(StashedClosure, StashedExecutor); 3]) {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode,
        hot_threshold: 1,
        max_queue_entries: 2,
        ..Default::default()
    })
    .unwrap();
    let jobs = lua.enter(|ctx| {
        std::array::from_fn(|index| {
            let closure = Closure::load(
                ctx,
                None,
                b"local n=... local s=0 for i=1,n do s=s+i end return s",
            )
            .unwrap();
            let executor = Executor::start(ctx, closure.into(), (100 * (index as i64 + 1),));
            (ctx.stash(closure), ctx.stash(executor))
        })
    });
    (lua, jobs)
}

#[test]
fn dropping_state_with_queued_work_reclaims_storage_without_compiling() {
    let host = {
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        let executor = lua.enter(|ctx| {
            ctx.jit().0.borrow_mut().before_compile =
                Some(Box::new(|| panic!("state destruction entered compiler")));
            let closure = Closure::load(ctx, None, b"while true do end").unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        lua.enter(|ctx| {
            assert!(!ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap());
        });
        let stats = lua.jit_stats();
        assert_eq!(stats.queued_requests, 1);
        assert_eq!(stats.compilation_requests, 1);
        assert_eq!(stats.installed_regions, 0);
        assert_eq!((stats.code_bytes, stats.snapshot_bytes), (0, 0));
        assert!(stats.metadata_bytes > 0);
        lua.enter(|ctx| ctx.jit().0.borrow().host.clone())
    };
    assert_eq!(host.current(), 0);
    assert_eq!(
        host.accounted(),
        super::resources::LedgerRef::allocation_bytes()
    );
}

#[test]
fn blocked_compiler_does_not_stall_manual_executors_or_share_queue_state() {
    let blocked = BlockedCompiler::start();
    for budget in [1, 64, 65536] {
        let (mut reference, left) = state(JitMode::Off);
        let (mut candidate, right) = state(JitMode::Auto);
        candidate.enter(|ctx| {
            ctx.jit().0.borrow_mut().before_compile =
                Some(Box::new(|| panic!("manual stepping entered compiler")));
        });
        let mut done = [false; 3];
        for _ in 0..10000 {
            for index in 0..3 {
                if done[index] {
                    continue;
                }
                let step = |lua: &mut Lua, executor: &StashedExecutor| {
                    let mut fuel = Fuel::with(budget);
                    let (finished, mode) = lua.enter(|ctx| {
                        let executor = ctx.fetch(executor);
                        (executor.step(ctx, &mut fuel).unwrap(), executor.mode())
                    });
                    (finished, mode, fuel.remaining())
                };
                let expected = step(&mut reference, &left[index].1);
                let actual = step(&mut candidate, &right[index].1);
                assert_eq!(actual, expected, "budget={budget} executor={index}");
                done[index] = actual.0;
                let stats = candidate.jit_stats();
                assert_eq!(
                    (
                        stats.native_instructions,
                        stats.installed_regions,
                        stats.compilation_failures
                    ),
                    (0, 0, 0)
                );
                assert!(stats.queued_requests <= 2);
                assert_eq!((stats.code_bytes, stats.snapshot_bytes), (0, 0));
            }
            reference.gc_collect();
            candidate.gc_collect();
            if done.iter().all(|done| *done) {
                break;
            }
        }
        assert!(done.iter().all(|done| *done));
        assert_eq!(candidate.jit_stats().queued_requests, 2);
        assert_eq!(candidate.jit_stats().compilation_requests, 2);
        assert_eq!(reference.jit_stats().compilation_requests, 0);
        candidate.enter(|ctx| assert!(ctx.jit().0.borrow_mut().before_compile.take().is_some()));
        for index in 0..3 {
            let n = 100 * (index as i64 + 1);
            let expected = n * (n + 1) / 2;
            for (lua, jobs) in [(&mut reference, &left), (&mut candidate, &right)] {
                assert_eq!(
                    lua.enter(|ctx| ctx
                        .fetch(&jobs[index].1)
                        .take_result::<i64>(ctx)
                        .unwrap()
                        .unwrap()),
                    expected
                );
            }
        }
        assert!(!blocked.worker.as_ref().unwrap().is_finished());
        assert_eq!(candidate.service_jit().unwrap(), 1);
        assert_eq!(candidate.service_jit().unwrap(), 1);
        assert_eq!(candidate.jit_stats().installed_regions, 2);
        for (index, (closure, _)) in right.iter().take(2).enumerate() {
            let n = 100 * (index as i64 + 1);
            let executor = candidate
                .enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), (n,))));
            assert_eq!(
                candidate.execute::<i64>(&executor).unwrap(),
                n * (n + 1) / 2
            );
        }
        assert!(candidate.jit_stats().native_instructions > 0);
    }
    assert!(!blocked.worker.as_ref().unwrap().is_finished());
    blocked.finish();
}
