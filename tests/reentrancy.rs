//! A Rust callback that drives a nested `Executor` while the outer thread is still running.
//!
//! This is how an embedder calls a Lua function from several Rust frames down, where there is no
//! continuation to hand back as `CallbackReturn::Call`.

use luna::{
    Callback, CallbackReturn, Closure, Context, Executor, ExternError, Function, Lua, Value,
};

/// Run `f` on a nested executor, to completion, from inside a callback.
fn call_nested<'gc>(ctx: Context<'gc>, f: Function<'gc>) -> Result<Value<'gc>, luna::Error<'gc>> {
    let executor = Executor::start(ctx, f, ());
    loop {
        let mut fuel = luna::Fuel::with(i32::MAX);
        if executor.step(ctx, &mut fuel).unwrap() {
            break;
        }
    }
    executor.take_result::<Value>(ctx).unwrap()
}

/// The callee captures nothing, so no upvalue of the running thread is read.
#[test]
fn nested_call_without_upvalues() -> Result<(), ExternError> {
    let mut lua = Lua::core();

    lua.try_enter(|ctx| {
        let call = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let f: Function = stack.consume(ctx)?;
            let v = call_nested(ctx, f)?;
            stack.replace(ctx, v);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("call", call);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                return call(function() return 42 end)
            "#[..],
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    Ok(())
}

/// The callee reads an upvalue belonging to the still-running outer thread.
///
/// This is the shape every real embedder hits, because a callback registered from a config closes
/// over that config's own locals.
#[test]
fn nested_call_reading_an_upvalue_of_the_running_thread() -> Result<(), ExternError> {
    let mut lua = Lua::core();

    lua.try_enter(|ctx| {
        let call = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let f: Function = stack.consume(ctx)?;
            let v = call_nested(ctx, f)?;
            stack.replace(ctx, v);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("call", call);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                local captured = 41
                -- deliberately not a tail call: the outer frame must still be live, and so
                -- must its open upvalue, while the callback runs.
                local result = call(function() return captured + 1 end)
                return result
            "#[..],
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    Ok(())
}

/// The same, writing through the upvalue rather than reading it.
#[test]
fn nested_call_writing_an_upvalue_of_the_running_thread() -> Result<(), ExternError> {
    let mut lua = Lua::core();

    lua.try_enter(|ctx| {
        let call = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let f: Function = stack.consume(ctx)?;
            let v = call_nested(ctx, f)?;
            stack.replace(ctx, v);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("call", call);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                local captured = 0
                call(function() captured = 42 end)
                return captured
            "#[..],
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    Ok(())
}
