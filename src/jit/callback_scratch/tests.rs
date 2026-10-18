use super::*;

#[test]
fn callback_scratch_requires_matching_callback_and_normal_return() {
    for bottom in [0, 4, 5, 6, usize::MAX] {
        let mut scratch = Scratch::new(true);
        scratch.key = (17, 2);
        scratch.tail = 3;
        scratch.phase = Phase::Pending;
        scratch.callback_boundary(bottom);
        assert!(scratch.phase != Phase::Ready);
        scratch.returned();
        assert_eq!(scratch.phase == Phase::Ready, bottom == 5);
        scratch.vm_boundary();
        assert_eq!(scratch.phase == Phase::Ready, bottom == 5);
    }
}

#[test]
fn callback_scratch_new_scopes_and_non_callback_work_discard_pending_state() {
    for phase in [Phase::Empty, Phase::Pending, Phase::Active, Phase::Ready] {
        for enabled in [false, true] {
            let mut scratch = Scratch::new(enabled);
            scratch.phase = phase;
            let fresh = Scratch::new(enabled);
            assert!(fresh.phase == Phase::Empty);
            assert_eq!(fresh.enabled, enabled);
            assert_eq!(
                (fresh.closure, fresh.key, fresh.count, fresh.pc, fresh.tail),
                (0, (0, 0), 0, 0, 0)
            );
            assert!(scratch.phase == phase);
            scratch.invalidate();
            scratch.returned();
            assert!(scratch.phase == Phase::Empty);
        }
        let mut scratch = Scratch::new(true);
        scratch.phase = phase;
        scratch.vm_boundary();
        assert!(
            scratch.phase
                == if phase == Phase::Ready {
                    Phase::Ready
                } else {
                    Phase::Empty
                }
        );
    }
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod native {
    use crate::{
        Callback, CallbackReturn, Closure, Executor, Function, JitConfig, JitMode, Lua, Value,
    };
    use ottavino_gc_arena::{lock::Lock, Gc};

    fn run(selected: bool, case: u8) -> (i64, [u64; 25], (u64, u64)) {
        let mut lua = Lua::core();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        lua.enter(|ctx| {
            ctx.jit().0.borrow_mut().memory_failure = if selected {
                super::super::super::backend::Failure::PreferCallbackScratch
            } else {
                super::super::super::backend::Failure::None
            };
            if case == 1 {
                let alias = Gc::new(&ctx, Lock::new(None));
                ctx.set_global(
                    "host",
                    Callback::from_fn_with(&ctx, alias, |alias, ctx, exec, mut args| {
                        let i: i64 = args.consume(ctx)?;
                        if i == 20 {
                            let base = exec.upper_lua_frame().unwrap().base;
                            let values = exec.current_thread().thread.into_inner().borrow().stack();
                            alias.set(&ctx, Some((values, base)));
                        }
                        if let Some((values, base)) = alias.get() {
                            values.borrow_mut(&ctx)[base] = Value::Integer(i * 3);
                        }
                        Ok(CallbackReturn::Return)
                    }),
                );
            } else if case == 2 {
                let saved = Gc::new(&ctx, Lock::new(None));
                ctx.set_global(
                    "retain",
                    Callback::from_fn_with(&ctx, saved, |saved, ctx, _, mut args| {
                        let Value::Function(Function::Closure(closure)) = args.get(0) else {
                            panic!("missing closure");
                        };
                        let crate::closure::UpValueState::Open(open) =
                            closure.upvalues()[0].get().get()
                        else {
                            panic!("capture already closed");
                        };
                        saved.set(&ctx, Some(open));
                        args.clear();
                        Ok(CallbackReturn::Return)
                    }),
                );
                ctx.set_global(
                    "host",
                    Callback::from_fn_with(&ctx, saved, |saved, ctx, _, mut args| {
                        let i: i64 = args.consume(ctx)?;
                        saved.get().unwrap().set(&ctx, Value::Integer(i * 3));
                        Ok(CallbackReturn::Return)
                    }),
                );
            } else if case == 4 {
                let nested = Closure::load(
                    ctx,
                    None,
                    &b"local n=0 for i=1,10 do n=n+i end return n"[..],
                )
                .unwrap();
                ctx.set_global(
                    "host",
                    Callback::from_fn_with(&ctx, nested, |nested, ctx, _, mut args| {
                        let _: i64 = args.consume(ctx)?;
                        let executor = Executor::start(ctx, (*nested).into(), ());
                        let mut fuel = crate::Fuel::with(i32::MAX);
                        assert!(executor.step(ctx, &mut fuel).unwrap());
                        assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 55);
                        Ok(CallbackReturn::Return)
                    }),
                );
            } else {
                ctx.set_global(
                    "host",
                    Callback::from_fn_with(&ctx, case, |case, ctx, _, mut args| {
                        let _: i64 = args.consume(ctx)?;
                        if *case == 3 {
                            args.resize(512);
                            args.set(511, Value::Table(crate::Table::new(&ctx)));
                            args.clear();
                        }
                        Ok(CallbackReturn::Return)
                    }),
                );
            }
        });
        let script = if case == 2 {
            "do local x=40 retain(function() return x end) end local n=0 local sum=0 for i=1,80 do n=n+1 host(i) sum=sum+n end return sum"
        } else {
            "local n=0 local sum=0 for i=1,80 do n=n+1 host(i) sum=sum+n end return sum"
        };
        let executor = lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, script.as_bytes()).unwrap();
            assert!(closure.prototype().stack_size <= 8);
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        lua.prepare_jit().unwrap();
        let result = lua.execute::<i64>(&executor).unwrap();
        lua.gc_collect();
        let counts = lua.enter(|ctx| ctx.fetch(&executor).callback_scratch_counts());
        let stats = lua.jit_stats();
        assert!(stats.native_entries >= 80);
        (
            result,
            super::super::super::payload_runtime_tests::counters(stats),
            counts,
        )
    }

    #[test]
    fn callback_scratch_preserves_escaped_aliases_closed_captures_and_relocation() {
        for (case, expected) in [(0, 3240), (1, 9340), (2, 9720), (3, 3240), (4, 3240)] {
            let reference = run(false, case);
            let candidate = run(true, case);
            assert_eq!(reference.0, expected, "case={case}");
            assert_eq!(
                (candidate.0, candidate.1),
                (reference.0, reference.1),
                "case={case}"
            );
            if case == 2 {
                assert_eq!(candidate.2 .0, 0);
            } else {
                assert!(candidate.2 .0 > 0, "case={case} counts={:?}", candidate.2);
            }
        }
    }

    #[test]
    fn callback_scratch_reuses_only_within_the_current_executor_step() {
        for split in [false, true] {
            let mut counts = Vec::new();
            let mut stats = Vec::new();
            for selected in [false, true] {
                let mut lua = Lua::core();
                lua.set_jit_config(JitConfig {
                    mode: JitMode::Auto,
                    hot_threshold: 1,
                    ..Default::default()
                })
                .unwrap();
                let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
                let executor = lua.enter(|ctx| {
                    ctx.jit().0.borrow_mut().memory_failure = if selected {
                        super::super::super::backend::Failure::PreferCallbackScratch
                    } else {
                        super::super::super::backend::Failure::None
                    };
                    let calls = calls.clone();
                    ctx.set_global(
                        "host",
                        Callback::from_fn(&ctx, move |_, _, mut args| {
                            calls.set(calls.get() + 1);
                            args.clear();
                            Ok(CallbackReturn::Return)
                        }),
                    );
                    let closure =
                        Closure::load(ctx, None, &b"local n=1 host() n=n+1 return n"[..]).unwrap();
                    ctx.stash(Executor::start(ctx, closure.into(), ()))
                });
                lua.prepare_jit().unwrap();
                if split {
                    lua.enter(|ctx| {
                        for _ in 0..8 {
                            let mut fuel = crate::Fuel::with(8);
                            assert!(!ctx.fetch(&executor).step(ctx, &mut fuel).unwrap());
                            if calls.get() == 1 {
                                break;
                            }
                        }
                        assert_eq!(calls.get(), 1);
                        assert_eq!(ctx.fetch(&executor).mode(), crate::ExecutorMode::Normal);
                    });
                    lua.gc_collect();
                }
                assert_eq!(lua.execute::<i64>(&executor).unwrap(), 2);
                lua.enter(|ctx| {
                    assert_eq!(calls.get(), 1);
                    counts.push(ctx.fetch(&executor).callback_scratch_counts());
                });
                let actual = lua.jit_stats();
                assert_eq!(actual.native_entries, 2);
                stats.push(super::super::super::payload_runtime_tests::counters(actual));
            }
            assert_eq!(stats[0], stats[1]);
            assert_eq!(counts[0], (0, 0));
            assert_eq!(counts[1].0, u64::from(!split));
            assert!(counts[1].1 > 0);
        }
    }

    #[test]
    fn callback_scratch_selection_requires_auto_and_explicit_probe() {
        for mode in [JitMode::Off, JitMode::Auto] {
            for selected in [false, true] {
                let mut lua = Lua::empty();
                lua.set_jit_config(JitConfig {
                    mode,
                    ..Default::default()
                })
                .unwrap();
                lua.enter(|ctx| {
                    ctx.jit().0.borrow_mut().memory_failure = if selected {
                        super::super::super::backend::Failure::PreferCallbackScratch
                    } else {
                        super::super::super::backend::Failure::None
                    };
                    assert_eq!(
                        ctx.jit().callback_scratch_enabled(),
                        selected && mode == JitMode::Auto
                    );
                });
            }
        }
    }
}
