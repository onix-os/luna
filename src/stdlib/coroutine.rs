use std::pin::Pin;

use ottavino_gc_arena::Collect;

use crate::{
    meta_ops, BoxSequence, Callback, CallbackReturn, Context, Error, Execution, Sequence,
    SequencePoll, Stack, Table, Thread, ThreadMode,
};

use super::base::PCall;

#[derive(Collect)]
#[collect(no_drop)]
struct WrapResult<'gc> {
    thread: Thread<'gc>,
    pending_error: Option<Error<'gc>>,
}

impl<'gc> Sequence<'gc> for WrapResult<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        _: Context<'gc>,
        _: Execution<'gc, '_>,
        _: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        match self.pending_error.take() {
            Some(error) => Err(error),
            None => Ok(SequencePoll::Return),
        }
    }

    fn error(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _: Execution<'gc, '_>,
        error: Error<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        if self.pending_error.is_some() || !self.thread.has_terminal_error() {
            return Err(error);
        }
        self.pending_error = Some(error);
        self.thread.prepare_close(ctx)?;
        stack.clear();
        Ok(SequencePoll::Resume {
            thread: self.thread,
            bottom: 0,
        })
    }
}

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
                Callback::from_fn_with(&ctx, thread, |thread, ctx, _, _| {
                    Ok(CallbackReturn::Resume {
                        thread: *thread,
                        then: Some(BoxSequence::new(
                            &ctx,
                            WrapResult {
                                thread: *thread,
                                pending_error: None,
                            },
                        )),
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
            thread.prepare_close(ctx)?;
            Ok(CallbackReturn::Resume {
                thread,
                then: Some(BoxSequence::new(&ctx, PCall)),
            })
        }),
    );

    coroutine.set_field(
        ctx,
        "isyieldable",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            let current = exec.current_thread();
            stack.replace(ctx, !current.is_main && !current.thread.is_closing());
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
                    ThreadMode::Result if thread.result_is_terminal() => "dead",
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
