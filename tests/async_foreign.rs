//! Awaiting foreign futures from inside Lua.
//!
//! Everything here runs on a hand-rolled `block_on`. That is deliberate: luna's async support is
//! `std::task` and nothing more, and a test that pulled in tokio would quietly make that untrue.

#![cfg(feature = "async")]

use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

use luna::{
    async_sequence, Callback, CallbackReturn, Closure, Executor, ExternError, Lua, SequenceReturn,
};

/// Counts how many times it was woken, so a test can prove a real waker is being used rather than
/// the NOOP one the coroutine-syntax layer polls with.
struct CountingWaker(AtomicUsize);

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Drive a future to completion by polling in a loop. Enough for tests: every future here either
/// finishes or asks to be polled again immediately.
fn block_on<F: Future>(future: F) -> (F::Output, usize) {
    let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return (value, counter.0.load(Ordering::SeqCst)),
            Poll::Pending => continue,
        }
    }
}

/// A future that is `Pending` for `n` polls and then yields `value`.
struct Delayed<T> {
    remaining: usize,
    value: Option<T>,
}

impl<T: Unpin> Future for Delayed<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if self.remaining == 0 {
            Poll::Ready(self.value.take().expect("polled after completion"))
        } else {
            self.remaining -= 1;
            // A real future would register the waker with its reactor; waking immediately is the
            // equivalent for a poll-driven test loop.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn delayed<T>(polls: usize, value: T) -> Delayed<T> {
    Delayed {
        remaining: polls,
        value: Some(value),
    }
}

/// Install a global that awaits `polls` times and returns `value` to Lua.
fn lua_awaiting(polls: usize) -> (Lua, luna::StashedExecutor) {
    let mut lua = Lua::core();
    let executor = lua
        .try_enter(|ctx| {
            let callback = Callback::from_fn(&ctx, move |ctx, _, _| {
                Ok(CallbackReturn::Sequence(async_sequence(
                    &ctx,
                    move |_, mut seq| async move {
                        let answer = seq.await_future(delayed(polls, 42i64)).await;
                        seq.enter(|ctx, _, _, mut stack| stack.replace(ctx, answer));
                        Ok(SequenceReturn::Return)
                    },
                )))
            });
            ctx.set_global("wait", callback);

            let closure = Closure::load(ctx, Some("probe"), b"return wait() + 1")?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    (lua, executor)
}

#[test]
fn a_future_that_is_ready_immediately() {
    let (mut lua, executor) = lua_awaiting(0);
    let (result, _) = block_on(lua.execute_async::<i64>(&executor));
    assert_eq!(result.unwrap(), 43);
}

#[test]
fn a_future_that_completes_after_several_polls() {
    let (mut lua, executor) = lua_awaiting(5);
    let (result, wakes) = block_on(lua.execute_async::<i64>(&executor));
    assert_eq!(result.unwrap(), 43);
    // The real waker reached the foreign future: the NOOP one would never have counted.
    assert!(wakes >= 5, "expected at least 5 wakes, got {wakes}");
}

/// Lua either side of the await, so the suspension has to survive a real frame stack.
#[test]
fn awaiting_interleaves_with_lua() {
    let mut lua = Lua::core();
    let executor = lua
        .try_enter(|ctx| {
            let callback = Callback::from_fn(&ctx, |ctx, _, _| {
                Ok(CallbackReturn::Sequence(async_sequence(
                    &ctx,
                    |_, mut seq| async move {
                        let a = seq.await_future(delayed(2, 10i64)).await;
                        let b = seq.await_future(delayed(1, 30i64)).await;
                        seq.enter(move |ctx, _, _, mut stack| stack.replace(ctx, a + b));
                        Ok(SequenceReturn::Return)
                    },
                )))
            });
            ctx.set_global("wait", callback);

            let closure = Closure::load(
                ctx,
                Some("probe"),
                br#"
                    local total = 0
                    for i = 1, 3 do
                        total = total + wait() + i
                    end
                    return total
                "#,
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    let (result, _) = block_on(lua.execute_async::<i64>(&executor));
    // (40 + 1) + (40 + 2) + (40 + 3)
    assert_eq!(result.unwrap(), 126);
}

/// A future is allowed to produce an error; it travels back as a normal Lua error.
#[test]
fn an_awaited_future_can_fail() {
    let mut lua = Lua::core();
    let executor = lua
        .try_enter(|ctx| {
            let callback = Callback::from_fn(&ctx, |ctx, _, _| {
                Ok(CallbackReturn::Sequence(async_sequence(
                    &ctx,
                    |_, mut seq| async move {
                        let outcome: Result<i64, &'static str> = seq
                            .await_future(delayed(1, Err("network is on fire")))
                            .await;
                        match outcome {
                            Ok(v) => {
                                seq.enter(move |ctx, _, _, mut stack| stack.replace(ctx, v));
                                Ok(SequenceReturn::Return)
                            }
                            Err(message) => Err(seq
                                .try_enter(|ctx, _, _, _| -> Result<(), luna::Error<'_>> {
                                    Err(luna::IntoValue::into_value(message, ctx).into())
                                })
                                .unwrap_err()),
                        }
                    },
                )))
            });
            ctx.set_global("wait", callback);

            let closure = Closure::load(
                ctx,
                Some("probe"),
                b"local ok, err = pcall(wait) return tostring(err)",
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    let (result, _) = block_on(lua.execute_async::<String>(&executor));
    assert!(
        result
            .as_deref()
            .unwrap_or("")
            .contains("network is on fire"),
        "got {result:?}"
    );
}

/// The synchronous driver must refuse an awaiting sequence rather than spin on a slice that can
/// never advance — polling the future needs the arena released, which `finish` never does.
#[test]
fn the_sync_driver_refuses_rather_than_hanging() {
    let (mut lua, executor) = lua_awaiting(1);
    let err = lua
        .execute::<i64>(&executor)
        .expect_err("the sync driver should refuse an awaiting executor");
    assert!(
        err.to_string().contains("bad thread mode"),
        "expected a thread-mode error, got {err}"
    );
}

/// Waiting on I/O is not running, so it must not burn fuel: a host that interrupts on a fuel
/// budget must not mistake a blocked script for a runaway one.
#[test]
fn waiting_does_not_consume_the_whole_fuel_budget() {
    let mut lua = Lua::core();
    let polls = Rc::new(Cell::new(0usize));
    let seen = polls.clone();

    let executor = lua
        .try_enter(|ctx| {
            let callback = Callback::from_fn(&ctx, move |ctx, _, _| {
                Ok(CallbackReturn::Sequence(async_sequence(
                    &ctx,
                    |_, mut seq| async move {
                        seq.await_future(delayed(20, ())).await;
                        seq.enter(|ctx, _, _, mut stack| stack.replace(ctx, 1i64));
                        Ok(SequenceReturn::Return)
                    },
                )))
            });
            ctx.set_global("wait", callback);
            let closure = Closure::load(ctx, Some("probe"), b"return wait()")?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    // Step manually with a tiny budget: each slice that parks a future should end without having
    // spent its fuel, so a whole run needs far fewer steps than the budget would allow.
    let (result, _) = block_on(async {
        loop {
            let finished = lua
                .enter(|ctx| {
                    seen.set(seen.get() + 1);
                    let mut fuel = luna::Fuel::with(8);
                    ctx.fetch(&executor).step(ctx, &mut fuel)
                })
                .unwrap();
            if let Some(future) = lua.enter(|ctx| ctx.fetch(&executor).take_pending_future(&ctx)) {
                future.await;
                continue;
            }
            if finished {
                break;
            }
        }
        lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<i64>(ctx)?)
    });

    assert_eq!(result.unwrap(), 1);
    assert!(polls.get() < 60, "too many steps: {}", polls.get());
}

/// With the feature on but nothing awaiting, the ordinary drivers are unaffected.
#[test]
fn a_non_awaiting_executor_still_runs_synchronously() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), b"return 6 * 7")?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    Ok(())
}

/// Dropping a parked future instead of awaiting it is a host error, and it fails loudly.
///
/// `take_pending_future` hands over ownership; a host that drops it and steps again resumes a
/// sequence whose future never produced a value. There is nothing sensible to return, so this
/// panics with a message that says exactly that rather than yielding a bogus value.
#[test]
#[should_panic(expected = "a parked future was resumed before it completed")]
fn dropping_a_parked_future_fails_loudly() {
    let (mut lua, executor) = lua_awaiting(1);

    // First slice parks the future.
    lua.enter(|ctx| {
        let mut fuel = luna::Fuel::with(4096);
        ctx.fetch(&executor).step(ctx, &mut fuel)
    })
    .unwrap();

    let parked = lua.enter(|ctx| ctx.fetch(&executor).take_pending_future(&ctx));
    assert!(parked.is_some(), "expected a parked future");
    drop(parked);

    // Stepping again resumes the sequence with nothing to give it.
    let _ = lua.enter(|ctx| {
        let mut fuel = luna::Fuel::with(4096);
        ctx.fetch(&executor).step(ctx, &mut fuel)
    });
}
