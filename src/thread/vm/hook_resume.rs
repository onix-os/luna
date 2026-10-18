use crate::{
    thread::thread::{Frame, LuaFrame, ThreadState},
    Callback, CallbackReturn, Closure, Context, Fuel, Lua, Thread,
};

fn run<'gc>(
    ctx: Context<'gc>,
    state: &mut ThreadState<'gc>,
    fuel: &mut Fuel,
    budget: u32,
) -> Result<u32, super::VMError> {
    let storage = state.stack;
    let stack = storage.borrow_mut(&ctx);
    #[cfg(feature = "jit")]
    let mut stack = stack;
    super::run_vm(
        ctx,
        LuaFrame {
            #[cfg(feature = "jit")]
            callback_scratch: None,
            #[cfg(all(
                feature = "jit",
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            pair_handoff: None,
            state,
            #[cfg(feature = "jit")]
            stack: &mut stack,
            #[cfg(not(feature = "jit"))]
            stack,
            fuel,
        },
        budget,
    )
}

#[test]
fn pending_marker_survives_zero_budget_and_skips_exactly_one_hook() {
    for hooked in [false, true] {
        for remaining in [0, 7] {
            for interrupted in [false, true] {
                Lua::empty().enter(|ctx| {
                    if hooked {
                        let hook = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
                        ctx.set_debug_hook(hook.into(), false, 1);
                    }
                    let closure =
                        Closure::load(ctx, None, b"local a=1; local b=2; return a+b").unwrap();
                    assert!(closure.prototype().opcodes.len() >= 3);
                    let thread = Thread::new(ctx);
                    thread.start(ctx, closure.into(), ()).unwrap();
                    let mut state = thread.into_inner().borrow_mut(&ctx);
                    state.frames.push(Frame::HookResume);
                    let capacity = state.frames.capacity();
                    let mut fuel = Fuel::with(remaining);
                    if interrupted {
                        fuel.interrupt();
                    }
                    for _ in 0..2 {
                        assert_eq!(run(ctx, &mut state, &mut fuel, 0).unwrap(), 0);
                        assert_eq!(state.frames.len(), 2);
                        assert!(matches!(state.frames.last(), Some(Frame::HookResume)));
                        assert!(matches!(state.frames[0], Frame::Lua { pc: 0, .. }));
                        assert_eq!(fuel.remaining(), remaining);
                        assert_eq!(fuel.is_interrupted(), interrupted);
                    }
                    assert_eq!(run(ctx, &mut state, &mut fuel, 1).unwrap(), 1);
                    assert_eq!(state.frames.len(), 1);
                    assert!(matches!(state.frames[0], Frame::Lua { pc: 1, .. }));
                    assert_eq!(state.frames.capacity(), capacity);
                    assert_eq!(fuel.remaining(), remaining);
                    assert_eq!(fuel.is_interrupted(), interrupted);
                    if hooked {
                        assert_eq!(run(ctx, &mut state, &mut fuel, 1).unwrap(), 0);
                        assert!(matches!(state.frames.last(), Some(Frame::Callback { .. })));
                        assert!(matches!(state.frames[0], Frame::Lua { pc: 1, .. }));
                    }
                });
            }
        }
    }
}

#[test]
fn ordinary_entry_does_not_suppress_a_pending_count_hook() {
    Lua::empty().enter(|ctx| {
        let hook = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
        ctx.set_debug_hook(hook.into(), false, 1);
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        let thread = Thread::new(ctx);
        thread.start(ctx, closure.into(), ()).unwrap();
        let mut state = thread.into_inner().borrow_mut(&ctx);
        assert_eq!(run(ctx, &mut state, &mut Fuel::with(64), 1).unwrap(), 0);
        assert!(matches!(state.frames.last(), Some(Frame::Callback { .. })));
        assert!(matches!(state.frames[0], Frame::Lua { pc: 0, .. }));
    });
}
