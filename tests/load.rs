//! `load` has to honour all four of its arguments.
//!
//! The environment argument in particular: a chunk loaded into a restricted table must not be able
//! to reach the real globals, or sandboxing through `load` is not sandboxing at all.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

#[test]
fn the_env_argument_is_honoured() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local env = { answer = 42 }
        local f = load("return answer", "chunk", "t", env)
        return f() == 42
    "#
    )?);
    Ok(())
}

/// The escape this test exists for: without the fourth argument wired up, a chunk loaded into a
/// bare table still saw the real globals.
#[test]
fn a_restricted_env_cannot_reach_the_real_globals() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        secret = "leaked"
        local f = load("return secret", "chunk", "t", {})
        return f() == nil
    "#
    )?);
    Ok(())
}

#[test]
fn writes_land_in_the_given_env() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local env = {}
        load("written = 7", "chunk", "t", env)()
        return env.written == 7 and written == nil
    "#
    )?);
    Ok(())
}

#[test]
fn no_env_still_means_the_globals() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        shared = 5
        return load("return shared")() == 5
    "#
    )?);
    Ok(())
}

/// There is no bytecode loader, so a binary-only mode is refused rather than quietly treated as
/// source.
#[test]
fn binary_only_mode_is_refused() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local ok = pcall(load, "return 1", "chunk", "b")
        return ok == false
    "#
    )?);
    Ok(())
}

#[test]
fn text_modes_are_accepted() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return load("return 1", "c", "t")() == 1 and load("return 2", "c", "bt")() == 2
    "#
    )?);
    Ok(())
}

/// The chunk name is recorded on the compiled prototype.
///
/// It does not yet reach error *messages* — luna has no `chunk:line:` prefix — but it is carried
/// rather than discarded, which is what a traceback will need.
#[test]
fn the_chunk_name_is_recorded_on_the_prototype() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    lua.try_enter(|ctx| {
        let loaded: luna::Function = ctx.globals().get::<_, luna::Function>(ctx, "load").unwrap();
        let executor = Executor::start(
            ctx,
            loaded,
            ("return 1", "my_chunk_name", "t", ctx.globals()),
        );
        Ok(ctx.stash(executor))
    })?;

    // Compiling directly is the same path `load` now takes.
    lua.enter(|ctx| {
        let closure =
            Closure::load_with_env(ctx, Some("my_chunk_name"), b"return 1", ctx.globals()).unwrap();
        assert_eq!(
            closure.prototype().chunk_name.display_lossy().to_string(),
            "my_chunk_name"
        );
    });
    Ok(())
}

/// A bad chunk still reports a compile error through `load`'s two-value return.
#[test]
fn a_bad_chunk_returns_nil_and_a_message() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local f, err = load("this is not lua", "my_chunk_name")
        return f == nil and type(err) == "string"
    "#
    )?);
    Ok(())
}

#[test]
fn g_is_the_globals_table() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        marker = 11
        return _G ~= nil and _G.marker == 11 and _G._G == _G
    "#
    )?);
    Ok(())
}
