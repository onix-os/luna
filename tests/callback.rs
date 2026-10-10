use std::pin::Pin;

use luna::{
    BoxSequence, Callback, CallbackReturn, Closure, Context, Error, Execution, Executor,
    ExternError, Function, IntoValue, Sequence, SequencePoll, Stack, String, Thread, Value,
};
use ottavino_gc_arena::Collect;

#[test]
fn callback() -> Result<(), ExternError> {
    let mut lua = common::core();

    lua.try_enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |_, _, mut stack| {
            stack.push_back(Value::Integer(42));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("callback", callback);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                local a, b, c = callback(1, 2)
                assert(a == 1 and b == 2 and c == 42)
                local d, e, f = callback(3, 4)
                assert(d == 3 and e == 4 and f == 42)
            "#[..],
        )?;

        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    lua.execute::<()>(&executor)?;
    Ok(())
}

#[test]
fn tail_call_trivial_callback() -> Result<(), ExternError> {
    let mut lua = common::core();

    lua.try_enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |_, _, mut stack| {
            stack.push_back(Value::Integer(3));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("callback", callback);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                return callback(1, 2)
            "#[..],
        )?;

        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert_eq!(lua.execute::<(i64, i64, i64)>(&executor)?, (1, 2, 3));
    Ok(())
}

#[test]
fn loopy_callback() -> Result<(), ExternError> {
    let mut lua = common::core();

    lua.try_enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |ctx, _, _| {
            #[derive(Collect)]
            #[collect(require_static)]
            struct Cont(i64);

            impl<'gc> Sequence<'gc> for Cont {
                fn poll(
                    mut self: Pin<&mut Self>,
                    _ctx: Context<'gc>,
                    _exec: Execution<'gc, '_>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                    stack.push_back(self.0.into());
                    self.0 += 1;
                    if self.0 > 6 {
                        Ok(SequencePoll::Return)
                    } else {
                        Ok(SequencePoll::Pending)
                    }
                }
            }

            Ok(CallbackReturn::Call {
                function: Callback::from_fn(&ctx, |_, _, mut stack| {
                    stack.push_back(3.into());
                    Ok(CallbackReturn::Yield {
                        to_thread: None,
                        then: None,
                    })
                })
                .into(),
                then: Some(BoxSequence::new(&ctx, Cont(4))),
            })
        });
        ctx.set_global("callback", callback);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                local function cotest()
                    return callback(1, 2)
                end

                local co = coroutine.create(cotest)

                local e1, r1, r2, r3 = coroutine.resume(co)
                local s1 = coroutine.status(co)
                local e2, r4, r5, r6, r7, r8, r9 = coroutine.resume(co, r1, r2, r3)
                local s2 = coroutine.status(co)

                return
                    e1 == true and
                    r1 == 1 and r2 == 2 and r3 == 3 and
                    s1 == "suspended" and
                    e2 == true and
                    r4 == 1 and r5 == 2 and r6 == 3 and r7 == 4 and r8 == 5 and r9 == 6 and
                    s2 == "dead"
            "#[..],
        )?;

        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}

#[test]
fn yield_sequence() -> Result<(), ExternError> {
    let mut lua = common::core();

    lua.try_enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            #[derive(Collect)]
            #[collect(require_static)]
            struct Cont(i8);

            impl<'gc> Sequence<'gc> for Cont {
                fn poll(
                    mut self: Pin<&mut Self>,
                    ctx: Context<'gc>,
                    _exec: Execution<'gc, '_>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                    match self.0 {
                        0 => {
                            let (a, b): (i32, i32) = stack.consume(ctx)?;
                            assert_eq!((a, b), (5, 6));
                            stack.extend([
                                Value::Integer(5),
                                Value::Integer(6),
                                Value::Integer(7),
                                Value::Integer(8),
                            ]);
                            self.0 = 1;
                            Ok(SequencePoll::Yield {
                                to_thread: None,
                                bottom: 2,
                            })
                        }
                        1 => {
                            let (a, b, c, d): (i32, i32, i32, i32) = stack.consume(ctx)?;
                            assert_eq!((a, b, c, d), (5, 6, 9, 10));
                            stack.extend([Value::Integer(11), Value::Integer(12)]);
                            self.0 = 2;
                            Ok(SequencePoll::Return)
                        }
                        _ => unreachable!(),
                    }
                }
            }

            let (a, b): (i32, i32) = stack.consume(ctx)?;
            assert_eq!((a, b), (1, 2));
            stack.extend([Value::Integer(3), Value::Integer(4)]);
            Ok(CallbackReturn::Yield {
                to_thread: None,
                then: Some(BoxSequence::new(&ctx, Cont(0))),
            })
        });
        ctx.set_global("callback", callback);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                local co = coroutine.create(callback)

                local e, r1, r2 = coroutine.resume(co, 1, 2)
                assert(e == true and r1 == 3 and r2 == 4)
                assert(coroutine.status(co) == "suspended")

                local e, r1, r2 = coroutine.resume(co, 5, 6)
                assert(e == true and r1 == 7 and r2 == 8)
                assert(coroutine.status(co) == "suspended")

                local e, r1, r2 = coroutine.resume(co, 9, 10)
                assert(e == true and r1 == 11 and r2 == 12)
                assert(coroutine.status(co) == "dead")
            "#[..],
        )?;

        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    lua.execute(&executor)
}

#[test]
fn resume_with_err() {
    let mut lua = common::core();

    let executor = lua.enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            #[derive(Collect)]
            #[collect(require_static)]
            struct Cont;

            impl<'gc> Sequence<'gc> for Cont {
                fn poll(
                    self: Pin<&mut Self>,
                    ctx: Context<'gc>,
                    _exec: Execution<'gc, '_>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                    stack.replace(ctx, 12);
                    Ok(SequencePoll::Call {
                        function: Callback::from_fn(&ctx, |ctx, _, _| {
                            Err("an error".into_value(ctx).into())
                        })
                        .into(),
                        bottom: 1,
                    })
                }

                fn error(
                    self: Pin<&mut Self>,
                    ctx: Context<'gc>,
                    _exec: Execution<'gc, '_>,
                    error: Error<'gc>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                    assert_eq!(stack.consume::<i32>(ctx).unwrap(), 12);
                    Err(error)
                }
            }

            assert!(stack.len() == 1);
            assert_eq!(stack.consume::<String>(ctx)?, "resume");
            stack.replace(ctx, "return");
            Ok(CallbackReturn::Yield {
                to_thread: None,
                then: Some(BoxSequence::new(&ctx, Cont)),
            })
        });

        let thread = Thread::new(ctx);
        thread
            .start_suspended(&ctx, Function::Callback(callback))
            .unwrap();

        thread.resume(ctx, "resume").unwrap();

        ctx.stash(Executor::run(&ctx, thread).unwrap())
    });

    lua.finish(&executor).unwrap();

    lua.enter(|ctx| {
        let executor = ctx.fetch(&executor);
        assert!(executor.take_result::<String>(ctx).unwrap().unwrap() == "return");
        executor.resume(ctx, ()).unwrap();
    });

    lua.finish(&executor).unwrap();

    lua.enter(
        |ctx| match ctx.fetch(&executor).take_result::<()>(ctx).unwrap() {
            Err(Error::Lua(val)) => {
                assert!(matches!(val.0, Value::String(s) if s == "an error"))
            }
            _ => panic!("wrong error returned"),
        },
    );
}

#[test]
fn nested_callback_outcomes_preserve_payloads_and_single_drops() -> Result<(), ExternError> {
    use std::{
        cell::Cell,
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
    };

    #[derive(Collect)]
    #[collect(require_static)]
    struct Counted(Rc<Cell<usize>>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    impl<'gc> Sequence<'gc> for Counted {
        fn poll(
            self: Pin<&mut Self>,
            _: Context<'gc>,
            _: Execution<'gc, '_>,
            _: Stack<'gc, '_>,
        ) -> Result<SequencePoll<'gc>, Error<'gc>> {
            panic!("forwarded sequence must not be polled");
        }
    }

    let drops = Rc::new(Cell::new(0));
    let mut lua = common::core();
    let executor = lua.try_enter(|ctx| {
        let counts = drops.clone();
        ctx.set_global(
            "probe",
            Callback::from_fn(&ctx, move |ctx, mut exec, mut stack| {
                let choice: i64 = stack.consume(ctx)?;
                let counts = counts.clone();
                let child = Callback::from_fn(&ctx, move |ctx, _, _| {
                    let sequence = || BoxSequence::new(&ctx, Counted(counts.clone()));
                    Ok(match choice {
                        0 => CallbackReturn::Return,
                        1 => CallbackReturn::Sequence(sequence()),
                        2 => CallbackReturn::Call {
                            function: Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return))
                                .into(),
                            then: Some(sequence()),
                        },
                        3 => CallbackReturn::Yield {
                            to_thread: Some(Thread::new(ctx)),
                            then: Some(sequence()),
                        },
                        4 => CallbackReturn::Resume {
                            thread: Thread::new(ctx),
                            then: Some(sequence()),
                        },
                        5 => return Err(Value::Integer(91).into()),
                        6 => {
                            let _sequence = sequence();
                            std::panic::panic_any(73usize);
                        }
                        _ => unreachable!(),
                    })
                });
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    child.call(ctx, exec.reborrow(), stack.reborrow())
                }));
                let actual = match outcome {
                    Ok(Ok(CallbackReturn::Return)) => 0,
                    Ok(Ok(CallbackReturn::Sequence(_))) => 1,
                    Ok(Ok(CallbackReturn::Call { function, then })) => {
                        assert!(matches!(function, Function::Callback(_)) && then.is_some());
                        2
                    }
                    Ok(Ok(CallbackReturn::Yield { to_thread, then })) => {
                        assert!(to_thread.is_some() && then.is_some());
                        3
                    }
                    Ok(Ok(CallbackReturn::Resume { thread, then })) => {
                        assert_eq!(thread.mode(), luna::ThreadMode::Stopped);
                        assert!(then.is_some());
                        4
                    }
                    Ok(Err(Error::Lua(error))) => {
                        assert!(matches!(error.0, Value::Integer(91)));
                        5
                    }
                    Err(payload) => {
                        assert_eq!(payload.downcast_ref::<usize>(), Some(&73));
                        6
                    }
                    _ => panic!("unexpected forwarded outcome"),
                };
                assert_eq!(actual, choice);
                stack.replace(ctx, true);
                Ok(CallbackReturn::Return)
            }),
        );
        let closure = Closure::load(ctx, None, b"for i=0,6 do assert(probe(i)) end return 42")?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert_eq!(drops.get(), 5);
    lua.gc_collect();
    assert_eq!(drops.get(), 5);
    Ok(())
}

mod common;
