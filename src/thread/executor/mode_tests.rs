use super::*;
use crate::{Callback, Lua, Value};
use std::{cell::Cell, rc::Rc};

#[derive(Collect)]
#[collect(require_static)]
struct ReturnSequence;

impl<'gc> Sequence<'gc> for ReturnSequence {
    fn poll(
        self: Pin<&mut Self>,
        _: Context<'gc>,
        _: Execution<'gc, '_>,
        _: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        Ok(SequencePoll::Return)
    }
}

fn completed_child<'gc>(ctx: Context<'gc>, error: bool) -> Thread<'gc> {
    let child = Thread::new(ctx);
    let mut state = child.into_inner().borrow_mut(&ctx);
    if error {
        state.frames.push(Frame::Error(
            crate::RuntimeError::new(anyhow::anyhow!("child-result-error")).into(),
        ));
    } else {
        state.stack.borrow_mut(&ctx).push(Value::Integer(42));
        state.frames.push(Frame::Result { bottom: 0 });
    }
    drop(state);
    assert_eq!(child.mode(), ThreadMode::Result);
    child
}

#[test]
fn ordinary_callback_preserves_minimum_progress_interrupts_and_fuel() {
    Lua::empty().enter(|ctx| {
        for budget in [i32::MIN, -1, 0, 1, 12, 13, i32::MAX] {
            for interrupted in [false, true] {
                let calls = Rc::new(Cell::new(0));
                let observed = calls.clone();
                let callback = Callback::from_fn(&ctx, move |ctx, execution, mut stack| {
                    assert_eq!(execution.executor.mode(), ExecutorMode::Running);
                    assert_eq!(
                        execution.threads.last().unwrap().mode(),
                        ThreadMode::Running
                    );
                    observed.set(observed.get() + 1);
                    stack.replace(ctx, 42);
                    Ok(CallbackReturn::Return)
                });
                let executor = Executor::start(ctx, callback.into(), ());
                let mut fuel = Fuel::with(budget);
                if interrupted {
                    fuel.interrupt();
                }
                assert_eq!(
                    executor.step(ctx, &mut fuel).unwrap(),
                    budget > 12 && !interrupted
                );
                assert_eq!(calls.get(), 1);
                assert_eq!(fuel.remaining(), budget.saturating_sub(12));
                assert_eq!(fuel.is_interrupted(), interrupted);
                assert_eq!(executor.mode(), ExecutorMode::Result);
                let mut empty = Fuel::empty();
                assert!(executor.step(ctx, &mut empty).unwrap());
                assert_eq!(empty.remaining(), 0);
                assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 42);
                assert_eq!(executor.mode(), ExecutorMode::Stopped);
            }
        }
    });
}

#[test]
fn child_result_handoff_rechecks_parent_mode_before_dispatch() {
    Lua::empty().enter(|ctx| {
        for (sequence, error) in [(false, false), (true, false), (false, true)] {
            for budget in [0, 1, 4, 8, 9, i32::MAX] {
                for interrupted in [false, true] {
                    let executor = Executor::new(ctx);
                    let parent = executor.0.borrow().thread_stack[0];
                    {
                        let mut state = parent.into_inner().borrow_mut(&ctx);
                        if sequence {
                            state.frames.push(Frame::Sequence {
                                bottom: 0,
                                sequence: BoxSequence::new(&ctx, ReturnSequence),
                                pending_error: None,
                            });
                        }
                        state.frames.push(Frame::WaitThread);
                    }
                    assert_eq!(parent.mode(), ThreadMode::Waiting);
                    executor
                        .0
                        .borrow_mut(&ctx)
                        .thread_stack
                        .push(completed_child(ctx, error));
                    let mut fuel = Fuel::with(budget);
                    if interrupted {
                        fuel.interrupt();
                    }
                    let consumed = if sequence { 8 } else { 4 };
                    assert_eq!(
                        executor.step(ctx, &mut fuel).unwrap(),
                        budget > consumed && !interrupted
                    );
                    assert_eq!(fuel.remaining(), budget - consumed);
                    assert_eq!(fuel.is_interrupted(), interrupted);
                    assert_eq!(executor.0.borrow().thread_stack.len(), 1);
                    assert_eq!(parent.mode(), ThreadMode::Result);
                    let result = executor.take_result::<i64>(ctx).unwrap();
                    if error {
                        assert!(result
                            .unwrap_err()
                            .to_string()
                            .contains("child-result-error"));
                    } else {
                        assert_eq!(result.unwrap(), 42);
                    }
                }
            }
        }
    });
}

#[test]
fn completed_child_still_rejects_nonwaiting_parents_without_dispatch() {
    Lua::empty().enter(|ctx| {
        for expected in [
            ThreadMode::Stopped,
            ThreadMode::Suspended,
            ThreadMode::Result,
            ThreadMode::Normal,
        ] {
            let executor = Executor::new(ctx);
            let parent = executor.0.borrow().thread_stack[0];
            {
                let mut state = parent.into_inner().borrow_mut(&ctx);
                match expected {
                    ThreadMode::Stopped => {}
                    ThreadMode::Suspended => state.frames.push(Frame::Yielded),
                    ThreadMode::Result => state.frames.push(Frame::Result { bottom: 0 }),
                    ThreadMode::Normal => state.frames.push(Frame::Callback {
                        bottom: 0,
                        callback: Callback::from_fn(&ctx, |_, _, _| {
                            panic!("invalid parent dispatched")
                        }),
                    }),
                    _ => unreachable!(),
                }
            }
            executor
                .0
                .borrow_mut(&ctx)
                .thread_stack
                .push(completed_child(ctx, false));
            let mut fuel = Fuel::with(100);
            let failure = executor.step(ctx, &mut fuel).unwrap_err();
            assert_eq!(failure.found, expected);
            assert_eq!(failure.expected, Some(ThreadMode::Waiting));
            assert_eq!(fuel.remaining(), 100);
        }
    });
}
