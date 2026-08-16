//! Awaiting foreign futures from a Lua callback.
//!
//! luna's async support is `std::task` and nothing else — it never chooses a runtime for you. This
//! example therefore ships its own three-line executor; in a real program that would be tokio,
//! smol, or whatever you already have, and nothing else here would change.
//!
//! Run with: `make run EXAMPLE=async FEATURES=async`

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    time::{Duration, Instant},
};

use luna::{async_sequence, Callback, CallbackReturn, Closure, Executor, Lua, SequenceReturn};

/// The smallest thing that can drive a future: spin, and let the OS have the thread in between.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::yield_now();
    }
}

/// A timer, standing in for whatever real I/O a host would expose — a socket read, a query, an
/// HTTP response. The only thing luna requires of it is that it is a `Future` and `'static`.
struct Sleep(Instant);

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.0 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn sleep(ms: u64) -> Sleep {
    Sleep(Instant::now() + Duration::from_millis(ms))
}

fn main() {
    let mut lua = Lua::full();

    let executor = lua
        .try_enter(|ctx| {
            // A perfectly ordinary Lua function, which happens to suspend the whole VM while it
            // waits. Nothing above it in the Lua call stack needs to know.
            let callback = Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let millis: u64 = stack.consume(ctx)?;
                Ok(CallbackReturn::Sequence(async_sequence(
                    &ctx,
                    move |_, mut seq| async move {
                        let started = Instant::now();
                        seq.await_future(sleep(millis)).await;
                        let elapsed = started.elapsed().as_millis() as i64;
                        seq.enter(move |ctx, _, _, mut stack| stack.replace(ctx, elapsed));
                        Ok(SequenceReturn::Return)
                    },
                )))
            });
            ctx.set_global("sleep", callback);

            let closure = Closure::load(
                ctx,
                Some("example"),
                br#"
                    -- Reads like blocking code, suspends like async code.
                    local total = 0
                    for _, ms in ipairs { 30, 20, 10 } do
                        local slept = sleep(ms)
                        print(("slept ~%dms"):format(slept))
                        total = total + slept
                    end
                    return total
                "#,
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    // The one `await` in the whole program. Everything inside the VM suspended and resumed around
    // it without any of the Lua code being written differently.
    let total = block_on(lua.execute_async::<i64>(&executor)).unwrap();
    println!("total ~{total}ms");
}
