use super::*;
use crate::{Closure, Executor, Fuel, Lua, StashedExecutor};

fn state(mode: JitMode, payload: bool) -> Lua {
    let mut lua = Lua::core();
    lua.load_debug();
    lua.set_jit_config(JitConfig {
        mode,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let mut manager = ctx.jit().0.borrow_mut();
        manager.memory_failure = if payload {
            backend::Failure::RequirePayload
        } else {
            backend::Failure::None
        };
        manager.array_kernels = true;
    });
    lua
}

fn source(lua: &mut Lua, script: &str) -> StashedExecutor {
    lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("payload-runtime"), script.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })
    .unwrap()
}

fn counters(s: JitStats) -> [u64; 25] {
    [
        s.total_dispatches,
        s.interpreted_slices,
        s.interpreted_instructions,
        s.native_entries,
        s.native_pair_calls,
        s.native_pair_returns,
        s.native_pair_cache_hits,
        s.native_region_entries,
        s.native_region_fragments,
        s.code_lookups,
        s.code_leases,
        s.native_instructions,
        s.guard_exits,
        s.native_interpreter_exits,
        s.native_budget_exits,
        s.native_panic_exits,
        s.helper_calls,
        s.helper_instructions,
        s.helper_declines,
        s.native_table_reads,
        s.native_table_writes,
        s.native_upvalue_reads,
        s.native_upvalue_writes,
        s.native_allocations,
        s.hook_exits,
    ]
}

#[test]
fn payload_runtime_preserves_steps_callbacks_upvalues_gc_and_array_kernels() {
    for (script, expected, arrays) in [
        ("local s=0 for i=1,120 do s=s+i end return s", 7260, false),
        ("local s=0.0 for i=1,120 do s=s+0.5 end return s", 60, false),
        ("local n=0 local function f(x) n=n+x return n end for i=1,120 do f(i) end return n", 7260, false),
        ("local n=0 for i=1,120 do n=n+tonumber('1') end return n", 120, false),
        ("local t={} for i=1,120 do t[i]=i end local s=0 for i=1,120 do s=s+t[i] end return s", 7260, true),
        ("local n=0 for i=1,120 do local ok=pcall(function() local a=1 return a.x end) if not ok then n=n+1 end end return n", 120, false),
        ("local a,b=0,100 local function f(x) a=a+x b=b+1 return a+b end debug.upvaluejoin(f,2,f,1) local result for i=1,10 do result=f(i) end return result", 130, false),
        ("local co=coroutine.create(function() local n=0 local function f(x) n=n+x return n end coroutine.yield(f) return n end) local ok,f=coroutine.resume(co) assert(ok) local result for i=1,100 do result=f(i) end local ok,n=coroutine.resume(co) assert(ok) return result+n", 10100, false),
        ("local n=0 local function f(x) n=n+x return n end for i=1,120 do if i%2==0 then debug.setupvalue(f,1,i) end f(1) end return n", 121, false),
        ("local t={} t.self=t local v=0 for i=1,120 do if i%2==0 then v=t else v=i end end assert(v.self==t) return 120", 120, false),
    ] {
        for initial in [-1, 0, 1, 63, 64, 65, 65536, i32::MAX] {
            let mut reference = state(JitMode::Off, false);
            let mut control = state(JitMode::Auto, false);
            let mut candidate = state(JitMode::Auto, true);
            let reference_executor = source(&mut reference, script);
            let control_executor = source(&mut control, script);
            let candidate_executor = source(&mut candidate, script);
            let step = |lua: &mut Lua, executor: &StashedExecutor, budget, interrupt| {
                lua.enter(|ctx| {
                    let mut fuel = Fuel::with(budget);
                    if interrupt { fuel.interrupt(); }
                    let executor = ctx.fetch(executor);
                    let finished = executor.step(ctx, &mut fuel).unwrap();
                    (finished, executor.mode(), fuel.remaining(), fuel.is_interrupted())
                })
            };
            let mut finished = false;
            for turn in 0..4096 {
                control.prepare_jit().unwrap();
                candidate.prepare_jit().unwrap();
                let budget = if turn == 0 { initial } else { [-1, 0, 1, 63, 64, 65][turn % 6] };
                let interrupt = turn % 7 == 3;
                let reference_step = step(&mut reference, &reference_executor, budget, interrupt);
                let expected_step = step(&mut control, &control_executor, budget, interrupt);
                let actual_step = step(&mut candidate, &candidate_executor, budget, interrupt);
                assert_eq!(actual_step, reference_step, "{script}: initial={initial} turn={turn}");
                assert_eq!(actual_step, expected_step);
                assert_eq!(counters(candidate.jit_stats()), counters(control.jit_stats()), "{script}: initial={initial} turn={turn}");
                assert_eq!(candidate.jit_stats().total_dispatches, reference.jit_stats().total_dispatches);
                candidate.enter(|ctx| assert!(ctx.jit().0.borrow().code.values().all(|cached| cached.code.payload)));
                if actual_step.0 { finished = true; break; }
                if turn % 11 == 0 {
                    reference.gc_collect();
                    control.gc_collect();
                    candidate.gc_collect();
                }
            }
            assert!(finished);
            assert_eq!(reference.execute::<i64>(&reference_executor).unwrap(), expected);
            assert_eq!(control.execute::<i64>(&control_executor).unwrap(), expected);
            assert_eq!(candidate.execute::<i64>(&candidate_executor).unwrap(), expected);
            assert!(candidate.jit_stats().native_instructions > 0);
            let installed = |s: JitStats| (s.compilation_requests, s.compilation_failures, s.installed_regions);
            assert_eq!(installed(candidate.jit_stats()), installed(control.jit_stats()));
            let actual_arrays = candidate.enter(|ctx| ctx.jit().test_array_counts());
            let expected_arrays = control.enter(|ctx| ctx.jit().test_array_counts());
            assert_eq!(actual_arrays, expected_arrays);
            if arrays { assert!(actual_arrays.0 + actual_arrays.1 > 0); }
            candidate.clear_jit_cache();
            assert_eq!(candidate.jit_stats().code_bytes, 0);
        }
    }
}
