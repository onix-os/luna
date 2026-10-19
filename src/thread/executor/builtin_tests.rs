use super::activation_tests::run;
use crate::thread::thread::builtin::scoped;

#[test]
#[cfg(not(miri))]
fn builtin_fusion_matches_every_step_and_collects_between_steps() {
    let scripts = [
        "local t={} local m={} local r=setmetatable(t,m) assert(r==t and getmetatable(t)==m) return 1",
        "local m={} local function f(t) return setmetatable(t,m) end local t={} assert(f(t)==t and getmetatable(t)==m) return 2",
        "local m={__metatable=false} local t=setmetatable({},m) local ok=pcall(setmetatable,t,{}) assert(not ok) return 3",
        "local t=setmetatable({}, {__mode='v'}) local m={} setmetatable(t,m) assert(getmetatable(t)==m) return 4",
        "local t=setmetatable({}, {__gc=function() end}) setmetatable(t,nil) assert(getmetatable(t)==nil) return 5",
        "local real=setmetatable setmetatable=function(t,m) t.called=true return real(t,m) end local t=setmetatable({},{}) assert(t.called) return 6",
        "local real=setmetatable setmetatable=nil local t={} local function f() return real(t,{}) end assert(f()==t) return 7",
        "local mt={} mt.__add=function(a,b) return setmetatable({n=a.n+b.n},mt) end local sum=setmetatable({n=0},mt) local one=setmetatable({n=1},mt) for i=1,20 do sum=sum+one end return sum.n",
        "local co=coroutine.create(function() local t=setmetatable({},{}) coroutine.yield(t) return setmetatable(t,{}) end) local ok,t=coroutine.resume(co) assert(ok) local ok,u=coroutine.resume(co) assert(ok and u==t) return 9",
        "local t={} local function f() local x=t local function g() return x end _G.g=g return setmetatable(x,{}) end assert(f()==g()) return 10",
        "local m={} local t=setmetatable({},{__close=function() setmetatable(m,{}) end}) do local x <close> = t end return 11",
        "local hits=0 debug.sethook(function() hits=hits+1 if hits>=3 then debug.sethook() end end,'',1) local t=setmetatable({},{}) assert(hits>=3) return 12",
        "local t={} boundary(1) local function f() return setmetatable(t,{}) end assert(f()==t) return 13",
        "local ok=pcall(setmetatable,1,{}) assert(not ok) local ok=pcall(setmetatable,{},1) assert(not ok) return 14",
    ];
    let mut fused = 0;
    for (case, source) in scripts.into_iter().enumerate() {
        for budget in [0, 1, 8, 16, 24, 64, 4096] {
            for native in [false, true] {
                eprintln!("builtin_case={case} budget={budget} native={native} phase=control");
                let (expected, unused) = scoped(false, || run(source, budget, native));
                eprintln!("builtin_case={case} budget={budget} native={native} phase=candidate");
                let (actual, hits) = scoped(true, || run(source, budget, native));
                assert_eq!(unused, 0);
                assert_eq!(
                    expected, actual,
                    "budget={budget}, native={native}, {source}"
                );
                assert!(actual.1.is_ok(), "{source}: {:?}", actual.1);
                if !native {
                    assert_eq!(hits, 0);
                }
                if native && budget == 4096 && case == 7 {
                    assert!(hits >= 20, "metamethod tail calls were not fused: {hits}");
                }
                eprintln!("builtin_case={case} budget={budget} native={native} fused={hits}");
                fused += hits;
            }
        }
    }
    assert!(fused > 0, "builtin fusion never executed");
    eprintln!("builtin_schedules=196 fused_calls={fused}");
}

#[test]
fn builtin_persistent_hook_zero_fuel_trace_matches_through_completion() {
    use super::*;
    use crate::{JitConfig, JitMode, Lua, Value};
    let run = |enabled, native| {
        scoped(enabled, || {
            let mut lua = Lua::core();
            lua.load_debug();
            lua.set_jit_config(JitConfig {
                mode: if native { JitMode::Auto } else { JitMode::Off },
                hot_threshold: u32::MAX,
                ..Default::default()
            })
            .unwrap();
            let executor = lua.enter(|ctx| {
                let closure = Closure::load(ctx, Some("hook-prefix"), b"debug.sethook(function() end,'',1) local t=setmetatable({},{}) debug.sethook() return 12").unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            });
            if native {
                while lua.prepare_jit().unwrap() != 0 {}
            }
            let mut trace = Vec::new();
            for _ in 0..64 {
                trace.push(lua.enter(|ctx| {
                    let executor = ctx.fetch(&executor);
                    let mut fuel = Fuel::empty();
                    let done = executor.step(ctx, &mut fuel).unwrap();
                    let state = executor.0.borrow();
                    let states: Vec<_> = state.thread_stack.iter().map(|thread| {
                        let state = thread.into_inner().borrow();
                        let frames: Vec<_> = state.frames.iter().map(|frame| match frame {
                            Frame::Lua { bottom, base, stack_size, pc, is_variable, expected_return, .. } => format!("lua:{bottom}:{base}:{stack_size}:{pc}:{is_variable}:{expected_return:?}"),
                            Frame::Callback { bottom, .. } => format!("callback:{bottom}"),
                            frame => format!("{:?}", std::mem::discriminant(frame)),
                        }).collect();
                        let values: Vec<_> = state.stack.borrow().iter().map(|value| match value {
                            Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::String(_) => format!("{value:?}"),
                            Value::Number(number) => format!("number:{:016x}", number.to_bits()),
                            value => value.type_name().to_owned(),
                        }).collect();
                        (frames, values, state.running)
                    }).collect();
                    (done, executor.mode(), fuel.remaining(), fuel.is_interrupted(), states, ctx.jit().0.borrow().stats.total_dispatches)
                }));
                lua.gc_collect();
                if trace.last().unwrap().0 {
                    break;
                }
            }
            assert!(
                trace.last().unwrap().0,
                "hooked program did not finish in 64 steps"
            );
            assert_eq!(
                lua.enter(|ctx| ctx
                    .fetch(&executor)
                    .take_result::<i64>(ctx)
                    .unwrap()
                    .unwrap()),
                12
            );
            trace
        })
    };
    for native in [false, true] {
        let (control, _) = run(false, native);
        let (candidate, hits) = run(true, native);
        assert_eq!(control, candidate);
        assert_eq!(hits, 0);
        assert!(candidate.last().unwrap().0);
    }
}
