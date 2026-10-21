use std::cell::Cell;

use super::*;
use crate::thread::VMError;

thread_local! {
    static SELECTOR: Cell<(bool, usize, usize)> = const { Cell::new((false, 0, 0)) };
}

fn scoped<T>(enabled: bool, run: impl FnOnce() -> T) -> (T, usize, usize) {
    struct Restore((bool, usize, usize));
    impl Drop for Restore {
        fn drop(&mut self) {
            SELECTOR.set(self.0);
        }
    }
    let _restore = Restore(SELECTOR.replace((enabled, 0, 0)));
    let result = run();
    let (_, scopes, slices) = SELECTOR.get();
    (result, scopes, slices)
}

pub(super) fn after<'gc>(
    ctx: Context<'gc>,
    state: &mut ThreadState<'gc>,
    storage: Gc<'gc, RefLock<StackVec<'gc>>>,
    fuel: &mut Fuel,
    result: Result<u32, VMError>,
    step_charged: &mut bool,
) -> Result<u32, VMError> {
    if !SELECTOR.get().0
        || *step_charged
        || result.is_err()
        || state.mode() != ThreadMode::Normal
        || !matches!(state.frames.last(), Some(Frame::Lua { .. }))
        || ctx.jit().active()
    {
        return result;
    }
    fuel.consume(result.unwrap().try_into().unwrap());
    fuel.consume(Executor::FUEL_PER_STEP);
    *step_charged = true;
    if !fuel.should_continue() {
        return Ok(0);
    }
    SELECTOR.set({
        let (enabled, scopes, slices) = SELECTOR.get();
        (enabled, scopes + 1, slices)
    });
    let mut stack = storage.borrow_mut(&ctx);
    for index in 0..63 {
        SELECTOR.set({
            let (enabled, scopes, slices) = SELECTOR.get();
            (enabled, scopes, slices + 1)
        });
        let result = run_vm(
            ctx,
            LuaFrame {
                #[cfg(all(
                    not(miri),
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ))]
                pair_handoff: None,
                state: &mut *state,
                stack: &mut stack,
                fuel: &mut *fuel,
            },
            Executor::VM_GRANULARITY,
        )
        .map(|instructions| fuel.consume(instructions.try_into().unwrap()));
        fuel.consume(Executor::FUEL_PER_STEP);
        result?;
        if index == 62
            || !fuel.should_continue()
            || state.mode() != ThreadMode::Normal
            || !matches!(state.frames.last(), Some(Frame::Lua { .. }))
        {
            return Ok(0);
        }
    }
    unreachable!()
}

#[test]
fn selector_restores_after_nested_scopes_and_panics() {
    assert_eq!(SELECTOR.get(), (false, 0, 0));
    scoped(true, || {
        assert!(SELECTOR.get().0);
        scoped(false, || assert!(!SELECTOR.get().0));
        assert!(SELECTOR.get().0);
        let panic = std::panic::catch_unwind(|| {
            scoped(false, || std::panic::panic_any(71_u64));
        });
        assert_eq!(*panic.unwrap_err().downcast::<u64>().unwrap(), 71);
        assert!(SELECTOR.get().0);
    });
    assert_eq!(SELECTOR.get(), (false, 0, 0));
}

#[test]
fn interpreted_continuations_preserve_complete_executor_slices() {
    let mut selected = 0;
    for source in [
        "local n=0 local function f(x) n=n+x end for i=1,12 do f(1) end return n",
        "local function f(...) return ... end local function g(...) return f(...) end local n=0 for i=1,12 do n=g(n+1) end return n",
        "local n=0 for i=1,12 do local ok=pcall(function() return i.x end) if not ok then n=n+1 end end return n",
        "local function f() return 1+{} end return f()",
        "local n=0 local function f() local x <close> = setmetatable({}, {__close=function() n=n+1 end}) end for i=1,12 do f() end return n",
        "local co=coroutine.create(function() for i=1,12 do coroutine.yield(i) end end) local n=0 for i=1,12 do local ok,x=coroutine.resume(co) assert(ok) n=x end return n",
        "local function f(x) return boundary(x) end local n=0 for i=1,12 do n=f(n) end return n",
        "local n=0 local function read() return n end _ENV.read=read local function f() n=nested_boundary() end for i=1,12 do f() end return n",
        "local ticks=0 debug.sethook(function() ticks=ticks+1 end,'',1) local n=0 local function f(x) return x+1 end for i=1,12 do n=f(n) end debug.sethook() return n",
    ] {
        for budget in [0, 1, 17, 4096] {
            for native in [false, true] {
                if native && (cfg!(miri) || !crate::Lua::empty().jit_capabilities().supported_target) {
                    continue;
                }
                let run = || activation_tests::run_mode_scoped(source, budget, native, false, None, false, 0);
                let (reference, scopes, slices) = scoped(false, run);
                assert_eq!((scopes, slices), (0, 0));
                let (candidate, scopes, slices) = scoped(true, run);
                assert_eq!(reference, candidate, "{source}, {budget}, {native}");
                if native {
                    assert_eq!((scopes, slices), (0, 0));
                }
                assert!(scopes <= slices && slices <= scopes * 63);
                selected += slices;
            }
        }
    }
    assert!(selected > 0, "no continuation was exercised");
}
