use std::panic::{catch_unwind, AssertUnwindSafe};

use super::thread::{Frame, LuaFrame};
use crate::{Closure, Context, Fuel, Lua, Thread, Value};

fn lua_frame(closure: Closure<'_>, base: usize, pc: usize, variable: bool) -> Frame<'_> {
    Frame::Lua {
        bottom: 0,
        closure,
        base,
        is_variable: variable,
        pc,
        stack_size: 3,
        expected_return: None,
    }
}

fn with_frame<'gc>(ctx: Context<'gc>, test: impl FnOnce(&mut LuaFrame<'gc, '_>)) {
    let thread = Thread::new(ctx).into_inner();
    let closure = Closure::load(ctx, None, &b"return 1"[..]).unwrap();
    let mut state = thread.borrow_mut(&ctx);
    let stack = state.stack;
    let mut stack = stack.borrow_mut(&ctx);
    stack.extend([Value::Integer(11), Value::Integer(22), Value::Integer(33)]);
    state.frames.push(lua_frame(closure, 0, 17, false));
    state.frames.push(lua_frame(closure, 1, 19, false));
    state.to_be_closed.push(2);
    let mut fuel = Fuel::with(7);
    fuel.interrupt();
    test(&mut LuaFrame {
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        pair_handoff: None,
        state: &mut state,
        stack: &mut stack,
        fuel: &mut fuel,
    });
    state.frames.clear();
    state.to_be_closed.clear();
    stack.clear();
}

fn unchanged_payload(frame: &LuaFrame<'_, '_>) {
    assert_eq!(frame.stack.len(), 3);
    assert!(matches!(frame.stack[0], Value::Integer(11)));
    assert!(matches!(frame.stack[1], Value::Integer(22)));
    assert!(matches!(frame.stack[2], Value::Integer(33)));
    assert_eq!(frame.fuel.remaining(), 7);
    assert!(frame.fuel.is_interrupted());
    assert!(frame.state.open_upvalues.is_empty());
    assert_eq!(&frame.state.to_be_closed[..], &[2]);
}

#[test]
fn register_view_pc_mutates_only_the_current_counter() {
    Lua::core().enter(|ctx| {
        with_frame(ctx, |frame| {
            for variable in [false, true] {
                for base in 0..=frame.stack.len() {
                    let closure = frame.closure();
                    *frame.state.frames.last_mut().unwrap() =
                        lua_frame(closure, base, 19, variable);
                    assert_eq!(*frame.registers().pc, 19);
                    *frame.registers().pc += 3;
                    assert_eq!(*frame.registers().pc, 22);
                    assert_eq!(frame.registers().fixed_list_stack(), !variable);
                    assert!(matches!(frame.state.frames[0], Frame::Lua { pc: 17, .. }));
                    unchanged_payload(frame);
                }
            }
        });
    });
}

#[test]
fn register_view_rejects_out_of_bounds_frame_bases() {
    Lua::core().enter(|ctx| {
        with_frame(ctx, |frame| {
            for base in [4, usize::MAX] {
                for variable in [false, true] {
                    let closure = frame.closure();
                    *frame.state.frames.last_mut().unwrap() =
                        lua_frame(closure, base, 19, variable);
                    assert!(catch_unwind(AssertUnwindSafe(|| {
                        let _ = frame.registers();
                    }))
                    .is_err());
                    assert!(matches!(
                        frame.state.frames.last(),
                        Some(Frame::Lua { pc: 19, .. })
                    ));
                    unchanged_payload(frame);
                }
            }
        });
    });
}

#[test]
fn register_view_rejects_missing_and_non_lua_top_frames() {
    Lua::core().enter(|ctx| {
        with_frame(ctx, |frame| {
            for top in [
                None,
                Some(Frame::Yielded),
                Some(Frame::Result { bottom: 0 }),
            ] {
                frame.state.frames.clear();
                if let Some(top) = top {
                    frame.state.frames.push(top);
                }
                let panic = catch_unwind(AssertUnwindSafe(|| {
                    let _ = frame.registers();
                }))
                .unwrap_err();
                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"top frame is not lua frame")
                );
                unchanged_payload(frame);
            }
        });
    });
}

#[test]
fn register_view_rebinds_after_frame_and_stack_changes() {
    Lua::core().enter(|ctx| {
        with_frame(ctx, |frame| {
            *frame.registers().pc = 23;
            let closure = frame.closure();
            frame.stack.resize(1024, Value::Nil);
            frame.state.frames.push(lua_frame(closure, 1024, 31, true));
            *frame.registers().pc = 37;
            assert_eq!(*frame.registers().pc, 37);
            frame.state.frames.pop();
            frame.stack.truncate(3);
            assert_eq!(*frame.registers().pc, 23);
            unchanged_payload(frame);
        });
    });
}
