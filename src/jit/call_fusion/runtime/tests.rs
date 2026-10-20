use super::*;
use crate::{Executor, Fuel, JitConfig, JitMode, JitStats, Lua};

type Snapshot = (Vec<Vec<String>>, Vec<Vec<String>>);

struct Outcome {
    trace: Vec<(Snapshot, i32)>,
    value: i64,
    stats: JitStats,
    fused: u64,
}

fn run(source: &[u8], mode: JitMode, enabled: bool, fuels: &[i32]) -> Outcome {
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    lua.set_jit_config(JitConfig {
        mode,
        ..Default::default()
    })
    .unwrap();
    let (executor, request) = lua.enter(|ctx| {
        let caller = Closure::load(ctx, Some("fusion-runtime"), source).unwrap();
        let request = enabled.then(|| Request::new(ctx, caller).unwrap().unwrap());
        (ctx.stash(Executor::start(ctx, caller.into(), ())), request)
    });
    if mode != JitMode::Off {
        assert!(lua.prepare_jit().unwrap() > 0);
    }
    let prepared = request.map(|request| request.compile().unwrap());
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    let mut trace = Vec::new();
    let mut execute = || {
        let mut done = false;
        for step in 0..10000 {
            lua.gc_collect();
            let mut fuel = Fuel::with(fuels[step % fuels.len()]);
            done = lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                let done = executor.step(ctx, &mut fuel).unwrap();
                trace.push((executor.test_frame_snapshot(), fuel.remaining()));
                done
            });
            if done {
                break;
            }
        }
        assert!(done, "execution did not finish");
        lua.execute::<i64>(&executor).unwrap()
    };
    let value = if let Some(prepared) = &prepared {
        with_prepared(prepared.clone(), execute)
    } else {
        execute()
    };
    let fused = prepared.as_ref().map_or(0, |p| p.executions());
    let stats = lua.jit_stats();
    Outcome {
        trace,
        value,
        stats,
        fused,
    }
}

#[test]
fn integrated_fusion_matches_interpreter_frames_fuel_and_results_across_gc() {
    for source in [
        b"local sum=0 local function add(v) sum=sum+v end for i=1,80 do add(i) end return sum".as_slice(),
        b"local sum=0 local function add(v) sum=sum+v end for i=1,80 do add(7) end return sum".as_slice(),
        b"local sum=1 local function add(v) sum=sum+v end for i=1,80 do add(sum) end return sum".as_slice(),
        b"local sum=0x7fffffffffffffff local function add(v) sum=sum+v end for i=1,80 do add(i) end return sum".as_slice(),
        b"local sum=0 local function add(v) sum=sum+v end for i=1,8 do for j=1,10 do add(j) end end return sum".as_slice(),
    ] {
        for fuels in [&[1, 1, 256, 31, 0, 64][..], &[10000][..]] {
            let off = run(source, JitMode::Off, false, fuels);
            let ordinary = run(source, JitMode::Auto, false, fuels);
            let candidate = run(source, JitMode::Auto, true, fuels);
            assert_eq!(ordinary.value, off.value);
            assert_eq!(ordinary.trace, off.trace, "ordinary interpreter mismatch");
            assert_eq!(candidate.value, off.value);
            assert_eq!(candidate.trace, off.trace, "fusion interpreter mismatch");
            assert!(candidate.fused > 0, "no fused region ran");
            assert!(candidate.stats.native_instructions >= 5 * candidate.fused);
            assert!(candidate.stats.native_upvalue_reads >= candidate.fused);
            assert!(candidate.stats.native_upvalue_writes >= candidate.fused);
            assert_eq!(candidate.stats.native_panic_exits, 0);
        }
    }
}

#[test]
fn low_fuel_keeps_physical_calls_and_identical_step_traces() {
    let source =
        b"local sum=0 local function add(v) sum=sum+v end for i=1,20 do add(i) end return sum";
    for fuels in [&[0, 1][..], &[16][..], &[22][..]] {
        let ordinary = run(source, JitMode::Auto, false, fuels);
        let candidate = run(source, JitMode::Auto, true, fuels);
        assert_eq!(candidate.value, ordinary.value);
        assert_eq!(candidate.trace, ordinary.trace);
        assert_eq!(candidate.fused, 0);
    }
}

#[test]
fn scoped_leases_restore_after_nested_unwind_and_isolate_runtime_identity() {
    fn setup(lua: &mut Lua) -> (crate::StashedExecutor, Lease) {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        })
        .unwrap();
        let (executor, request) = lua.enter(|ctx| {
            let caller = Closure::load(ctx, None, b"local sum=0 local function add(v) sum=sum+v end for i=1,20 do add(i) end return sum").unwrap();
            (ctx.stash(Executor::start(ctx, caller.into(), ())), Request::new(ctx, caller).unwrap().unwrap())
        });
        lua.prepare_jit().unwrap();
        let prepared = request.compile().unwrap();
        assert_eq!(lua.jit_stats().snapshot_bytes, 0);
        (executor, prepared)
    }
    let mut first = Lua::empty();
    let mut second = Lua::empty();
    let (first_executor, a) = setup(&mut first);
    let (second_executor, b) = setup(&mut second);
    assert_eq!(a.source, b.source);
    with_prepared(a.clone(), || {
        first.enter(|ctx| assert!(current(ctx, Some(a.source)).is_some()));
        second.enter(|ctx| assert!(current(ctx, Some(b.source)).is_none()));
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_prepared(b.clone(), || {
                first.enter(|ctx| assert!(current(ctx, Some(a.source)).is_none()));
                second.enter(|ctx| assert!(current(ctx, Some(b.source)).is_some()));
                panic!("scoped lease unwind");
            });
        }));
        assert!(unwind.is_err());
        first.enter(|ctx| assert!(current(ctx, Some(a.source)).is_some()));
        second.enter(|ctx| assert!(current(ctx, Some(b.source)).is_none()));
    });
    first.enter(|ctx| assert!(current(ctx, Some(a.source)).is_none()));
    second.enter(|ctx| assert!(current(ctx, Some(b.source)).is_none()));
    first.clear_jit_cache();
    second.clear_jit_cache();
    assert!(first.jit_stats().code_bytes > 0);
    assert!(second.jit_stats().code_bytes > 0);
    drop(a);
    drop(b);
    assert_eq!(
        (
            first.jit_stats().code_bytes,
            first.jit_stats().snapshot_bytes
        ),
        (0, 0)
    );
    assert_eq!(
        (
            second.jit_stats().code_bytes,
            second.jit_stats().snapshot_bytes
        ),
        (0, 0)
    );
    drop(first_executor);
    drop(second_executor);
    first.gc_collect();
    second.gc_collect();
}

#[test]
fn compiled_source_identity_is_not_reused_for_other_parents_or_after_reset() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..Default::default()
    })
    .unwrap();
    let (a, b, request) = lua.enter(|ctx| {
        let a = Closure::load(
            ctx,
            None,
            b"local sum=0 local function add(v) sum=sum+v end for i=1,40 do add(i) end return sum",
        )
        .unwrap();
        let b = Closure::load(
            ctx,
            None,
            b"local sum=99 local function add(v) sum=sum+v end for i=1,40 do add(i) end return sum",
        )
        .unwrap();
        (
            ctx.stash(a),
            ctx.stash(b),
            Request::new(ctx, a).unwrap().unwrap(),
        )
    });
    lua.prepare_jit().unwrap();
    let prepared = request.compile().unwrap();
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    lua.enter(|ctx| {
        assert!(prepared.matches(ctx, ctx.fetch(&a)));
        assert!(!prepared.matches(ctx, ctx.fetch(&b)));
    });
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&b).into(), ())));
    assert_eq!(
        with_prepared(prepared.clone(), || lua.execute::<i64>(&executor)).unwrap(),
        919
    );
    assert_eq!(prepared.executions(), 0);
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&a).into(), ())));
    assert_eq!(
        with_prepared(prepared.clone(), || lua.execute::<i64>(&executor)).unwrap(),
        820
    );
    let count = prepared.executions();
    assert!(count > 0);
    lua.enter(|ctx| {
        ctx.jit_registry().borrow_mut(&ctx).reset(ctx);
        ctx.jit_registry()
            .borrow_mut(&ctx)
            .register(ctx, ctx.fetch(&a).prototype());
        assert!(!prepared.matches(ctx, ctx.fetch(&a)));
    });
    lua.prepare_jit().unwrap();
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&a).into(), ())));
    assert_eq!(
        with_prepared(prepared.clone(), || lua.execute::<i64>(&executor)).unwrap(),
        820
    );
    assert_eq!(prepared.executions(), count);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn one_compiled_parent_keeps_interleaved_executor_captures_separate() {
    fn execute(mode: JitMode, enabled: bool) -> (Vec<Vec<(Snapshot, i32)>>, [i64; 2], u64) {
        use allocator_api2::vec::Vec as ArenaVec;
        use ottavino_gc_arena::allocator_api::MetricsAlloc;
        let mut lua = Lua::empty();
        lua.set_gc_pacing(false);
        lua.set_jit_config(JitConfig {
            mode,
            ..Default::default()
        })
        .unwrap();
        let (executors, request) = lua.enter(|ctx| {
            let outer = Closure::load(ctx, None, b"local function run(seed) local sum=seed local function add(v) sum=sum+v end for i=1,100 do add(i) end return sum end return run").unwrap();
            let proto = outer.prototype().prototypes[0];
            assert!(proto.upvalues.is_empty());
            let caller = Closure::from_parts(&ctx, proto, ArenaVec::new_in(MetricsAlloc::new(&ctx)));
            let request = enabled.then(|| Request::new(ctx, caller).unwrap().unwrap());
            ([ctx.stash(Executor::start(ctx, caller.into(), (11,))), ctx.stash(Executor::start(ctx, caller.into(), (97,)))], request)
        });
        if mode == JitMode::Auto {
            lua.prepare_jit().unwrap();
        }
        let prepared = request.map(|r| r.compile().unwrap());
        assert_eq!(lua.jit_stats().snapshot_bytes, 0);
        let mut traces = vec![Vec::new(), Vec::new()];
        let run = || {
            let mut done = [false; 2];
            for _ in 0..1000 {
                lua.gc_collect();
                for index in 0..2 {
                    if done[index] {
                        continue;
                    }
                    let mut fuel = Fuel::with(256);
                    done[index] = lua.enter(|ctx| {
                        let executor = ctx.fetch(&executors[index]);
                        let done = executor.step(ctx, &mut fuel).unwrap();
                        traces[index].push((executor.test_frame_snapshot(), fuel.remaining()));
                        done
                    });
                }
                if done == [true; 2] {
                    break;
                }
            }
            assert_eq!(done, [true; 2]);
            executors.map(|e| lua.execute::<i64>(&e).unwrap())
        };
        let values = if let Some(p) = &prepared {
            with_prepared(p.clone(), run)
        } else {
            run()
        };
        let count = prepared.as_ref().map_or(0, |p| p.executions());
        (traces, values, count)
    }
    let off = execute(JitMode::Off, false);
    let candidate = execute(JitMode::Auto, true);
    assert_eq!(candidate.0, off.0);
    assert_eq!(candidate.1, [5061, 5147]);
    assert_eq!(candidate.1, off.1);
    assert!(candidate.2 > 0);
}
