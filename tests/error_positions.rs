//! Errors carry `chunk:line:`, and `error` honours its `level` argument.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<String, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<String>(&executor)
}

#[test]
fn error_prefixes_the_calling_line() -> Result<(), ExternError> {
    // `error` is on line 3 of the chunk.
    assert_eq!(
        eval("\nlocal _, e = pcall(function()\n  error('boom')\nend)\nreturn tostring(e)")?,
        "probe:3: boom"
    );
    Ok(())
}

/// Level 2 blames the caller — the idiom every argument-checking function uses.
#[test]
fn level_two_blames_the_caller() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            "local function check() error('bad argument', 2) end\n\
             local _, e = pcall(function()\n  check()\nend)\nreturn tostring(e)"
        )?,
        "probe:3: bad argument"
    );
    Ok(())
}

#[test]
fn level_zero_adds_no_position() -> Result<(), ExternError> {
    assert_eq!(
        eval("local _, e = pcall(function() error('raw', 0) end)\nreturn tostring(e)")?,
        "raw"
    );
    Ok(())
}

/// A non-string error value is passed through untouched, position or not.
#[test]
fn non_string_errors_are_untouched() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            "local _, e = pcall(function() error({ code = 7 }) end)\n\
             return type(e) .. ':' .. tostring(e.code)"
        )?,
        "table:7"
    );
    Ok(())
}

#[test]
fn runtime_errors_carry_the_faulting_line() -> Result<(), ExternError> {
    assert_eq!(
        eval("local _, e = pcall(function()\n  local z = nil\n  return z.field\nend)\nreturn tostring(e)")?,
        "probe:3: could not index into a nil value"
    );
    Ok(())
}

/// The message used to render as the useless "operator error".
#[test]
fn arithmetic_errors_say_what_went_wrong() -> Result<(), ExternError> {
    let msg = eval("local _, e = pcall(function()\n  return 1 + {}\nend)\nreturn tostring(e)")?;
    assert!(
        msg.starts_with("probe:2:") && msg.contains("add"),
        "got {msg:?}"
    );
    Ok(())
}

/// `error` called *by a native* has no source position to blame.
///
/// PUC-Rio resolves `level` against every activation, not just the Lua ones, so for
/// `pcall(error, "x")` level 1 is `pcall` itself — a C function, with no position — and the message
/// comes back bare. Walking only Lua frames reached past `pcall` and blamed the line that called
/// it, which is the wrong function entirely.
#[test]
fn a_native_caller_contributes_no_position() -> Result<(), ExternError> {
    assert_eq!(eval(r#"return select(2, pcall(error, "x"))"#)?, "x");
    // The same message raised from Lua *is* positioned, so this is about the caller, not the level.
    assert_eq!(
        eval("local _, e = pcall(function()\n  error('x')\nend)\nreturn e")?,
        "probe:2: x"
    );
    Ok(())
}
