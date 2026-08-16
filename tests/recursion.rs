//! Deep recursion is a feature of a stackless VM; unbounded recursion is an accident.

use luna::{Closure, Executor, ExternError, Lua};

fn run(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

/// Far past the ~200 levels PUC-Rio manages, and expected to keep working.
#[test]
fn deep_recursion_still_works() -> Result<(), ExternError> {
    assert!(run(r#"
        local function count(n)
            if n == 0 then return 0 end
            return 1 + count(n - 1)
        end
        return count(10000) == 10000
    "#)?);
    Ok(())
}

/// Runaway recursion raises an ordinary error rather than running the machine out of memory,
/// and `pcall` can catch it.
#[test]
fn runaway_recursion_is_catchable() -> Result<(), ExternError> {
    assert!(run(r#"
        local function forever() return 1 + forever() end
        local ok, err = pcall(forever)
        return ok == false and err ~= nil
    "#)?);
    Ok(())
}

/// The ceiling applies to a coroutine as well as the main thread.
#[test]
fn runaway_recursion_in_a_coroutine_is_catchable() -> Result<(), ExternError> {
    assert!(run(r#"
        local function forever() return 1 + forever() end
        local co = coroutine.create(forever)
        local ok, err = coroutine.resume(co)
        return ok == false and err ~= nil
    "#)?);
    Ok(())
}

/// A lowered ceiling takes effect for threads created afterwards.
#[test]
fn the_ceiling_is_configurable() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    lua.enter(|ctx| ctx.set_max_call_depth(64));

    let executor = lua.try_enter(|ctx| {
        assert_eq!(ctx.max_call_depth(), 64);
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                local function count(n)
                    if n == 0 then return 0 end
                    return 1 + count(n - 1)
                end
                local ok = pcall(count, 1000)
                return ok == false
            "#[..],
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}
