use super::*;

#[test]
fn chained_callbacks_preserve_fuel_boundaries_and_callback_visible_state() {
    use std::{cell::Cell, rc::Rc};
    for budget in [i32::MIN, -1, 0, 1, 11, 12, 13, 23, 24, 25, i32::MAX] {
        for interrupted in [false, true] {
            for interrupt_first in [false, true] {
                crate::Lua::empty().enter(|ctx| {
                    let calls = Rc::new(Cell::new(0));
                    let observed = calls.clone();
                    let second =
                        crate::Callback::from_fn(&ctx, move |ctx, execution, mut stack| {
                            let thread = execution.current_thread().thread;
                            assert_eq!(thread.mode(), ThreadMode::Running);
                            let state = thread.into_inner().borrow();
                            assert!(state.frames.is_empty());
                            assert!(state.stack.try_borrow_mut(&ctx).is_ok());
                            assert_eq!(stack.consume::<i64>(ctx)?, 8);
                            observed.set(observed.get() + 1);
                            stack.replace(ctx, 9);
                            Ok(CallbackReturn::Return)
                        });
                    let observed = calls.clone();
                    let first = crate::Callback::from_fn_with(
                        &ctx,
                        second,
                        move |second, ctx, execution, mut stack| {
                            let thread = execution.current_thread().thread;
                            assert_eq!(thread.mode(), ThreadMode::Running);
                            assert!(thread.into_inner().borrow().frames.is_empty());
                            assert_eq!(stack.consume::<i64>(ctx)?, 7);
                            observed.set(observed.get() + 1);
                            stack.replace(ctx, 8);
                            if interrupt_first {
                                execution.fuel.interrupt();
                            }
                            Ok(CallbackReturn::Call {
                                function: (*second).into(),
                                then: None,
                            })
                        },
                    );
                    let executor = Executor::start(ctx, first.into(), 7);
                    let mut fuel = Fuel::with(budget);
                    if interrupted {
                        fuel.interrupt();
                    }
                    let blocked = interrupted || interrupt_first;
                    let both = budget > 12 && !blocked;
                    assert_eq!(
                        executor.step(ctx, &mut fuel).unwrap(),
                        budget > 24 && !blocked
                    );
                    assert_eq!(calls.get(), if both { 2 } else { 1 });
                    assert_eq!(
                        fuel.remaining(),
                        budget.saturating_sub(if both { 24 } else { 12 })
                    );
                    assert_eq!(fuel.is_interrupted(), blocked);
                    if !both {
                        let thread = executor.0.borrow().thread_stack[0];
                        let state = thread.into_inner().borrow();
                        assert!(matches!(
                            state.frames.as_slice(),
                            [Frame::Callback { bottom: 0, .. }]
                        ));
                        drop(state);
                        let mut next = Fuel::empty();
                        assert!(!executor.step(ctx, &mut next).unwrap());
                        assert_eq!(next.remaining(), -12);
                        assert_eq!(calls.get(), 2);
                    }
                    assert_eq!(executor.mode(), ExecutorMode::Result);
                    assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 9);
                });
            }
        }
    }
}
