use luna::{Closure, Executor, ExecutorMode, ExternError, Fuel};

#[test]
fn exhausted_fuel_runs_one_bounded_slice() -> Result<(), ExternError> {
    for initial in [0, -1, 1, 63, 64] {
        let mut lua = common::core();
        let executor = lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, None, b"while true do end")?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        lua.enter(|ctx| {
            let mut fuel = Fuel::with(initial);
            let executor = ctx.fetch(&executor);
            assert!(!executor.step(ctx, &mut fuel).unwrap());
            assert_eq!(fuel.remaining(), initial - 68);
            assert_eq!(executor.mode(), ExecutorMode::Normal);
        });
    }
    Ok(())
}

#[test]
fn return_transition_charges_call_and_result_work() -> Result<(), ExternError> {
    let mut lua = common::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42")?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.enter(|ctx| {
        let mut fuel = Fuel::empty();
        let executor = ctx.fetch(&executor);
        assert!(!executor.step(ctx, &mut fuel).unwrap());
        assert_eq!(fuel.remaining(), -10);
        assert_eq!(executor.mode(), ExecutorMode::Result);
        assert!(executor.step(ctx, &mut fuel).unwrap());
        assert_eq!(fuel.remaining(), -10);
        assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 42);
    });
    Ok(())
}

#[test]
fn preexisting_interrupt_preserves_minimal_progress() -> Result<(), ExternError> {
    let mut lua = common::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, b"while true do end")?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.enter(|ctx| {
        let mut fuel = Fuel::with(1000);
        fuel.interrupt();
        assert!(!ctx.fetch(&executor).step(ctx, &mut fuel).unwrap());
        assert_eq!(fuel.remaining(), 932);
        assert!(fuel.is_interrupted());
        fuel.refill(100, 1000);
        assert_eq!(fuel.remaining(), 1000);
        assert!(!fuel.is_interrupted());
    });
    Ok(())
}

mod common;
