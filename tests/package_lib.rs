//! `require`, `package.preload`, `package.loaded` and the Lua file searcher.
//!
//! There is no C loader and there never will be, so `package.cpath` and `package.loadlib` are
//! absent by design.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

#[test]
fn preload_is_consulted_first() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        package.preload["greet"] = function(name)
            return { who = "world", name = name }
        end
        local m = require("greet")
        return m.who == "world" and m.name == "greet"
    "#
    )?);
    Ok(())
}

/// A module is loaded once; the second `require` returns the cached value.
#[test]
fn modules_are_cached() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local runs = 0
        package.preload["counter"] = function()
            runs = runs + 1
            return { n = runs }
        end
        local a = require("counter")
        local b = require("counter")
        return runs == 1 and a == b
    "#
    )?);
    Ok(())
}

/// A loader that returns nothing still counts as loaded.
#[test]
fn a_nil_returning_loader_records_true() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        package.preload["silent"] = function() end
        local v = require("silent")
        return v == true and package.loaded["silent"] == true
    "#
    )?);
    Ok(())
}

#[test]
fn loaded_can_be_seeded_directly() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        package.loaded["preseeded"] = { ok = true }
        return require("preseeded").ok == true
    "#
    )?);
    Ok(())
}

#[test]
fn a_missing_module_errors_by_name() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local ok, err = pcall(require, "definitely.not.here")
        return ok == false and string.find(err, "definitely.not.here", 1, true) ~= nil
    "#
    )?);
    Ok(())
}

/// The file searcher, including the dot-to-slash rewrite.
#[test]
fn the_file_searcher_finds_modules_on_the_path() -> Result<(), ExternError> {
    let dir = std::env::temp_dir().join("luna_require_test");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("plain.lua"), "return { id = 'plain' }").unwrap();
    std::fs::write(dir.join("nested/inner.lua"), "return { id = 'inner' }").unwrap();
    let base = dir.display().to_string().replace('\\', "\\\\");

    assert!(eval(&format!(
        r#"
        package.path = "{base}/?.lua"
        local a = require("plain")
        local b = require("nested.inner")
        return a.id == "plain" and b.id == "inner"
    "#
    ))?);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[test]
fn there_is_no_c_loader() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return package.loadlib == nil and package.cpath == nil
    "#
    )?);
    Ok(())
}
