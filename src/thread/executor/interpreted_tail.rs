#[cfg(test)]
use std::cell::Cell;

use super::*;
use crate::thread::VMError;

#[cfg(test)]
thread_local! {
    static SELECTOR: Cell<(bool, usize, usize, usize)> = const { Cell::new((true, 0, 0, 0)) };
}

#[cfg(test)]
pub(super) fn scoped<T>(enabled: bool, run: impl FnOnce() -> T) -> (T, usize, usize) {
    struct Restore((bool, usize, usize, usize));
    impl Drop for Restore {
        fn drop(&mut self) {
            SELECTOR.set(self.0);
        }
    }
    let _restore = Restore(SELECTOR.replace((enabled, 0, 0, 0)));
    let result = run();
    let (_, scopes, slices, _) = SELECTOR.get();
    (result, scopes, slices)
}

pub(super) fn after<'gc>(
    ctx: Context<'gc>,
    state: &mut ThreadState<'gc>,
    storage: Gc<'gc, RefLock<StackVec<'gc>>>,
    fuel: &mut Fuel,
    instructions: u32,
    step_charged: &mut bool,
) -> Result<u32, VMError> {
    #[cfg(test)]
    if !SELECTOR.get().0 || ctx.jit().active() {
        return Ok(instructions);
    }
    if *step_charged
        || state.mode() != ThreadMode::Normal
        || !matches!(state.frames.last(), Some(Frame::Lua { .. }))
    {
        return Ok(instructions);
    }
    fuel.consume(instructions.try_into().unwrap());
    fuel.consume(Executor::FUEL_PER_STEP);
    *step_charged = true;
    if !fuel.should_continue() {
        return Ok(0);
    }
    #[cfg(test)]
    SELECTOR.set({
        let (enabled, scopes, slices, growths) = SELECTOR.get();
        (enabled, scopes + 1, slices, growths)
    });
    let mut stack = storage.borrow_mut(&ctx);
    for index in 0..63 {
        #[cfg(test)]
        SELECTOR.set({
            let (enabled, scopes, slices, growths) = SELECTOR.get();
            (enabled, scopes, slices + 1, growths)
        });
        #[cfg(test)]
        let capacity = stack.capacity();
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
        #[cfg(test)]
        if stack.capacity() > capacity {
            let (enabled, scopes, slices, growths) = SELECTOR.get();
            SELECTOR.set((enabled, scopes, slices, growths + 1));
        }
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
    let initial = SELECTOR.get();
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
    assert_eq!(SELECTOR.get(), initial);
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

#[test]
fn bounded_continuations_reenter_the_outer_executor() {
    let source = "local n=0 local function f(x) n=n+x end for i=1,80 do f(1) end return n";
    let run = || activation_tests::run_mode_scoped(source, i32::MAX, false, false, None, false, 0);
    let (reference, _, _) = scoped(false, run);
    assert_eq!(reference.1, Ok(80));
    let (candidate, scopes, slices) = scoped(true, run);
    assert_eq!(reference, candidate);
    assert!(scopes >= 2 && slices > 63 && slices <= scopes * 63);
}

#[test]
fn explicit_host_limits_preserve_the_full_usize_range() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        for limit in [1, 2, 64, 65, usize::MAX] {
            ctx.jit().test_activation_host(limit);
            assert_eq!(ctx.jit().scoped_activation_limit(), limit);
        }
        ctx.jit().test_activation_host(0);
        let mut manager = ctx.jit().0.borrow_mut();
        let mut config = manager.config.clone();
        config.mode = crate::JitMode::Off;
        manager.configure(config);
        drop(manager);
        assert!(!ctx.jit().active());
        assert_eq!(ctx.jit().scoped_activation_limit(), 0);
    });
}

#[test]
fn continuation_rebuilds_registers_after_its_borrowed_stack_grows() {
    let locals = (1..=160)
        .map(|index| format!("local x{index}={index} "))
        .collect::<std::string::String>();
    let sum = (1..=160)
        .map(|index| format!("x{index}"))
        .collect::<Vec<_>>()
        .join("+");
    let source = format!("local function wide() {locals} return {sum} end local function outer() return wide()+1 end return outer()");
    let run = || activation_tests::run_mode_scoped(&source, 4096, false, false, None, false, 0);
    let (reference, _, _) = scoped(false, run);
    assert_eq!(reference.1, Ok(12881));
    let ((candidate, growths), scopes, slices) = scoped(true, || {
        let result = run();
        (result, SELECTOR.get().3)
    });
    assert_eq!(reference, candidate);
    assert!(scopes > 0 && slices >= scopes && growths > 0);
}
