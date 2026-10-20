use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::{
    opcode::Operation,
    thread::thread::{Frame, LuaFrame},
    Closure, Fuel, Lua, Thread, Value,
};

#[test]
fn interpreter_panics_unwind_through_the_vm_boundary() {
    #[cfg(feature = "jit")]
    let modes = [crate::JitMode::Off, crate::JitMode::Auto];
    #[cfg(not(feature = "jit"))]
    let modes = [()];
    for mode in modes {
        let mut lua = Lua::empty();
        #[cfg(feature = "jit")]
        lua.set_jit_config(crate::JitConfig {
            mode,
            ..Default::default()
        })
        .unwrap();
        #[cfg(not(feature = "jit"))]
        let _ = mode;
        lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return 7").unwrap();
            assert!(matches!(
                closure.prototype().opcodes[0].decode(),
                Operation::LoadConstant { dest, .. } if dest.0 == 0
            ));
            let thread = Thread::new(ctx).into_inner();
            let mut state = thread.borrow_mut(&ctx);
            let storage = state.stack;
            let mut stack = storage.borrow_mut(&ctx);
            stack.push(Value::Integer(117));
            state.frames.push(Frame::Lua {
                closure,
                bottom: 0,
                base: 1,
                pc: 0,
                is_variable: false,
                stack_size: 0,
                expected_return: None,
            });
            let mut fuel = Fuel::with(19);
            #[cfg(feature = "jit")]
            let before = ctx.jit().0.borrow().stats;
            let result = catch_unwind(AssertUnwindSafe(|| {
                super::run_vm_slice(
                    ctx,
                    LuaFrame {
                        #[cfg(all(
                            feature = "jit",
                            not(miri),
                            target_os = "linux",
                            any(target_arch = "x86_64", target_arch = "aarch64")
                        ))]
                        pair_handoff: None,
                        state: &mut state,
                        #[cfg(feature = "jit")]
                        stack: &mut stack,
                        #[cfg(not(feature = "jit"))]
                        stack,
                        fuel: &mut fuel,
                    },
                    64,
                )
            }));
            let panic = result.unwrap_err();
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap();
            assert!(message.contains("index out of bounds"), "{message}");
            assert!(matches!(state.frames[0], Frame::Lua { pc: 1, .. }));
            assert_eq!(fuel.remaining(), 19);
            #[cfg(feature = "jit")]
            {
                let after = ctx.jit().0.borrow().stats;
                assert_eq!(after.total_dispatches, before.total_dispatches + 1);
                assert_eq!(after.interpreted_slices, before.interpreted_slices);
                assert_eq!(
                    after.interpreted_instructions,
                    before.interpreted_instructions
                );
                assert_eq!(after.native_instructions, before.native_instructions);
                drop(stack);
            }
            let mut recovered = storage.borrow_mut(&ctx);
            assert!(matches!(recovered.as_slice(), [Value::Integer(117)]));
            recovered.clear();
            state.frames.clear();
        });
        lua.gc_collect();
        lua.gc_collect();
    }
}
