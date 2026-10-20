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
        (executor, request.compile().unwrap())
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
