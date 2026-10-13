use std::{cell::Cell, rc::Rc};

use luna::{Callback, CallbackReturn, Closure, Executor, Fuel, Lua, StashedExecutor, Value};

const SOURCE: &[u8] = b"local n=0 for i=1,7 do n=n+i end return n";

fn native_modes() -> impl Iterator<Item = bool> {
    [false, true].into_iter().filter(|native| {
        !native
            || cfg!(all(
                feature = "jit",
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))
    })
}

fn state(native: bool) -> Lua {
    let lua = Lua::empty();
    assert!(!native || cfg!(feature = "jit"));
    #[cfg(feature = "jit")]
    let mut lua = lua;
    #[cfg(feature = "jit")]
    lua.set_jit_config(luna::JitConfig {
        mode: if native {
            luna::JitMode::Auto
        } else {
            luna::JitMode::Off
        },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    lua
}

fn prepare(lua: &mut Lua, native: bool) {
    #[cfg(feature = "jit")]
    if native {
        assert!(lua.prepare_jit().unwrap() > 0);
    }
    let _ = (lua, native);
}

fn drive(lua: &mut Lua, executor: &StashedExecutor, budget: i32) {
    for _ in 0..10000 {
        let completed = lua.enter(|ctx| {
            ctx.fetch(executor)
                .step(ctx, &mut Fuel::with(budget))
                .unwrap()
        });
        lua.gc_collect();
        if completed {
            return;
        }
    }
    panic!("hooked execution did not finish: budget={budget}");
}

fn measure(count: u32, lua_hook: bool, native: bool, budget: i32) -> u32 {
    let mut lua = state(native);
    let calls = Rc::new(Cell::new(0));
    let executor = lua.enter(|ctx| {
        let calls = calls.clone();
        let record = Callback::from_fn(&ctx, move |_, _, _| {
            calls.set(calls.get() + 1);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("record", record);
        let hook = if lua_hook {
            Value::from(
                Closure::load(ctx, None, b"record(); local n=0 for i=1,3 do n=n+i end").unwrap(),
            )
        } else {
            record.into()
        };
        ctx.set_debug_hook(hook, false, count);
        let closure = Closure::load(ctx, None, SOURCE).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    prepare(&mut lua, native);
    drive(&mut lua, &executor, budget);
    assert_eq!(
        lua.enter(|ctx| ctx
            .fetch(&executor)
            .take_result::<i64>(ctx)
            .unwrap()
            .unwrap()),
        28
    );
    calls.get()
}

#[test]
fn count_intervals_exclude_hook_body_and_replayed_instruction() {
    let instructions = measure(1, false, false, 65536);
    assert!(instructions > 7);
    for native in native_modes() {
        for lua_hook in [false, true] {
            for count in [1, 2, 3, 7, u32::MAX] {
                for budget in [1, 64, 65536] {
                    assert_eq!(
                        measure(count, lua_hook, native, budget),
                        instructions / count,
                        "count={count} lua_hook={lua_hook} native={native} budget={budget}"
                    );
                }
            }
        }
    }
}

#[test]
fn replacing_or_removing_hook_does_not_replay_delivery() {
    let instructions = measure(1, false, false, 65536);
    for replace in [false, true] {
        for native in native_modes() {
            for budget in [1, 64, 65536] {
                let mut lua = state(native);
                let first = Rc::new(Cell::new(0));
                let second = Rc::new(Cell::new(0));
                let executor = lua.enter(|ctx| {
                    let first = first.clone();
                    let second = second.clone();
                    let hook = Callback::from_fn(&ctx, move |ctx, _, _| {
                        first.set(first.get() + 1);
                        let replacement = if replace {
                            let second = second.clone();
                            Callback::from_fn(&ctx, move |_, _, _| {
                                second.set(second.get() + 1);
                                Ok(CallbackReturn::Return)
                            })
                            .into()
                        } else {
                            Value::Nil
                        };
                        ctx.set_debug_hook(replacement, false, 1);
                        Ok(CallbackReturn::Return)
                    });
                    ctx.set_debug_hook(hook.into(), false, 1);
                    let closure = Closure::load(ctx, None, SOURCE).unwrap();
                    ctx.stash(Executor::start(ctx, closure.into(), ()))
                });
                prepare(&mut lua, native);
                drive(&mut lua, &executor, budget);
                assert_eq!(
                    lua.enter(|ctx| ctx
                        .fetch(&executor)
                        .take_result::<i64>(ctx)
                        .unwrap()
                        .unwrap()),
                    28
                );
                assert_eq!(first.get(), 1);
                assert_eq!(second.get(), if replace { instructions - 1 } else { 0 });
            }
        }
    }
}

#[test]
fn yielding_hook_resumes_the_pending_instruction_after_collection() {
    for native in native_modes() {
        for budget in [1, 64, 65536] {
            let mut lua = state(native);
            let calls = Rc::new(Cell::new(0));
            let executor = lua.enter(|ctx| {
                let calls = calls.clone();
                let hook = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                    calls.set(calls.get() + 1);
                    ctx.set_debug_hook(Value::Nil, false, 0);
                    stack.replace(ctx, 9);
                    Ok(CallbackReturn::Yield {
                        to_thread: None,
                        then: None,
                    })
                });
                ctx.set_debug_hook(hook.into(), false, 1);
                let closure = Closure::load(ctx, None, SOURCE).unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            });
            prepare(&mut lua, native);
            drive(&mut lua, &executor, budget);
            lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 9);
                assert_eq!(executor.mode(), luna::ExecutorMode::Suspended);
                executor.resume(ctx, ()).unwrap();
            });
            lua.gc_collect();
            drive(&mut lua, &executor, budget);
            assert_eq!(
                lua.enter(|ctx| ctx
                    .fetch(&executor)
                    .take_result::<i64>(ctx)
                    .unwrap()
                    .unwrap()),
                28
            );
            assert_eq!(calls.get(), 1);
        }
    }
}

#[test]
fn resetting_after_hook_return_drops_the_pending_resume() {
    let instructions = measure(1, false, false, 65536);
    for native in native_modes() {
        let mut lua = state(native);
        let calls = Rc::new(Cell::new(0));
        let (executor, closure) = lua.enter(|ctx| {
            let calls = calls.clone();
            let hook = Callback::from_fn(&ctx, move |_, _, _| {
                calls.set(calls.get() + 1);
                Ok(CallbackReturn::Return)
            });
            ctx.set_debug_hook(hook.into(), false, 1);
            let closure = Closure::load(ctx, None, SOURCE).unwrap();
            (
                ctx.stash(Executor::start(ctx, closure.into(), ())),
                ctx.stash(closure),
            )
        });
        prepare(&mut lua, native);
        for _ in 0..2 {
            assert!(!lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut Fuel::with(1)).unwrap()));
        }
        assert_eq!(calls.get(), 1);
        lua.gc_collect();
        lua.enter(|ctx| {
            ctx.fetch(&executor)
                .restart(ctx, ctx.fetch(&closure).into(), ())
        });
        calls.set(0);
        drive(&mut lua, &executor, 1);
        assert_eq!(
            lua.enter(|ctx| ctx
                .fetch(&executor)
                .take_result::<i64>(ctx)
                .unwrap()
                .unwrap()),
            28
        );
        assert_eq!(calls.get(), instructions);
    }
}

#[test]
fn errors_from_hook_or_resumed_opcode_leave_no_pending_replay() {
    for native in native_modes() {
        for hook_error in [false, true] {
            let mut lua = state(native);
            let executor = lua.enter(|ctx| {
                let hook = Callback::from_fn(&ctx, move |ctx, _, _| {
                    if hook_error {
                        ctx.set_debug_hook(Value::Nil, false, 0);
                        Err(luna::RuntimeError::new(anyhow::anyhow!("hook failure")).into())
                    } else {
                        Ok(CallbackReturn::Return)
                    }
                });
                ctx.set_debug_hook(hook.into(), false, 1);
                let closure = Closure::load(ctx, Some("hook-error"), b"return nil + 1").unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            });
            prepare(&mut lua, native);
            drive(&mut lua, &executor, 1);
            lua.enter(|ctx| {
                let error = ctx
                    .fetch(&executor)
                    .take_result::<i64>(ctx)
                    .unwrap()
                    .unwrap_err();
                if hook_error {
                    assert!(error.to_string().contains("hook failure"));
                } else {
                    assert!(error.to_string().contains("hook-error:1:"), "{error}");
                }
                ctx.set_debug_hook(Value::Nil, false, 0);
                let closure = Closure::load(ctx, None, SOURCE).unwrap();
                ctx.fetch(&executor).restart(ctx, closure.into(), ());
            });
            drive(&mut lua, &executor, 1);
            assert_eq!(
                lua.enter(|ctx| ctx
                    .fetch(&executor)
                    .take_result::<i64>(ctx)
                    .unwrap()
                    .unwrap()),
                28
            );
        }
    }
}

#[test]
fn count_one_hook_advances_each_instruction() {
    for budget in [1, 64, 65536] {
        let mut lua = Lua::empty();
        let calls = Rc::new(Cell::new(0));
        let executor = lua.enter(|ctx| {
            let calls = calls.clone();
            let hook = Callback::from_fn(&ctx, move |_, _, _| {
                calls.set(calls.get() + 1);
                Ok(CallbackReturn::Return)
            });
            ctx.set_debug_hook(hook.into(), false, 1);
            let closure =
                Closure::load(ctx, None, b"local n=0 for i=1,7 do n=n+i end return n").unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        let mut completed = false;
        for _ in 0..1000 {
            completed = lua.enter(|ctx| {
                ctx.fetch(&executor)
                    .step(ctx, &mut Fuel::with(budget))
                    .unwrap()
            });
            lua.gc_collect();
            if completed {
                break;
            }
        }
        assert!(
            completed,
            "count-one hook stalled: budget={budget} calls={}",
            calls.get()
        );
        let result = lua.enter(|ctx| {
            ctx.fetch(&executor)
                .take_result::<i64>(ctx)
                .unwrap()
                .unwrap()
        });
        assert_eq!(result, 28);
        assert!(calls.get() > 7);
    }
}
