use super::activation_tests::run;
use crate::thread::vm::static_off::scoped;

#[test]
#[cfg(not(miri))]
fn static_off_matches_dynamic_dispatch_at_every_executor_boundary() {
    let scripts = [
        "local n=0 for i=1,12 do n=n+i end return n",
        "local n=0 local function f(x) n=n+x end for i=1,12 do f(1) end return n",
        "local function f(...) return ... end local function g(...) return f(...) end return g(17)",
        "local a={1,2,3} local n=0 for i=1,3 do n=n+a[i] end return n",
        "local t=setmetatable({n=2},{__add=function(a,b) return a.n+b.n end}) return t+t",
        "local ok=pcall(function() return 1+{} end) return ok and 0 or 1",
        "local n=0 do local x <close> = setmetatable({}, {__close=function() n=n+1 end}) end return n",
        "local co=coroutine.create(function() coroutine.yield(3) return 7 end) local a,b=coroutine.resume(co) assert(a and b==3) local c,d=coroutine.resume(co) assert(c) return d",
        "local n=0 for i=1,4 do n=boundary(n) end return n",
        "local n=0 _ENV.read=function() return n end for i=1,4 do n=nested_boundary() end return n",
        "local ticks=0 debug.sethook(function() ticks=ticks+1 end,'',1) local n=0 for i=1,4 do n=n+i end debug.sethook() return n",
        "local function f() error('static-off-error') end return f()",
    ];
    for (case, source) in scripts.into_iter().enumerate() {
        for budget in [0, 1, 17, 4096] {
            let (expected, ordinary) = scoped(true, || run(source, budget, false));
            let (actual, selected) = scoped(false, || run(source, budget, false));
            assert_eq!(expected, actual, "case={case} budget={budget}");
            assert!(ordinary.0 > 0 && ordinary.1 == 0);
            assert!(selected.0 == 0 && selected.1 > 0);
            assert_eq!(ordinary.0, selected.1);
        }
    }
}

#[test]
#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn static_off_selector_does_not_restrict_auto_native_dispatch() {
    let source = "local n=0 for i=1,128 do n=n+i end return n";
    for budget in [0, 1, 17, 4096] {
        let (expected, first) = scoped(true, || run(source, budget, true));
        let (actual, second) = scoped(false, || run(source, budget, true));
        assert_eq!(expected, actual);
        assert_eq!(first, (0, 0));
        assert_eq!(second, (0, 0));
    }
}

#[test]
#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn static_off_reselects_after_callback_mode_changes_and_cache_retirement() {
    use crate::{Callback, CallbackReturn, Closure, Executor, Fuel, JitConfig, JitMode, Lua};
    use std::{cell::RefCell, rc::Rc};

    let run = || {
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: u32::MAX,
            ..Default::default()
        })
        .unwrap();
        let observed = Rc::new(RefCell::new(Vec::new()));
        let callback_observed = observed.clone();
        let executor = lua.enter(|ctx| {
            ctx.set_global(
                "flip",
                Callback::from_fn(&ctx, move |ctx, mut execution, mut stack| {
                    let enabled: bool = stack.consume(ctx)?;
                    let frame = execution.upper_lua_frame().unwrap();
                    callback_observed.borrow_mut().push((
                        enabled,
                        ctx.jit().active(),
                        frame.pc,
                        frame.base,
                        execution.fuel().remaining(),
                    ));
                    let mut manager = ctx.jit().0.borrow_mut();
                    let mut config = manager.config.clone();
                    config.mode = if enabled { JitMode::Auto } else { JitMode::Off };
                    manager.configure(config);
                    drop(manager);
                    Ok(CallbackReturn::Return)
                }),
            );
            let closure = Closure::load(ctx, Some("static-off-mode"), b"local n=0 for i=1,32 do n=n+i end flip(false) n=n+1 flip(true) n=n+1 flip(false) n=n+1 return n").unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        while lua.prepare_jit().unwrap() != 0 {}
        let mut trace = Vec::new();
        for _ in 0..1000 {
            let mut fuel = Fuel::with(17);
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
                assert_eq!(result, 531);
                assert!(lua.jit_stats().native_entries > 0);
                let observations = observed.borrow().clone();
                assert_eq!(observations.len(), 3);
                assert_eq!(
                    observations.iter().map(|v| v.1).collect::<Vec<_>>(),
                    [true, false, true]
                );
                return (trace, result, observations);
            }
        }
        panic!("mode transition did not finish");
    };
    let (expected, reference) = scoped(true, run);
    let (actual, selected) = scoped(false, run);
    assert_eq!(expected, actual);
    assert!(reference.0 > 0 && reference.1 == 0);
    assert!(selected.0 == 0 && selected.1 > 0);
    assert_eq!(reference.0, selected.1);
}
