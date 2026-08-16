//! The small standard-library additions.

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
fn coroutine_wrap_returns_values() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local gen = coroutine.wrap(function()
            coroutine.yield(1)
            coroutine.yield(2)
            return 3
        end)
        return gen() == 1 and gen() == 2 and gen() == 3
    "#
    )?);
    Ok(())
}

/// The difference from `resume`: an error propagates instead of coming back as `false, err`.
#[test]
fn coroutine_wrap_propagates_errors() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local gen = coroutine.wrap(function() error("boom") end)
        local ok = pcall(gen)
        return ok == false
    "#
    )?);
    Ok(())
}

#[test]
fn coroutine_close_kills_a_suspended_coroutine() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local co = coroutine.create(function() coroutine.yield(1) return 2 end)
        coroutine.resume(co)
        local before = coroutine.status(co)
        coroutine.close(co)
        return before == "suspended" and coroutine.status(co) == "dead"
    "#
    )?);
    Ok(())
}

#[test]
fn coroutine_isyieldable() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local inside
        local co = coroutine.create(function() inside = coroutine.isyieldable() end)
        coroutine.resume(co)
        return coroutine.isyieldable() == false and inside == true
    "#
    )?);
    Ok(())
}

/// A coroutine that has resumed another is "normal", not "running".
#[test]
fn coroutine_status_reports_normal() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local outer_status
        local inner = coroutine.create(function(outer) outer_status = coroutine.status(outer) end)
        local outer
        outer = coroutine.create(function() coroutine.resume(inner, outer) end)
        coroutine.resume(outer)
        return outer_status == "normal"
    "#
    )?);
    Ok(())
}

#[test]
fn rawequal_ignores_the_eq_metamethod() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local mt = { __eq = function() return true end }
        local a, b = setmetatable({}, mt), setmetatable({}, mt)
        return (a == b) == true and rawequal(a, b) == false and rawequal(a, a) == true
            and rawequal(1, 1) == true and rawequal("x", "x") == true and rawequal(1, 2) == false
    "#
    )?);
    Ok(())
}

#[test]
fn xpcall_runs_the_handler() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local ok, msg = xpcall(function() error("inner") end, function(e)
            -- `error` is level 1, so `e` arrives with a "chunk:line:" prefix.
            return "handled: " .. (tostring(e):gsub("^.*:%d+: ", ""))
        end)
        return ok == false and msg == "handled: inner"
    "#
    )?);
    Ok(())
}

#[test]
fn xpcall_passes_through_success() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local ok, a, b = xpcall(function() return 1, 2 end, function(e) return e end)
        return ok == true and a == 1 and b == 2
    "#
    )?);
    Ok(())
}

#[test]
fn xpcall_forwards_extra_arguments() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local ok, v = xpcall(function(a, b) return a + b end, function(e) return e end, 3, 4)
        return ok == true and v == 7
    "#
    )?);
    Ok(())
}

#[test]
fn warn_exists_and_accepts_strings() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        warn("@off")
        return type(warn) == "function"
    "#
    )?);
    Ok(())
}

/// `loadfile` and `dofile` read from disk, skipping a BOM or shebang the way `luaL_loadfile` does.
#[test]
fn loadfile_and_dofile_read_a_chunk() -> Result<(), ExternError> {
    let dir = std::env::temp_dir().join("luna_loadfile_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("chunk.lua");
    std::fs::write(&path, "#!/usr/bin/env lua\nreturn 21 * 2\n").unwrap();
    let path = path.display().to_string().replace('\\', "\\\\");

    assert!(eval(&format!(
        r#"
        local f = loadfile("{path}")
        local a = f()
        local b = dofile("{path}")
        local missing, err = loadfile("{path}.does-not-exist")
        return a == 42 and b == 42 and missing == nil and type(err) == "string"
    "#
    ))?);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
