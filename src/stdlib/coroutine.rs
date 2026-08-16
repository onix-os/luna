use crate::{meta_ops, BoxSequence, Callback, CallbackReturn, Context, Table, Thread, ThreadMode};

use super::base::PCall;

pub fn load_coroutine<'gc>(ctx: Context<'gc>) {
    let coroutine = Table::new(&ctx);

    coroutine.set_field(
        ctx,
        "create",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread = Thread::new(ctx);
            thread
                .start_suspended(&ctx, meta_ops::call(ctx, stack.get(0))?)
                .unwrap();
            stack.replace(ctx, thread);
            Ok(CallbackReturn::Return)
        }),
    );

    coroutine.set_field(
        ctx,
        "resume",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.from_front(ctx)?;
            Ok(CallbackReturn::Resume {
                thread,
                then: Some(BoxSequence::new(&ctx, PCall)),
            })
        }),
    );

    coroutine.set_field(
        ctx,
        "continue",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.from_front(ctx)?;
            Ok(CallbackReturn::Resume { thread, then: None })
        }),
    );

    // `wrap` is `create` plus a function that resumes it. The difference from `resume` is entirely
    // in the error handling: no `PCall` sequence, so an error inside the coroutine propagates to
    // the caller instead of being reported as `false, err`.
    coroutine.set_field(
        ctx,
        "wrap",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread = Thread::new(ctx);
            thread
                .start_suspended(&ctx, meta_ops::call(ctx, stack.get(0))?)
                .unwrap();
            stack.clear();
            stack.replace(
                ctx,
                Callback::from_fn_with(&ctx, thread, |thread, _, _, _| {
                    Ok(CallbackReturn::Resume {
                        thread: *thread,
                        then: None,
                    })
                }),
            );
            Ok(CallbackReturn::Return)
        }),
    );

    // Not expressible in Lua on top of the rest of this library, unlike `wrap`.
    coroutine.set_field(
        ctx,
        "close",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.consume(ctx)?;
            match thread.reset(&ctx) {
                Ok(()) => stack.replace(ctx, true),
                Err(err) => stack.replace(ctx, (false, err.to_string())),
            }
            Ok(CallbackReturn::Return)
        }),
    );

    coroutine.set_field(
        ctx,
        "isyieldable",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            // Anything but the main thread can yield.
            stack.replace(ctx, !exec.current_thread().is_main);
            Ok(CallbackReturn::Return)
        }),
    );

    coroutine.set_field(
        ctx,
        "status",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.consume(ctx)?;
            stack.replace(
                ctx,
                match thread.mode() {
                    ThreadMode::Stopped => "dead",
                    ThreadMode::Running => "running",
                    // Active, but it resumed another coroutine and is waiting on it. PUC-Rio calls
                    // that "normal", and scheduler code ported from it depends on the distinction.
                    ThreadMode::Waiting | ThreadMode::Normal => "normal",
                    ThreadMode::Result | ThreadMode::Suspended => "suspended",
                },
            );
            Ok(CallbackReturn::Return)
        }),
    );

    coroutine.set_field(
        ctx,
        "yield",
        Callback::from_fn(&ctx, |_, _, _| {
            Ok(CallbackReturn::Yield {
                to_thread: None,
                then: None,
            })
        }),
    );

    coroutine.set_field(
        ctx,
        "yieldto",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.from_front(ctx)?;
            Ok(CallbackReturn::Yield {
                to_thread: Some(thread),
                then: None,
            })
        }),
    );

    coroutine.set_field(
        ctx,
        "running",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            let current_thread = exec.current_thread();
            stack.replace(ctx, (current_thread.thread, current_thread.is_main));
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global("coroutine", coroutine);
}
