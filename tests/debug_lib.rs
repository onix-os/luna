//! The `debug` library. `sethook`/`getlocal` are deliberately absent — see the module docs.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<String, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<String>(&executor)
}

#[test]
fn traceback_walks_the_whole_chain() -> Result<(), ExternError> {
    // Deliberately not tail calls: a tail call replaces its frame, so the chain would be gone —
    // which is correct Lua behaviour, not a traceback bug.
    let tb = eval(
        "local function inner() local t = debug.traceback('msg') return t end\n\
         local function outer() local t = inner() return t end\n\
         local t = outer() return t",
    )?;
    // Message first, then one line per Lua frame: inner, outer, the main chunk.
    assert!(tb.starts_with("msg\nstack traceback:"), "got {tb:?}");
    assert_eq!(tb.matches("probe:").count(), 3, "got {tb:?}");
    Ok(())
}

/// A non-string message passes through, so a traceback handler works with error objects.
#[test]
fn traceback_passes_non_strings_through() -> Result<(), ExternError> {
    assert_eq!(
        eval("local t = debug.traceback({ code = 3 })\nreturn type(t) .. ':' .. tostring(t.code)")?,
        "table:3"
    );
    Ok(())
}

/// `currentline` is informational: it can land one line early depending on whether the call's
/// result is used. `error(msg, level)` is the exact one, and is tested in `error_positions`.
#[test]
fn getinfo_describes_a_level() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            "local function here()\n  local i = debug.getinfo(1)\n  \
             return i.short_src .. '|' .. i.what .. '|' .. tostring(i.currentline)\nend\n\
             return here()"
        )?,
        "probe|Lua|1"
    );
    Ok(())
}

#[test]
fn getinfo_describes_a_function_directly() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            "local function two(a, b) return a + b end\n\
             local i = debug.getinfo(two)\n\
             return i.what .. '|' .. tostring(i.nparams) .. '|' .. tostring(i.isvararg)"
        )?,
        "Lua|2|false"
    );
    Ok(())
}

#[test]
fn getinfo_marks_rust_callbacks_as_c() -> Result<(), ExternError> {
    assert_eq!(
        eval("local i = debug.getinfo(print)\nreturn i.what .. '|' .. i.short_src")?,
        "C|[C]"
    );
    Ok(())
}

/// mlua has no general upvalue accessor at all; luna exposes them directly.
#[test]
fn upvalues_can_be_read_and_written() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            "local captured = 10\n\
             local f = function() return captured end\n\
             local before = select(2, debug.getupvalue(f, 1))\n\
             debug.setupvalue(f, 1, 99)\n\
             return tostring(before) .. '->' .. tostring(f())"
        )?,
        "10->99"
    );
    Ok(())
}

#[test]
fn an_out_of_range_upvalue_is_nil() -> Result<(), ExternError> {
    assert_eq!(
        eval("local f = function() return 1 end\nreturn tostring(debug.getupvalue(f, 5))")?,
        "nil"
    );
    Ok(())
}

/// `debug.getmetatable` ignores `__metatable`, which is the whole point of it.
#[test]
fn debug_getmetatable_sees_through_protection() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            "local t = setmetatable({}, { __metatable = 'locked', marker = 'real' })\n\
             return tostring(getmetatable(t)) .. '|' .. tostring(debug.getmetatable(t).marker)"
        )?,
        "locked|real"
    );
    Ok(())
}
