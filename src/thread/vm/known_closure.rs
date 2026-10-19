use std::cell::Cell;

thread_local! {
    static STATE: Cell<(bool, usize, usize)> = const { Cell::new((false, 0, 0)) };
}

pub(in crate::thread) fn select() -> bool {
    STATE.with(|state| {
        let (reference, known, resolved) = state.get();
        state.set((
            reference,
            known + usize::from(!reference),
            resolved + usize::from(reference),
        ));
        !reference
    })
}

pub(in crate::thread) fn scoped<R>(
    reference: bool,
    run: impl FnOnce() -> R,
) -> (R, (usize, usize)) {
    struct Restore((bool, usize, usize));
    impl Drop for Restore {
        fn drop(&mut self) {
            STATE.with(|state| state.set(self.0));
        }
    }
    let restore = Restore(STATE.with(|state| state.replace((reference, 0, 0))));
    let result = run();
    let counts = STATE.with(|state| (state.get().1, state.get().2));
    drop(restore);
    (result, counts)
}

#[test]
fn selection_restores_nested_and_panicking_scopes() {
    let (_, counts) = scoped(false, || {
        assert!(select());
        let (_, counts) = scoped(true, || assert!(!select()));
        assert_eq!(counts, (0, 1));
        let panic = std::panic::catch_unwind(|| scoped(true, || std::panic::panic_any(19_u64)));
        assert_eq!(*panic.unwrap_err().downcast::<u64>().unwrap(), 19);
        assert!(select());
    });
    assert_eq!(counts, (2, 0));
}

#[test]
fn zero_budget_preserves_pending_hook_with_manager_borrowed() {
    use crate::{
        thread::thread::{Frame, LuaFrame},
        Closure, Fuel, Lua, Thread,
    };
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        let different = Closure::load(ctx, None, b"return 17").unwrap();
        let thread = Thread::new(ctx);
        thread.start(ctx, closure.into(), ()).unwrap();
        let mut state = thread.into_inner().borrow_mut(&ctx);
        state.frames.push(Frame::HookResume);
        let storage = state.stack;
        let mut stack = storage.borrow_mut(&ctx);
        let manager = ctx.jit().0.borrow_mut();
        let mut fuel = Fuel::with(7);
        fuel.interrupt();
        let frame = LuaFrame {
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            pair_handoff: None,
            state: &mut state,
            stack: &mut stack,
            fuel: &mut fuel,
        };
        assert_eq!(super::run_vm_known(ctx, frame, 0, different).unwrap(), 0);
        assert_eq!(state.frames.len(), 2);
        assert!(matches!(state.frames.last(), Some(Frame::HookResume)));
        assert!(matches!(state.frames[0], Frame::Lua { pc: 0, .. }));
        assert_eq!((fuel.remaining(), fuel.is_interrupted()), (7, true));
        drop(manager);
    });
}

#[test]
#[cfg(debug_assertions)]
fn stale_nonzero_closure_is_rejected_before_execution() {
    use crate::{thread::thread::LuaFrame, Closure, Fuel, Lua, Thread};
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        let different = Closure::load(ctx, None, b"return 17").unwrap();
        let thread = Thread::new(ctx);
        thread.start(ctx, closure.into(), ()).unwrap();
        let mut state = thread.into_inner().borrow_mut(&ctx);
        let storage = state.stack;
        let mut stack = storage.borrow_mut(&ctx);
        let mut fuel = Fuel::with(11);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::run_vm_known(
                ctx,
                LuaFrame {
                    #[cfg(all(
                        not(miri),
                        target_os = "linux",
                        any(target_arch = "x86_64", target_arch = "aarch64")
                    ))]
                    pair_handoff: None,
                    state: &mut state,
                    stack: &mut stack,
                    fuel: &mut fuel,
                },
                1,
                different,
            )
        }));
        assert!(result.is_err());
        assert_eq!(fuel.remaining(), 11);
        assert_eq!(ctx.jit().0.borrow().stats.total_dispatches, 0);
    });
}
