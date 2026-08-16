//! Running `__close` handlers for to-be-closed variables.
//!
//! A `local x <close>` handler must run when the block is left by *any* route: falling off the end,
//! `return`, `break`, `goto` out of it, and an error unwinding through it. luna gets that coverage
//! by hanging the work off the same "close everything at or above this level" rule that already
//! governs open upvalues, so there is one place for every exit rather than one per exit kind.
//!
//! Handlers cannot be called where that rule fires — the VM and the executor's unwinding path are
//! both mid-operation — so the values are collected and handed to this sequence, which the executor
//! drives like any other.

use std::pin::Pin;

use ottavino_gc_arena::Collect;

use crate::{
    meta_ops, Context, Error, Execution, MetaMethod, Sequence, SequencePoll, Stack, Value,
};

/// Runs `__close` for a batch of to-be-closed values, then resumes whatever was happening.
#[derive(Collect)]
#[collect(no_drop)]
pub struct CloseSequence<'gc> {
    /// Still to close, in declaration order; popped from the end so the last declared runs first.
    remaining: Vec<Value<'gc>>,
    /// The error this scope is unwinding, if it is. Re-raised once every handler has run.
    ///
    /// A handler that errors itself replaces this, matching PUC-Rio: the last error out of a
    /// closing scope is the one that propagates.
    pending_error: Option<Error<'gc>>,
}

impl<'gc> CloseSequence<'gc> {
    pub fn new(remaining: Vec<Value<'gc>>, pending_error: Option<Error<'gc>>) -> Self {
        Self {
            remaining,
            pending_error,
        }
    }

    /// Find the next value with a `__close` metamethod and ask for it to be called.
    ///
    /// `false` and `nil` are skipped rather than rejected, as Lua allows a to-be-closed variable to
    /// hold them so that a conditional resource needs no special case at the call site.
    fn step(
        &mut self,
        ctx: Context<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        while let Some(value) = self.remaining.pop() {
            if matches!(value, Value::Nil | Value::Boolean(false)) {
                continue;
            }

            let Some(handler) = meta_ops::get_metamethod(ctx, value, MetaMethod::Close) else {
                // Reaching here means the value lost its metamethod after being marked, since
                // marking checks for one.
                return Err(crate::IntoValue::into_value(
                    format!(
                        "variable of type {} has no '__close' metamethod",
                        value.type_name()
                    ),
                    ctx,
                )
                .into());
            };

            let error_value = self
                .pending_error
                .as_ref()
                .map(|e| e.to_value(ctx))
                .unwrap_or(Value::Nil);

            stack.replace(ctx, (value, error_value));
            return Ok(SequencePoll::Call {
                bottom: 0,
                function: meta_ops::call(ctx, handler)?,
            });
        }

        // Everything is closed. Either resume, or carry on unwinding.
        stack.clear();
        match self.pending_error.take() {
            Some(err) => Err(err),
            None => Ok(SequencePoll::Return),
        }
    }
}

impl<'gc> Sequence<'gc> for CloseSequence<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        // Whatever a handler returned is discarded.
        stack.clear();
        self.step(ctx, stack)
    }

    fn error(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        error: Error<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        // A handler raised. It replaces the error being carried, and the remaining handlers still
        // run — a failure to clean one thing up must not skip cleaning up the rest.
        stack.clear();
        self.pending_error = Some(error);
        self.step(ctx, stack)
    }
}
