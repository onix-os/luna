//! Driving a `Lua` on a worker thread.
//!
//! `Lua` is not `Send`, and that is not an oversight to be worked around: the arena's ownership
//! model is what makes luna's re-entrancy and pacing guarantees sound, and making it `Send` would
//! be a change to gc-arena rather than to luna. So the supported pattern is not "move the state
//! between threads" but **one `Lua` per thread, with values crossing as owned Rust data**.
//!
//! That is the Rust-idiomatic shape anyway, and luna is well placed for it: a script's results
//! convert to ordinary Rust types through the same `FromMultiValue` machinery every callback uses,
//! and those cross a channel freely.
//!
//! Run with: `make run EXAMPLE=worker_thread`

use std::sync::mpsc;
use std::thread;

use luna::{Closure, Executor, Lua};

/// What the worker is asked to do.
struct Job {
    source: String,
    reply: mpsc::Sender<Result<i64, String>>,
}

fn main() {
    let (jobs, inbox) = mpsc::channel::<Job>();

    // The `Lua` is created *on* the worker and never leaves it. Nothing about it crosses the
    // boundary; only `String` in and `i64` out.
    let worker = thread::spawn(move || {
        let mut lua = Lua::core();

        for job in inbox {
            let result = (|| -> Result<i64, String> {
                let executor = lua
                    .try_enter(|ctx| {
                        let closure = Closure::load(ctx, Some("job"), job.source.as_bytes())?;
                        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
                    })
                    .map_err(|e| e.to_string())?;

                lua.execute::<i64>(&executor).map_err(|e| e.to_string())
            })();

            let _ = job.reply.send(result);
        }
    });

    for source in [
        "local sum = 0 for i = 1, 100 do sum = sum + i end return sum",
        "return #('hello world')",
        "return nothing.at.all",
    ] {
        let (reply, answer) = mpsc::channel();
        jobs.send(Job {
            source: source.to_owned(),
            reply,
        })
        .unwrap();

        match answer.recv().unwrap() {
            Ok(value) => println!("ok:    {value}"),
            // Errors cross as text. A structured error would cross the same way any other value
            // does — as owned Rust data, not as a `luna::Error<'gc>`, which cannot leave the arena.
            Err(err) => println!("error: {}", err.lines().next().unwrap_or(&err)),
        }
    }

    drop(jobs);
    worker.join().unwrap();
}
