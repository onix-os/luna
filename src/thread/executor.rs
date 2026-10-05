use std::{
    hash::{Hash, Hasher},
    pin::Pin,
};

use allocator_api2::{boxed, vec};
use ottavino_gc_arena::{allocator_api::MetricsAlloc, lock::RefLock, Collect, Gc, Mutation};
use thiserror::Error;

use crate::{
    compiler::{FunctionRef, LineNumber},
    thread::BadThreadMode,
    BoxSequence, CallbackReturn, Closure, Context, Error, FromMultiValue, Fuel, Function,
    IntoMultiValue, PendingFuture, Sequence, SequencePoll, Stack, String, Thread, ThreadMode,
    Variadic,
};

use super::{
    close::CloseSequence,
    thread::{Frame, LuaFrame, StackVec, ThreadState},
    vm::run_vm,
};

enum ActiveSequence<'gc> {
    User(BoxSequence<'gc>),
    Close(boxed::Box<CloseSequence<'gc>, MetricsAlloc<'gc>>),
}

impl<'gc> ActiveSequence<'gc> {
    fn unpack(frame: Frame<'gc>) -> (usize, Self, Option<Error<'gc>>) {
        match frame {
            Frame::Sequence {
                bottom,
                sequence,
                pending_error,
            } => (bottom, Self::User(sequence), pending_error),
            Frame::Close {
                bottom,
                sequence,
                pending_error,
            } => (bottom, Self::Close(sequence), pending_error),
            _ => unreachable!(),
        }
    }

    fn frame(self, bottom: usize, pending_error: Option<Error<'gc>>) -> Frame<'gc> {
        match self {
            Self::User(sequence) => Frame::Sequence {
                bottom,
                sequence,
                pending_error,
            },
            Self::Close(sequence) => Frame::Close {
                bottom,
                sequence,
                pending_error,
            },
        }
    }

    fn poll(
        &mut self,
        ctx: Context<'gc>,
        exec: Execution<'gc, '_>,
        stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        match self {
            Self::User(sequence) => sequence.poll(ctx, exec, stack),
            Self::Close(sequence) => Pin::new(&mut **sequence).poll(ctx, exec, stack),
        }
    }

    fn error(
        &mut self,
        ctx: Context<'gc>,
        exec: Execution<'gc, '_>,
        error: Error<'gc>,
        stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        match self {
            Self::User(sequence) => sequence.error(ctx, exec, error, stack),
            Self::Close(sequence) => Pin::new(&mut **sequence).error(ctx, exec, error, stack),
        }
    }

    fn has_pending_close(&self) -> bool {
        matches!(self, Self::Close(sequence) if sequence.has_remaining())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorMode {
    /// There are no threads being run and the `Executor` must be restarted to do any work.
    Stopped,
    /// Lua has errored or returned (or yielded) values that must be taken to move the `Executor` to
    /// the `Stopped` (or `Suspended`) state.
    Result,
    /// There is an active thread in the `ThreadMode::Normal` state and it is can be run with
    /// `Executor::step`.
    Normal,
    /// The main thread has yielded and is waiting on being resumed.
    Suspended,
    /// The `Executor` is currently inside its own `Executor::step` function.
    Running,
}

#[derive(Debug, Copy, Clone, Error)]
#[error("bad executor mode: {found:?}, expected {expected:?}")]
pub struct BadExecutorMode {
    pub found: ExecutorMode,
    pub expected: ExecutorMode,
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct ExecutorState<'gc> {
    thread_stack: vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
    // A foreign future a sequence is waiting on. Parked here rather than kept inside the sequence
    // because it has to be polled with the arena *not* borrowed, which only the host can do.
    #[collect(require_static)]
    pending: Option<PendingFuture>,
}

pub type ExecutorInner<'gc> = RefLock<ExecutorState<'gc>>;

/// `chunk:line` for the innermost Lua frame, if there is one.
///
/// Taken where an error is first raised rather than where it is caught: unwinding pops one frame
/// per step, so by the time the error leaves the executor there is nothing left to ask.
fn error_position<'gc>(frames: &[Frame<'gc>]) -> Option<std::string::String> {
    let (closure, pc) = frames.iter().rev().find_map(|f| match f {
        Frame::Lua { closure, pc, .. } => Some((*closure, *pc)),
        _ => None,
    })?;
    let proto = closure.prototype();
    // `pc` has already been advanced past the faulting instruction.
    let faulting = pc.saturating_sub(1);
    let line = match proto
        .opcode_line_numbers
        .binary_search_by_key(&faulting, |(opi, _)| *opi)
    {
        Ok(i) => proto.opcode_line_numbers[i].1,
        Err(0) => proto.opcode_line_numbers.first()?.1,
        Err(i) => proto.opcode_line_numbers[i - 1].1,
    };
    Some(format!("{}:{}", proto.chunk_name.display_lossy(), line))
}

/// The entry-point for the Lua VM.
///
/// An `Executor` runs networks of [`Thread`]s that may depend on each other and may yield
/// control back and forth. All Lua code that is run is done so directly or indirectly by calling
/// [`Executor::step`].
///
/// # Panics
///
/// An `Executor` is not reentrant: calling a method on the *same* `Executor` from within a
/// callback that it is itself running (other than `Executor::mode`) will panic.
///
/// A *separate* `Executor` may be driven from inside a callback, and Lua run that way can read and
/// write open upvalues belonging to the suspended outer thread. That works because a thread's value
/// stack is a separate object with its own lock: a native holds no borrow on it between operations,
/// so a re-entrant upvalue access simply takes the lock in between. Prefer `CallbackReturn::Call` where the shape of the code allows it — it keeps
/// everything on one `Executor` and one fuel budget — but a nested `Executor` is supported for the
/// cases where there is no continuation to hand back, such as a callback several Rust frames below
/// the code that needs to call Lua.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Executor<'gc>(Gc<'gc, ExecutorInner<'gc>>);

impl<'gc> PartialEq for Executor<'gc> {
    fn eq(&self, other: &Executor<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Executor<'gc> {}

impl<'gc> Hash for Executor<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state)
    }
}

impl<'gc> Executor<'gc> {
    const VM_GRANULARITY: u32 = 64;

    const FUEL_PER_CALLBACK: i32 = 8;
    const FUEL_PER_SEQ_STEP: i32 = 4;
    const FUEL_PER_STEP: i32 = 4;

    /// Creates a new `Executor` with a stopped main thread.
    pub fn new(ctx: Context<'gc>) -> Self {
        Self::run(&ctx, Thread::new(ctx)).unwrap()
    }

    /// Creates a new `Executor` that begins running the given [`Thread`].
    ///
    /// If the provided thread is in [`ThreadMode::Waiting`] or [`ThreadMode::Running`], then this
    /// will return `Err(BadThreadMode)`.
    pub fn run(mc: &Mutation<'gc>, thread: Thread<'gc>) -> Result<Self, BadThreadMode> {
        let executor = Executor(Gc::new(
            mc,
            RefLock::new(ExecutorState {
                thread_stack: vec::Vec::new_in(MetricsAlloc::new(mc)),
                pending: None,
            }),
        ));
        executor.reset(mc, thread)?;
        Ok(executor)
    }

    pub fn from_inner(inner: Gc<'gc, ExecutorInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, ExecutorInner<'gc>> {
        self.0
    }

    /// Creates a new `Executor` with a new [`Thread`] running the given function.
    pub fn start(
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Self {
        let thread = Thread::new(ctx);
        thread.start(ctx, function, args).unwrap();
        Self::run(&ctx, thread).unwrap()
    }

    pub fn mode(self) -> ExecutorMode {
        if let Ok(state) = self.0.try_borrow() {
            if state.thread_stack.len() > 1 {
                ExecutorMode::Normal
            } else {
                match state.thread_stack[0].mode() {
                    ThreadMode::Stopped => ExecutorMode::Stopped,
                    ThreadMode::Result => ExecutorMode::Result,
                    ThreadMode::Normal => ExecutorMode::Normal,
                    ThreadMode::Suspended => ExecutorMode::Suspended,
                    ThreadMode::Waiting => {
                        // This should never happen from correct `Executor` / `Thread` use. In
                        // order for the main thread to be in the `Waiting` state with no thread
                        // being waited on, that thread must have been used by two `Executor`s at
                        // one time, and the *other* `Executor` must have moved it to the `Waiting`
                        // state.
                        //
                        // We call this `ExecutorMode::Normal` since the main thread is still not in
                        // some completed state, but calling `Executor::step` will never exit this
                        // mode (only forever return a `BadThreadMode` error).
                        ExecutorMode::Normal
                    }
                    ThreadMode::Running => ExecutorMode::Running,
                }
            }
        } else {
            ExecutorMode::Running
        }
    }

    /// Runs the VM for a period of time controlled by the `fuel` parameter.
    ///
    /// The VM and callbacks will consume fuel as they run, and `Executor::step` will return as soon
    /// as `Fuel::can_continue()` returns false *and some minimal positive progress has been made*.
    ///
    /// Returns `false` if the method has exhausted its fuel, but there is more work to
    /// do, and returns `true` if no more progress can be made. If `true` is returned, then
    /// `Executor::mode()` will no longer be `ExecutorMode::Normal`.
    ///
    /// # Errors
    ///
    /// If a `Thread` being run by this `Executor` in an unexpected state, then this method will
    /// return a `BadThreadMode` error.
    ///
    /// If a `Thread` is currently in the stack of threads being run by an `Executor`, then that
    /// `Executor` expects to be the sole instance driving those threads to completion and expects
    /// that the state of these threads will not be externally changed. This rule cannot be violated
    /// from Lua or by normal `Rust` callbacks, only by purposefully misusing an `Executor` from
    /// Rust by, for example, setting a single `Thread` as the main thread of two `Executor`s
    /// at once or by manually calling [`Thread::take_result`] or [`Thread::reset`] on a thread
    /// currently being run by an `Executor`.
    ///
    /// This is considered "outside" of a normal Lua or Rust callback error since it cannot be
    /// triggered solely by Lua and likely indicates a bug in some Rust code, so this error is
    /// delivered through a separate channel than normal results and cannot be caught by Lua.
    pub fn step(self, ctx: Context<'gc>, fuel: &mut Fuel) -> Result<bool, BadThreadMode> {
        let mut state = self.0.borrow_mut(&ctx);
        Ok(loop {
            let mut top_thread = state.thread_stack.last().copied().unwrap();
            let mut res_thread = None;
            match top_thread.mode() {
                ThreadMode::Normal => {}
                ThreadMode::Running => {
                    panic!("`Executor` thread already running")
                }
                ThreadMode::Stopped | ThreadMode::Suspended | ThreadMode::Result
                    if state.thread_stack.len() == 1 =>
                {
                    break true;
                }
                ThreadMode::Result => {
                    state.thread_stack.pop();
                    res_thread = Some(top_thread);
                    top_thread = state.thread_stack.last().copied().unwrap();
                }
                mode => {
                    return Err(BadThreadMode {
                        found: mode,
                        expected: None,
                    })
                }
            }

            if let Some(res_thread) = res_thread {
                let mut top_state = top_thread.into_inner().borrow_mut(&ctx);
                let top_state = &mut *top_state;
                let mode = top_state.mode();
                if mode != ThreadMode::Waiting {
                    // Shenanigans have happened and the top thread has had its state externally
                    // changed.
                    return Err(BadThreadMode {
                        found: mode,
                        expected: Some(ThreadMode::Waiting),
                    });
                }

                assert!(matches!(top_state.frames.pop(), Some(Frame::WaitThread)));
                assert_eq!(res_thread.mode(), ThreadMode::Result);
                // Take the results from the res_thread and return them to our top
                // thread.
                let mut res_state = res_thread.into_inner().borrow_mut(&ctx);
                let res_stack = res_state.stack;
                let mut res_stack = res_stack.borrow_mut(&ctx);
                let top_stack = top_state.stack;
                let mut top_stack = top_stack.borrow_mut(&ctx);
                match res_state.take_result(&mut res_stack) {
                    Ok(vals) => {
                        let bottom = top_stack.len();
                        top_stack.extend(vals);
                        top_state.return_to(&mut top_stack, bottom);
                    }
                    Err(err) => {
                        top_state.frames.push(Frame::Error(err.into()));
                    }
                }
                drop(res_state);
            }

            if top_thread.mode() == ThreadMode::Normal {
                fn do_yield<'gc>(
                    ctx: Context<'gc>,
                    thread_stack: &mut vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
                    top_state: &mut ThreadState<'gc>,
                    // As `do_resume`: the caller holds this stack already.
                    stack: &mut StackVec<'gc>,
                    to_thread: Option<Thread<'gc>>,
                    bottom: usize,
                ) {
                    if top_state.closing {
                        stack.truncate(bottom);
                        top_state.frames.push(Frame::Error(
                            crate::RuntimeError::new(anyhow::anyhow!(
                                "cannot yield while closing a coroutine"
                            ))
                            .into(),
                        ));
                        return;
                    }
                    if let Some(to_thread) = to_thread {
                        if let Err(err) = to_thread.resume(ctx, Variadic(stack.drain(bottom..))) {
                            top_state.frames.push(Frame::Error(err.into()));
                        } else {
                            top_state.frames.push(Frame::Yielded);
                            thread_stack.pop();
                            thread_stack.push(to_thread);
                        }
                    } else {
                        top_state.frames.push(Frame::Yielded);
                        top_state.frames.push(Frame::Result { bottom });
                    }
                }

                fn do_resume<'gc>(
                    ctx: Context<'gc>,
                    thread_stack: &mut vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
                    top_state: &mut ThreadState<'gc>,
                    // Passed in rather than borrowed here: the caller already holds this stack,
                    // and re-locking it would be a double borrow of the same object.
                    stack: &mut StackVec<'gc>,
                    thread: Thread<'gc>,
                    bottom: usize,
                ) {
                    if let Err(err) = thread.resume(ctx, Variadic(stack.drain(bottom..))) {
                        top_state.frames.push(Frame::Error(err.into()));
                    } else {
                        // Tail call the thread resume if we can.
                        if top_state.frames.is_empty() {
                            thread_stack.pop();
                        } else {
                            top_state.frames.push(Frame::WaitThread);
                        }
                        thread_stack.push(thread);
                    }
                }

                // Popped under a short borrow. A native frame has to run with its thread
                // *unborrowed*, so nothing may hold the thread's state across the call.
                let frame = top_thread.into_inner().borrow_mut(&ctx).frames.pop();
                match frame {
                    Some(Frame::Callback { bottom, callback }) => {
                        fuel.consume(Self::FUEL_PER_CALLBACK);
                        // The callback runs on a detached window rather than on the thread's own
                        // stack, and with the thread unborrowed. A callback may drive another
                        // `Executor`, and Lua run that way can read or write an open upvalue that
                        // still points into *this* thread's stack; holding the thread borrowed
                        // across the call turns that into a panic. Detaching also means a callback
                        // cannot reach the frames below `bottom`.
                        let inner = top_thread.into_inner();
                        let stack = {
                            let top_state = &mut *inner.borrow_mut(&ctx);
                            top_state.running = true;
                            top_state.stack
                        };
                        // The frames stay borrowed for the duration of the call. Nothing a native
                        // does reaches them any more: an open upvalue locks only the stack, which
                        // is a separate object, so a re-entrant read no longer needs this lock.
                        let frames = inner.borrow();
                        let result = callback.call(
                            ctx,
                            Execution {
                                executor: self,
                                fuel,
                                threads: &state.thread_stack,
                                lua_frames: &frames.frames,
                            },
                            Stack::new(ctx, stack, bottom),
                        );
                        drop(frames);
                        let top_state = &mut *top_thread.into_inner().borrow_mut(&ctx);
                        let top_stack = top_state.stack;
                        let mut top_stack = top_stack.borrow_mut(&ctx);
                        top_state.running = false;
                        match result {
                            Ok(CallbackReturn::Return) => {
                                top_state.return_to(&mut top_stack, bottom);
                            }
                            Ok(CallbackReturn::Sequence(sequence)) => {
                                top_state.frames.push(Frame::Sequence {
                                    bottom,
                                    sequence,
                                    pending_error: None,
                                });
                            }
                            Ok(CallbackReturn::Call { function, then }) => {
                                if let Some(sequence) = then {
                                    top_state.frames.push(Frame::Sequence {
                                        bottom,
                                        sequence,
                                        pending_error: None,
                                    });
                                }
                                top_state.push_call(&mut top_stack, bottom, function);
                            }
                            Ok(CallbackReturn::Yield { to_thread, then }) => {
                                if let Some(sequence) = then {
                                    top_state.frames.push(Frame::Sequence {
                                        bottom,
                                        sequence,
                                        pending_error: None,
                                    });
                                }
                                do_yield(
                                    ctx,
                                    &mut state.thread_stack,
                                    top_state,
                                    &mut top_stack,
                                    to_thread,
                                    bottom,
                                );
                            }
                            Ok(CallbackReturn::Resume { thread, then }) => {
                                if let Some(sequence) = then {
                                    top_state.frames.push(Frame::Sequence {
                                        bottom,
                                        sequence,
                                        pending_error: None,
                                    });
                                }
                                do_resume(
                                    ctx,
                                    &mut state.thread_stack,
                                    top_state,
                                    &mut top_stack,
                                    thread,
                                    bottom,
                                );
                            }
                            Err(err) => {
                                top_stack.truncate(bottom);
                                top_state.frames.push(Frame::Error(err))
                            }
                        }
                    }
                    Some(frame @ (Frame::Sequence { .. } | Frame::Close { .. })) => {
                        let (bottom, mut sequence, pending_error) = ActiveSequence::unpack(frame);
                        fuel.consume(Self::FUEL_PER_SEQ_STEP);

                        let inner = top_thread.into_inner();
                        let stack = {
                            let top_state = &mut *inner.borrow_mut(&ctx);
                            top_state.running = true;
                            top_state.stack
                        };
                        let frames = inner.borrow();
                        let exec = Execution {
                            executor: self,
                            fuel,
                            threads: &state.thread_stack,
                            lua_frames: &frames.frames,
                        };
                        let poll = if let Some(err) = pending_error {
                            sequence.error(ctx, exec, err, Stack::new(ctx, stack, bottom))
                        } else {
                            sequence.poll(ctx, exec, Stack::new(ctx, stack, bottom))
                        };
                        drop(frames);
                        let top_state = &mut *top_thread.into_inner().borrow_mut(&ctx);
                        let top_stack = top_state.stack;
                        let mut top_stack = top_stack.borrow_mut(&ctx);
                        top_state.running = false;

                        match poll {
                            Ok(SequencePoll::Pending) => {
                                top_state.frames.push(sequence.frame(bottom, None));
                            }
                            Ok(SequencePoll::Return) => {
                                top_state.return_to(&mut top_stack, bottom);
                            }
                            Ok(SequencePoll::Call {
                                function,
                                bottom: rel_bottom,
                            }) => {
                                top_state.frames.push(sequence.frame(bottom, None));
                                top_state.push_call(&mut top_stack, bottom + rel_bottom, function);
                            }
                            Ok(SequencePoll::TailCall(function)) => {
                                top_state.push_call(&mut top_stack, bottom, function);
                            }
                            Ok(SequencePoll::Yield {
                                to_thread,
                                bottom: rel_bottom,
                            }) => {
                                top_state.frames.push(sequence.frame(bottom, None));
                                do_yield(
                                    ctx,
                                    &mut state.thread_stack,
                                    top_state,
                                    &mut top_stack,
                                    to_thread,
                                    bottom + rel_bottom,
                                );
                            }
                            Ok(SequencePoll::TailYield(to_thread)) => {
                                do_yield(
                                    ctx,
                                    &mut state.thread_stack,
                                    top_state,
                                    &mut top_stack,
                                    to_thread,
                                    bottom,
                                );
                            }
                            Ok(SequencePoll::Resume {
                                thread,
                                bottom: rel_bottom,
                            }) => {
                                top_state.frames.push(sequence.frame(bottom, None));
                                do_resume(
                                    ctx,
                                    &mut state.thread_stack,
                                    top_state,
                                    &mut top_stack,
                                    thread,
                                    bottom + rel_bottom,
                                );
                            }
                            Ok(SequencePoll::TailResume(thread)) => {
                                do_resume(
                                    ctx,
                                    &mut state.thread_stack,
                                    top_state,
                                    &mut top_stack,
                                    thread,
                                    bottom,
                                );
                            }
                            Ok(SequencePoll::Waiting(future)) => {
                                if top_state.closing {
                                    top_stack.truncate(bottom);
                                    top_state.frames.push(Frame::Error(
                                        crate::RuntimeError::new(anyhow::anyhow!(
                                            "cannot await while closing a coroutine"
                                        ))
                                        .into(),
                                    ));
                                } else {
                                    top_state.frames.push(sequence.frame(bottom, None));
                                    state.pending = Some(future);
                                    break false;
                                }
                            }
                            Err(error) => {
                                top_stack.truncate(bottom);
                                if sequence.has_pending_close() {
                                    top_state.frames.push(sequence.frame(bottom, None));
                                }
                                top_state.frames.push(Frame::Error(error));
                            }
                        }
                    }
                    Some(frame @ Frame::Lua { .. }) => {
                        let top_state = &mut *top_thread.into_inner().borrow_mut(&ctx);
                        top_state.frames.push(frame);

                        // One borrow of the stack for the whole VM slice, not one per opcode.
                        let stack = top_state.stack;
                        let lua_frame = LuaFrame {
                            state: top_state,
                            stack: stack.borrow_mut(&ctx),
                            fuel,
                        };
                        #[cfg(all(
                            test,
                            feature = "jit",
                            not(miri),
                            target_os = "linux",
                            any(target_arch = "x86_64", target_arch = "aarch64")
                        ))]
                        let mut lua_frame = lua_frame;
                        #[cfg(all(
                            test,
                            feature = "jit",
                            not(miri),
                            target_os = "linux",
                            any(target_arch = "x86_64", target_arch = "aarch64")
                        ))]
                        let result = if let Some((completed, start, count)) =
                            super::vm::try_scalar_activation(
                                ctx,
                                &mut lua_frame,
                                Self::VM_GRANULARITY,
                            ) {
                            let mut stats = ctx.jit().interpreter_stats();
                            stats.dispatches = 1;
                            let result = lua_frame
                                .return_upper(&ctx, start, count)
                                .map(|()| completed);
                            if result.is_ok() {
                                stats.reported_instructions = Some(0);
                            }
                            result
                        } else {
                            run_vm(ctx, lua_frame, Self::VM_GRANULARITY)
                        };
                        #[cfg(not(all(
                            test,
                            feature = "jit",
                            not(miri),
                            target_os = "linux",
                            any(target_arch = "x86_64", target_arch = "aarch64")
                        )))]
                        let result = run_vm(ctx, lua_frame, Self::VM_GRANULARITY);
                        match result {
                            Err(err) => {
                                // Give the error a `chunk:line:` prefix while the frame that raised
                                // it is still on the stack. Added as anyhow context rather than
                                // by replacing the error with a string, so that Rust callers can
                                // still `root_cause().downcast_ref()` to the typed cause.
                                let positioned = match error_position(&top_state.frames) {
                                    // The context is the position alone, not `position: message` — the
                                    // error it wraps is its own source, and a chain-printing
                                    // formatter would otherwise repeat the message verbatim.
                                    Some(at) => Error::from(crate::RuntimeError::new(
                                        anyhow::Error::new(err).context(at),
                                    )),
                                    None => Error::from(err),
                                };
                                top_state.frames.push(Frame::Error(positioned));
                            }
                            Ok(instructions_run) => {
                                fuel.consume(instructions_run.try_into().unwrap());
                            }
                        }
                    }
                    Some(Frame::Error(err)) => {
                        let top_state = &mut *top_thread.into_inner().borrow_mut(&ctx);
                        if state.thread_stack.len() > 1
                            && !top_state.closing
                            && !top_state
                                .frames
                                .iter()
                                .any(|frame| matches!(frame, Frame::Sequence { .. }))
                        {
                            top_state.terminal_error = Some(err);
                            top_state.deferred_error = true;
                            top_state.error_taken = false;
                        } else {
                            match top_state
                                .frames
                                .pop()
                                .expect("normal thread must have frame above error")
                            {
                                Frame::Lua { bottom, .. } => {
                                    let stack = top_state.stack;
                                    let mut stack = stack.borrow_mut(&ctx);
                                    top_state.close_upvalues(&ctx, &stack, bottom);
                                    // An error unwinding past a `<close>` variable still has to run its
                                    // handler — that is the case cleanup exists for. The handler gets
                                    // the in-flight error, and the sequence re-raises it afterwards.
                                    let to_close = top_state.take_to_be_closed(&stack, bottom);
                                    stack.truncate(bottom);
                                    if to_close.is_empty() {
                                        top_state.frames.push(Frame::Error(err));
                                    } else {
                                        top_state.frames.push(Frame::close(
                                            ctx,
                                            bottom,
                                            to_close,
                                            Some(err),
                                        ));
                                    }
                                }
                                Frame::Sequence {
                                    bottom,
                                    sequence,
                                    pending_error,
                                } => {
                                    assert!(pending_error.is_none());
                                    top_state.frames.push(Frame::Sequence {
                                        bottom,
                                        sequence,
                                        pending_error: Some(err),
                                    });
                                }
                                Frame::Close {
                                    bottom,
                                    sequence,
                                    pending_error,
                                } => {
                                    assert!(pending_error.is_none());
                                    top_state.frames.push(Frame::Close {
                                        bottom,
                                        sequence,
                                        pending_error: Some(err),
                                    });
                                }
                                frame => panic!("tried to wind through improper frame {frame:?}"),
                            }
                        }
                    }
                    _ => panic!("tried to step invalid frame type"),
                }
            }

            fuel.consume(Self::FUEL_PER_STEP);

            if !fuel.should_continue() {
                break false;
            }
        })
    }

    pub fn take_result<T: FromMultiValue<'gc>>(
        self,
        ctx: Context<'gc>,
    ) -> Result<Result<T, Error<'gc>>, BadExecutorMode> {
        let mode = self.mode();
        if mode == ExecutorMode::Result {
            let state = self.0.borrow();
            Ok(state.thread_stack[0].take_result(ctx).unwrap())
        } else {
            Err(BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Result,
            })
        }
    }

    pub fn resume(
        self,
        ctx: Context<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Result<(), BadExecutorMode> {
        let mode = self.mode();
        if mode == ExecutorMode::Suspended {
            let state = self.0.borrow();
            state.thread_stack[0].resume(ctx, args).unwrap();
            Ok(())
        } else {
            Err(BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Suspended,
            })
        }
    }

    pub fn resume_err(self, mc: &Mutation<'gc>, error: Error<'gc>) -> Result<(), BadExecutorMode> {
        let mode = self.mode();
        if mode == ExecutorMode::Suspended {
            let state = self.0.borrow();
            state.thread_stack[0].resume_err(mc, error).unwrap();
            Ok(())
        } else {
            Err(BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Suspended,
            })
        }
    }

    /// Reset this `Executor` entirely, leaving it with a stopped main thread. Equivalent to
    /// creating a new executor with `Executor::new`.
    pub fn stop(self, mc: &Mutation<'gc>) {
        let mut state = self.0.borrow_mut(mc);
        state.thread_stack.truncate(1);
        state.thread_stack[0].reset(mc).unwrap();
    }

    /// Take the foreign future this executor is waiting on, if any.
    ///
    /// A sequence that returns [`SequencePoll::Waiting`] parks its future here and the slice ends.
    /// The future must be polled with the arena *not* borrowed, so only a host outside `enter` can
    /// drive it — see `Lua::execute_async` (the `async` feature), which is what normally calls this.
    ///
    /// Taking the future does not disturb the sequence: it is already back on the frame stack and
    /// will be polled again on the next step, as though it had returned `Pending`.
    pub fn take_pending_future(self, mc: &Mutation<'gc>) -> Option<PendingFuture> {
        self.0.borrow_mut(mc).pending.take()
    }

    /// Whether this executor is waiting on a foreign future.
    pub fn is_waiting(self) -> bool {
        self.0
            .try_borrow()
            .map(|state| state.pending.is_some())
            .unwrap_or(false)
    }

    /// Reset this `Executor` entirely and begins running the given thread.
    ///
    /// This is equivalent to creating a new executor with `Executor::run`.
    pub fn reset(self, mc: &Mutation<'gc>, thread: Thread<'gc>) -> Result<(), BadThreadMode> {
        let thread_mode = thread.mode();
        if matches!(thread_mode, ThreadMode::Waiting | ThreadMode::Running) {
            return Err(BadThreadMode {
                found: thread_mode,
                expected: Some(ThreadMode::Normal),
            });
        }
        let mut state = self.0.borrow_mut(mc);
        state.thread_stack.clear();
        state.thread_stack.push(thread);
        Ok(())
    }

    /// Reset this `Executor` entirely and begins running the given function, equivalent to
    /// creating a new executor with `Executor::start`.
    pub fn restart(
        self,
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) {
        let mut state = self.0.borrow_mut(&ctx);
        state.thread_stack.truncate(1);
        state.thread_stack[0].reset(&ctx).unwrap();
        state.thread_stack[0].start(ctx, function, args).unwrap();
    }
}

/// Execution state passed to callbacks when they are run by an `Executor`.
pub struct Execution<'gc, 'a> {
    executor: Executor<'gc>,
    fuel: &'a mut Fuel,
    threads: &'a [Thread<'gc>],
    // The live frame stack, borrowed for the duration of the native.
    lua_frames: &'a [Frame<'gc>],
}

impl<'gc, 'a> Execution<'gc, 'a> {
    pub fn reborrow(&mut self) -> Execution<'gc, '_> {
        Execution {
            executor: self.executor,
            fuel: self.fuel,
            threads: self.threads,
            lua_frames: self.lua_frames,
        }
    }

    /// The fuel parameter passed to `Executor::step`.
    pub fn fuel(&mut self) -> &mut Fuel {
        self.fuel
    }

    /// The curently executing Thread.
    pub fn current_thread(&self) -> CurrentThread<'gc> {
        CurrentThread {
            thread: *self.threads.last().unwrap(),
            is_main: self.threads.len() == 1,
        }
    }

    /// The curently running Executor.
    ///
    /// Do not call methods on this from callbacks! This is provided only for identification
    /// purposes, so that callbacks can identify which executor that is currently executing them, or
    /// to store the pointer somewhere.
    pub fn executor(&self) -> Executor<'gc> {
        self.executor
    }

    /// If the function we are returning to is Lua, returns information about the Lua frame we are
    /// returning to.
    pub fn upper_lua_frame(&self) -> Option<UpperLuaFrame<'gc>> {
        self.lua_frame_at(0)
    }

    /// The `level` steps above this native, counting *every* activation — 0 is whatever called it,
    /// Lua or native. Returns `None` when that activation is not a Lua function.
    ///
    /// This is the `level` argument of `error`, and counting natives is what makes it match
    /// PUC-Rio: `pcall(error, "x")` blames level 1, which is `pcall` itself, and a native has no
    /// source position — so the message is returned bare. Skipping natives instead would reach past
    /// `pcall` and wrongly attribute the error to the Lua code that called *it*.
    pub fn frame_at(&self, level: usize) -> Option<UpperLuaFrame<'gc>> {
        match self.lua_frames.iter().rev().nth(level) {
            Some(Frame::Lua {
                closure, pc, base, ..
            }) => self.describe(*closure, *pc, *base),
            _ => None,
        }
    }

    /// The Lua frame `level` steps above this native, skipping natives entirely: 0 is the nearest
    /// Lua function, 1 the next one out.
    ///
    /// This is what a traceback wants — a chain of source locations, with the natives in between
    /// left out because they have none. For blame, use [`Execution::frame_at`].
    pub fn lua_frame_at(&self, level: usize) -> Option<UpperLuaFrame<'gc>> {
        // Filtered on lookup rather than pre-collected: this is read by `error(msg, level)`,
        // `debug.getinfo` and `debug.traceback` only, so paying per lookup beats paying per native
        // call — which is what snapshotting the chain up front cost.
        let Some((closure, pc, base)) = self
            .lua_frames
            .iter()
            .rev()
            .filter_map(|f| match f {
                Frame::Lua {
                    closure, pc, base, ..
                } => Some((*closure, *pc, *base)),
                _ => None,
            })
            .nth(level)
        else {
            return None;
        };
        self.describe(closure, pc, base)
    }

    fn describe(
        &self,
        closure: Closure<'gc>,
        pc: usize,
        base: usize,
    ) -> Option<UpperLuaFrame<'gc>> {
        let proto = closure.prototype();
        // Attribute to the Call that invoked this native. Whether the frame's `pc` still points at
        // that Call or has already moved past it depends on the call site, so look rather than
        // assume an offset: step back one if that is where the call actually is.
        let is_call = |i: usize| {
            proto.opcodes.get(i).is_some_and(|op| {
                matches!(
                    op.decode(),
                    crate::opcode::Operation::Call { .. }
                        | crate::opcode::Operation::TailCall { .. }
                )
            })
        };
        let call_opcode = match pc.checked_sub(1) {
            Some(prev) if is_call(prev) => prev,
            _ => pc,
        };

        Some(UpperLuaFrame {
            closure,
            chunk_name: proto.chunk_name,
            current_function: proto.reference,
            current_line: match proto
                .opcode_line_numbers
                .binary_search_by_key(&call_opcode, |(opi, _)| *opi)
            {
                Ok(i) => proto.opcode_line_numbers[i].1,
                Err(i) => proto.opcode_line_numbers[i - 1].1,
            },
            base,
            pc,
        })
    }
}

pub struct CurrentThread<'gc> {
    pub thread: Thread<'gc>,
    pub is_main: bool,
}

pub struct UpperLuaFrame<'gc> {
    pub closure: Closure<'gc>,
    pub chunk_name: String<'gc>,
    pub current_function: FunctionRef<String<'gc>>,
    pub current_line: LineNumber,
    /// Where this frame's registers start on the thread stack, so a register index can be read.
    pub base: usize,
    /// The instruction about to run. A local's name is only meaningful together with a pc, since
    /// the allocator reuses registers between blocks.
    pub pc: usize,
}

#[cfg(test)]
mod activation_tests {
    use super::*;
    use crate::{Callback, Lua, Value};

    #[derive(Debug, PartialEq, Eq)]
    struct Slice {
        done: bool,
        mode: ExecutorMode,
        fuel: i32,
        interrupted: bool,
        frames: Vec<Vec<std::string::String>>,
        stacks: Vec<Vec<std::string::String>>,
        dispatches: u64,
    }

    fn value(value: Value<'_>) -> std::string::String {
        match value {
            Value::Number(number) => format!("number:{:016x}", number.to_bits()),
            Value::String(string) => format!("string:{:?}", string.as_bytes()),
            Value::Nil | Value::Boolean(_) | Value::Integer(_) => {
                format!("{}:{}", value.type_name(), value.display())
            }
            _ => value.type_name().to_owned(),
        }
    }

    fn run(
        source: &str,
        budget: i32,
        native: bool,
    ) -> (Vec<Slice>, Result<i64, std::string::String>) {
        run_mode(source, budget, native, false)
    }

    fn run_mode(
        source: &str,
        budget: i32,
        native: bool,
        kernels: bool,
    ) -> (Vec<Slice>, Result<i64, std::string::String>) {
        run_mode_checked(source, budget, native, kernels, None)
    }

    fn run_mode_checked(
        source: &str,
        budget: i32,
        native: bool,
        kernels: bool,
        activations: Option<u64>,
    ) -> (Vec<Slice>, Result<i64, std::string::String>) {
        run_mode_checked_kernel(source, budget, native, kernels, activations, false)
    }

    fn run_mode_checked_kernel(
        source: &str,
        budget: i32,
        native: bool,
        kernels: bool,
        activations: Option<u64>,
        integer: bool,
    ) -> (Vec<Slice>, Result<i64, std::string::String>) {
        let mut lua = Lua::core();
        lua.load_debug();
        let _ = (native, kernels, activations, integer);
        #[cfg(feature = "jit")]
        let native = native && !cfg!(miri) && lua.jit_capabilities().supported_target;
        #[cfg(feature = "jit")]
        lua.set_jit_config(crate::JitConfig {
            mode: if native {
                crate::JitMode::Auto
            } else {
                crate::JitMode::Off
            },
            hot_threshold: u32::MAX,
            ..Default::default()
        })
        .unwrap();
        let executor = lua.enter(|ctx| {
            #[cfg(all(feature = "jit", not(miri)))]
            if integer {
                ctx.jit().test_integer_activations(native && kernels);
            } else {
                ctx.jit().test_scalar_kernels(native && kernels);
            }
            ctx.set_global(
                "nested_boundary",
                Callback::from_fn(&ctx, |ctx, _, mut stack| {
                    let function = ctx.get_global::<Function>("read")?;
                    let nested = Executor::start(ctx, function, ());
                    while !nested.step(ctx, &mut Fuel::with(4096)).unwrap() {}
                    let number = nested.take_result::<i64>(ctx).unwrap()?;
                    stack.replace(ctx, number + 1);
                    Ok(CallbackReturn::Return)
                }),
            );
            ctx.set_global(
                "boundary",
                Callback::from_fn(&ctx, |ctx, mut execution, mut stack| {
                    let number = stack.consume::<i64>(ctx)?;
                    execution.fuel().interrupt();
                    stack.replace(ctx, number + 1);
                    Ok(CallbackReturn::Return)
                }),
            );
            let closure = Closure::load(ctx, Some("lua-batch"), source.as_bytes()).unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        #[cfg(feature = "jit")]
        if native {
            while lua.prepare_jit().unwrap() != 0 {}
        }
        let mut trace = Vec::new();
        for _ in 0..10_000 {
            let slice = lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                let mut fuel = Fuel::with(budget);
                let done = executor.step(ctx, &mut fuel).unwrap();
                let state = executor.0.borrow();
                let mut frames = Vec::new();
                let mut stacks = Vec::new();
                for thread in &state.thread_stack {
                    let thread = thread.into_inner().borrow();
                    frames.push(
                        thread
                            .frames
                            .iter()
                            .map(|frame| match frame {
                                Frame::Lua {
                                    pc,
                                    base,
                                    bottom,
                                    stack_size,
                                    is_variable,
                                    ..
                                } => {
                                    format!("lua:{pc}:{base}:{bottom}:{stack_size}:{is_variable}")
                                }
                                Frame::Callback { bottom, .. } => format!("callback:{bottom}"),
                                Frame::Sequence {
                                    bottom,
                                    pending_error,
                                    ..
                                } => {
                                    format!("sequence:{bottom}:{}", pending_error.is_some())
                                }
                                Frame::Close {
                                    bottom,
                                    pending_error,
                                    ..
                                } => {
                                    format!("close:{bottom}:{}", pending_error.is_some())
                                }
                                Frame::Start(_) => "start".to_owned(),
                                Frame::Yielded => "yielded".to_owned(),
                                Frame::WaitThread => "wait".to_owned(),
                                Frame::Result { bottom } => format!("result:{bottom}"),
                                Frame::Error(error) => format!("error:{error}"),
                            })
                            .collect(),
                    );
                    stacks.push(thread.stack.borrow().iter().copied().map(value).collect());
                }
                #[cfg(feature = "jit")]
                let dispatches = ctx.jit().0.borrow().stats.total_dispatches;
                #[cfg(not(feature = "jit"))]
                let dispatches = 0;
                Slice {
                    done,
                    mode: executor.mode(),
                    fuel: fuel.remaining(),
                    interrupted: fuel.is_interrupted(),
                    frames,
                    stacks,
                    dispatches,
                }
            });
            let done = slice.done;
            trace.push(slice);
            lua.gc_collect();
            if done {
                #[cfg(all(feature = "jit", not(miri)))]
                if let Some(expected) = activations {
                    assert_eq!(
                        lua.enter(|ctx| ctx.jit().test_scalar_activation_entries()),
                        expected,
                        "{source}, {budget}"
                    );
                }
                let result = lua.enter(|ctx| {
                    ctx.fetch(&executor)
                        .take_result::<i64>(ctx)
                        .unwrap()
                        .map_err(|error| format!("{error}"))
                });
                return (trace, result);
            }
        }
        panic!("Lua activation batch did not finish");
    }

    #[test]
    fn native_and_interpreted_activations_have_identical_canonical_slices() {
        compare_canonical_slices(false);
    }

    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn scalar_kernels_have_identical_canonical_slices() {
        compare_canonical_slices(true);
    }

    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn direct_scalar_activations_preserve_canonical_slices() {
        for (source, expected, activations) in [
            ("local n=0 local function f(x) n=n+x end for i=1,100 do f(1) end return n", 100, 100),
            ("local n=0 local function f() n=n+1 end for i=1,100 do f() end return n", 100, 100),
            ("local n=100 local function f(x) n=n-x end for i=1,100 do f(1) end return n", 0, 100),
            ("local n=1 local function f(x) n=n*x end for i=1,4 do f(2) end return n", 16, 4),
            ("local n=1 local function f() n=n+n end for i=1,4 do f() end return n", 16, 0),
            ("local n=math.maxinteger local function f(x) n=n+x end f(1) return n==math.mininteger and 100 or 0", 100, 1),
            ("local n=math.mininteger local function f(x) n=n-x end f(1) return n==math.maxinteger and 100 or 0", 100, 1),
            ("local n=4611686018427387904 local function f(x) n=n*x end f(4) return n", 0, 1),
            ("local n=0 local function f(x) n=n+x end for i=1,100 do f(0.5) end return n", 50, 0),
            ("local function make() local n=0 return function(x) n=n+x return n end end local f=make() local n=0 for i=1,100 do n=f(1) end return n", 100, 0),
            ("local n=1 local function f(x) n=n+x end local x=setmetatable({}, {__add=function() return {} end}) f(x) return type(n)=='table' and 100 or 0", 100, 0),
            ("local n=0 local function f(x,t) n=n+x return t end local t={} for i=1,100 do assert(f(1,t)==t) end return n", 100, 0),
            ("local n=0 local function f(x,t) n=n+x end local t={x=1} for i=1,100 do f(1,t) assert(t.x==1) end return n", 100, 100),
            ("local n=0 local function f(x) n=n+x end local function g(x) return f(x) end for i=1,100 do g(1) end return n", 100, 100),
            ("local n=0 local function f(x) n=n+x end local co=coroutine.create(function() for i=1,100 do f(1) if i%10==0 then coroutine.yield() end end end) while coroutine.status(co)~='dead' do assert(coroutine.resume(co)) end return n", 100, 0),
            ("local ticks=0 debug.sethook(function() local x=0 for i=1,2 do x=x+i end ticks=ticks+x end,'',9) local n=0 local function f(x) n=n+x end for i=1,100 do f(1) end debug.sethook() return n", 100, 0),
        ] {
            for budget in [0, 1, 17, 4096] {
                let reference = run_mode(source, budget, false, false);
                assert_eq!(reference.1, Ok(expected));
                assert_eq!(
                    reference,
                    run_mode_checked(source, budget, true, true, Some(activations)),
                    "{source}, {budget}"
                );
                assert_eq!(
                    reference,
                    run_mode_checked_kernel(source, budget, true, true, Some(activations), true),
                    "integer {source}, {budget}"
                );
            }
        }
    }

    #[test]
    fn scalar_activation_refusals_match_compiled_shapes() {
        let mut lua = Lua::empty();
        for source in [
            "local n=1 local function f() n=n+n end return f",
            "local n=0 local function f(x,t) n=n+x return t end return f",
        ] {
            lua.enter(|ctx| {
                let closure = Closure::load(ctx, None, source.as_bytes()).unwrap();
                let prototype = closure.prototype().prototypes[0];
                let operations: Vec<_> = prototype.opcodes.iter().map(|op| op.decode()).collect();
                assert!(operations.len() > 4, "{source}: {operations:?}");
            });
        }
    }

    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn scalar_activation_executes_without_upvalue_helpers() {
        let mut lua = Lua::empty();
        lua.set_jit_config(crate::JitConfig {
            mode: crate::JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        let executor = lua.enter(|ctx| {
            ctx.jit().test_scalar_kernels(true);
            let closure = Closure::load(
                ctx,
                None,
                b"local sum=1 local function add(v) sum=sum+v end add(2) return sum",
            )
            .unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        assert_eq!(lua.prepare_jit().unwrap(), 2);
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 3);
        let stats = lua.jit_stats();
        assert_eq!(stats.native_upvalue_reads, 1);
        assert_eq!(stats.native_upvalue_writes, 1);
        assert!(stats.helper_calls < 2, "{stats:?}");
        assert_eq!(
            lua.enter(|ctx| ctx.jit().test_scalar_activation_entries()),
            1
        );
    }

    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn integer_activations_execute_without_upvalue_helpers() {
        for (initial, operation, argument, expected) in [
            (1, "+", 2, 3),
            (1, "-", 2, -1),
            (3, "*", 2, 6),
            (i64::MAX, "+", 1, i64::MIN),
            (i64::MIN, "-", 1, i64::MAX),
            (i64::MAX, "*", 2, -2),
        ] {
            let initial = if initial == i64::MIN {
                "(-9223372036854775807-1)".to_owned()
            } else {
                initial.to_string()
            };
            let source = format!(
                "local sum={initial} local function add(v) sum=sum{operation}v end add({argument}) return sum"
            );
            let mut lua = Lua::empty();
            lua.set_jit_config(crate::JitConfig {
                mode: crate::JitMode::Auto,
                hot_threshold: 1,
                ..Default::default()
            })
            .unwrap();
            let executor = lua.enter(|ctx| {
                ctx.jit().test_integer_activations(true);
                let closure = Closure::load(ctx, None, source.as_bytes()).unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            });
            assert_eq!(lua.prepare_jit().unwrap(), 2);
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), expected);
            let stats = lua.jit_stats();
            assert_eq!(stats.native_upvalue_reads, 1);
            assert_eq!(stats.native_upvalue_writes, 1);
            assert!(stats.helper_calls < 2, "{stats:?}");
            assert_eq!(
                lua.enter(|ctx| ctx.jit().test_scalar_activation_entries()),
                1
            );
        }
    }

    fn compare_canonical_slices(kernels: bool) {
        for source in [
            "local function f(x) return x+1 end local n=0 for i=1,100 do n=f(n) end return n",
            "local function f() end local n=0 for i=1,100 do f() n=n+1 end return n",
            "local function f(...) return ... end local function g(...) return f(...) end local n=0 for i=1,100 do n=g(n+1) end return n",
            "local n=0 local function f(x) n=n+x end for i=1,100 do f(1) end return n",
            "local function f(n) if n==0 then return 100 end return f(n-1) end return f(100)",
            "local n=0 for i=1,100 do local ok=pcall(function() return i.x end) if not ok then n=n+1 end end return n",
            "local t=coroutine.create(function() coroutine.yield(1) return 100 end) assert(coroutine.resume(t)) local ok,n=coroutine.resume(t) assert(ok) return n",
            "local n=0 local function f() local x <close> = setmetatable({}, {__close=function() n=n+1 end}) end for i=1,100 do f() end return n",
            "local n=0 debug.sethook(function() n=n+1 end,'',9) local function f(x) return x+1 end local result=0 for i=1,100 do result=f(result) end debug.sethook() return result",
            "local function f() return 1+{} end return f()",
        ] {
            for budget in [0, 1, 17, 4096] {
                let reference = run_mode(source, budget, false, kernels);
                if source.contains("1+{}") {
                    assert!(reference.1.is_err());
                } else {
                    assert_eq!(reference.1, Ok(100));
                }
                assert_eq!(reference, run_mode(source, budget, true, kernels), "{source}, {budget}");
            }
        }
    }

    #[test]
    fn native_and_interpreted_callbacks_match_interrupted_slices() {
        for budget in [1, 17, 4096] {
            let source = "local n=0 for i=1,100 do n=boundary(n) end return n";
            let reference = run(source, budget, false);
            assert_eq!(reference.1, Ok(100));
            assert!(reference.0.iter().any(|slice| slice.interrupted));
            assert_eq!(reference, run(source, budget, true));
        }
    }

    #[test]
    fn native_and_interpreted_nested_executors_preserve_open_upvalues() {
        let source =
            "local n=96 read=function() return n end for i=1,4 do n=nested_boundary() end return n";
        let reference = run(source, 4096, false);
        assert_eq!(reference.1, Ok(100));
        assert_eq!(reference, run(source, 4096, true));
    }
}
