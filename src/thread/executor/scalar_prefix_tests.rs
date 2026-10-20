use super::activation_tests::run;
use crate::thread::vm::scalar_prefix::scoped;

#[test]
fn scalar_prefix_matches_frames_fuel_errors_and_gc_schedules() {
    let scripts = [
        "local n=0 local function f(x) n=n+x end for i=1,12 do f(i) end return n",
        "local function make() local n=0 return function(x) n=n+x end,function() return n end end local f,read=make() for i=1,7 do f(i) end return read()",
        "local n=0x7fffffffffffffff local function f(x) n=n+x end f(1) f(-1) return n==0x7fffffffffffffff and 1 or 0",
        "local n=0.5 local function f(x) n=n+x end f(1.5) return n==2 and 1 or 0",
        "local n='4' local function f(x) n=n+x end f('2') return n==6 and 1 or 0",
        "local n={} local function f(x) n=n+x end local ok=pcall(f,1) return ok and 0 or 1",
        "local mt={__add=function(a,b) return setmetatable({n=a.n+b.n},getmetatable(a)) end} local n=setmetatable({n=1},mt) local function f(x) n=n+x end f({n=2}) return n.n",
        "local n=0 local function f(x) n=n+x end local co=coroutine.create(function() for i=1,4 do f(i) coroutine.yield() end end) for i=1,5 do assert(coroutine.resume(co)) end return n",
        "local n=0 local function f(x) n=n+x end local ticks=0 debug.sethook(function() ticks=ticks+1 end,'',1) f(7) debug.sethook() assert(ticks>0) return n",
        "local n=0 local function f(x) n=n+x end f(1) boundary(0) f(2) return n",
    ];
    let mut hits = 0;
    for (case, source) in scripts.into_iter().enumerate() {
        for budget in [0, 1, 4, 8, 12, 17, 64, 4096] {
            for native in [false, true] {
                let (expected, unused) = scoped(false, || run(source, budget, native));
                let (actual, count) = scoped(true, || run(source, budget, native));
                assert_eq!(unused, 0);
                assert_eq!(
                    expected, actual,
                    "case={case} budget={budget} native={native}"
                );
                assert!(actual.1.is_ok(), "case={case} {:?}", actual.1);
                hits += count;
            }
        }
    }
    assert!(hits > 0);
}

#[test]
fn interpreted_scalar_prefix_matches_small_miri_schedules() {
    let scripts = [
        "local n=0 local function f(x) n=n+x end f(3) f(4) return n",
        "local function make() local n=2 return function(x) n=n+x end,function() return n end end local f,r=make() f(3) return r()",
        "local n=0.5 local function f(x) n=n+x end f(1.5) return n==2 and 1 or 0",
        "local n={} local function f(x) n=n+x end local ok=pcall(f,1) return ok and 0 or 1",
    ];
    let mut hits = 0;
    for source in scripts {
        for budget in [0, 1, 17, 4096] {
            let (expected, _) = scoped(false, || run(source, budget, false));
            let (actual, count) = scoped(true, || run(source, budget, false));
            assert_eq!(expected, actual, "budget={budget} {source}");
            assert!(actual.1.is_ok());
            hits += count;
        }
    }
    assert!(hits > 0);
}

#[cfg(feature = "jit")]
#[test]
fn scalar_prefix_preserves_all_stats_and_callback_mode_changes() {
    use crate::{Callback, CallbackReturn, Closure, Executor, Fuel, JitConfig, JitMode, Lua};
    use std::{cell::RefCell, rc::Rc};

    let drive = |budget, transitions| {
        let mut lua = Lua::core();
        let supported = !cfg!(miri) && lua.jit_capabilities().supported_target;
        lua.set_jit_config(JitConfig {
            mode: JitMode::Off,
            hot_threshold: u32::MAX,
            ..Default::default()
        })
        .unwrap();
        let observations = Rc::new(RefCell::new(Vec::new()));
        let callback_observations = observations.clone();
        let executor = lua.enter(|ctx| {
            ctx.set_global("sample", Callback::from_fn(&ctx, move |ctx, mut execution, mut stack| {
                let enable = stack.consume::<bool>(ctx)? && transitions && supported;
                let frame = execution.upper_lua_frame().unwrap();
                callback_observations.borrow_mut().push((frame.pc, frame.base,
                    execution.fuel().remaining(), ctx.jit().0.borrow().stats));
                let mut manager = ctx.jit().0.borrow_mut();
                let mut config = manager.config.clone();
                config.mode = if enable { JitMode::Auto } else { JitMode::Off };
                manager.configure(config);
                Ok(CallbackReturn::Return)
            }));
            let source = b"local n=0 local function f(x) n=n+x end for i=1,4 do f(i) end sample(true) for i=1,4 do f(i) end sample(false) for i=1,4 do f(i) end return n";
            let closure = Closure::load(ctx, None, source).unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        let mut trace = Vec::new();
        for _ in 0..10000 {
            let mut fuel = Fuel::with(budget);
            let done = lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel).unwrap());
            trace.push((
                done,
                fuel.remaining(),
                fuel.is_interrupted(),
                lua.jit_stats(),
            ));
            lua.gc_collect();
            if done {
                let result = lua.enter(|ctx| {
                    ctx.fetch(&executor)
                        .take_result::<i64>(ctx)
                        .unwrap()
                        .unwrap()
                });
                assert_eq!(result, 30);
                let observations = observations.borrow().clone();
                return (trace, result, observations);
            }
        }
        panic!("scalar prefix stats fixture did not finish");
    };
    let mut hits = 0;
    for budget in [0, 1, 17, 4096] {
        for transitions in [false, true] {
            let (expected, unused) = scoped(false, || drive(budget, transitions));
            let (actual, count) = scoped(true, || drive(budget, transitions));
            assert_eq!(unused, 0);
            assert_eq!(
                expected, actual,
                "budget={budget} transitions={transitions}"
            );
            hits += count;
        }
    }
    assert!(hits > 0);
}
