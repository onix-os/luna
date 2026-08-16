//! The last PUC-Rio divergences closed: version reporting, `pairs` arity, the remaining `debug`
//! functions, and `package.searchers` as a real hook rather than a description of what `require`
//! happens to do.

use luna::{Closure, Executor, ExternError, Lua};

fn eval<T: for<'gc> luna::FromMultiValue<'gc> + 'static>(source: &str) -> Result<T, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<T>(&executor)
}

/// `_VERSION` is the language, so `_VERSION == "Lua 5.4"` feature detection works; `_LUNA` is the
/// implementation, which PUC-Rio has no equivalent for.
#[test]
fn version_reports_the_language_and_luna_reports_the_implementation() -> Result<(), ExternError> {
    assert_eq!(eval::<String>("return _VERSION")?, "Lua 5.4");
    assert!(eval::<bool>("return _LUNA ~= nil and #_LUNA > 0")?);
    Ok(())
}

#[test]
fn pairs_returns_three_values() -> Result<(), ExternError> {
    assert_eq!(eval::<i64>("return select('#', pairs({}))")?, 3);
    assert!(eval::<bool>(
        r#"
        local f, s, var = pairs({ a = 1 })
        return type(f) == "function" and type(s) == "table" and var == nil
    "#
    )?);
    Ok(())
}

#[test]
fn getregistry_returns_a_table_holding_the_globals() -> Result<(), ExternError> {
    // Index 2 is where PUC-Rio's `LUA_RIDX_GLOBALS` sits.
    assert!(eval::<bool>(
        r#"
        local r = debug.getregistry()
        if type(r) ~= "table" or not rawequal(r[2], _G) then return false end
        -- It persists, so a host and its scripts can use it to share state.
        r.marker = 7
        return debug.getregistry().marker == 7
    "#
    )?);
    Ok(())
}

/// The whole point of `upvalueid`: telling whether two closures share a variable.
#[test]
fn upvalueid_identifies_a_shared_upvalue() -> Result<(), ExternError> {
    assert!(eval::<bool>(
        r#"
        local function make()
            local x = 0
            return function() x = x + 1 return x end, function() return x end
        end
        local a, b = make()
        local c = make()
        local ida, idb = debug.upvalueid(a, 1), debug.upvalueid(b, 1)
        return type(ida) == "userdata"
            and ida == idb            -- the same variable
            and rawequal(ida, idb)    -- and literally the same object, so rawequal agrees
            and debug.upvalueid(c, 1) ~= ida   -- a different closure's variable is a different id
    "#
    )?);
    Ok(())
}

#[test]
fn upvalueid_rejects_an_index_out_of_range() -> Result<(), ExternError> {
    assert!(eval::<bool>(
        r#"
        local f = function() return 1 end
        return not pcall(debug.upvalueid, f, 5)
    "#
    )?);
    Ok(())
}

#[test]
fn upvaluejoin_makes_two_closures_share_a_variable() -> Result<(), ExternError> {
    assert_eq!(
        eval::<String>(
            r#"
            local function counter() local n = 0 return function() n = n + 1 return n end end
            local p, q = counter(), counter()
            p() p()
            -- p is at 2, q is at 0
            local before = p() .. "," .. q()
            debug.upvaluejoin(q, 1, p, 1)
            -- now q continues p's count rather than its own
            return before .. " " .. q() .. "," .. p()
        "#
        )?,
        "3,1 4,5"
    );
    Ok(())
}

#[test]
fn upvaluejoin_rejects_bad_arguments() -> Result<(), ExternError> {
    assert!(eval::<bool>(
        r#"
        local f = function() return 1 end
        local g = function() return 2 end
        return not pcall(debug.upvaluejoin, f, 1, g, 1)     -- neither has upvalues
            and not pcall(debug.upvaluejoin, print, 1, g, 1) -- not a Lua function
    "#
    )?);
    Ok(())
}

#[test]
fn getuservalue_answers_for_a_non_userdata() -> Result<(), ExternError> {
    // PUC-Rio returns fail rather than raising, so a script can probe safely.
    assert!(eval::<bool>("return debug.getuservalue({}) == nil")?);
    Ok(())
}

/// A searcher a script installs is really consulted, which is what makes it a hook.
#[test]
fn require_walks_the_searchers_table() -> Result<(), ExternError> {
    assert_eq!(
        eval::<String>(
            r#"
            package.searchers[#package.searchers + 1] = function(name)
                if name == "virtual" then
                    return function(modname, extra) return modname .. "|" .. tostring(extra) end
                end
                return "not the virtual module"
            end
            return require("virtual")
        "#
        )?,
        "virtual|nil"
    );
    Ok(())
}

#[test]
fn a_searcher_that_declines_contributes_its_reason() -> Result<(), ExternError> {
    let message = eval::<String>(
        r#"
        package.searchers[#package.searchers + 1] = function() return "searcher said no" end
        local ok, err = pcall(require, "absent.module")
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(
        message.contains("module 'absent.module' not found"),
        "{message}"
    );
    assert!(message.contains("searcher said no"), "{message}");
    // The built-in searchers still report their own reasons alongside it.
    assert!(message.contains("package.preload"), "{message}");
    Ok(())
}

#[test]
fn preload_still_wins_and_caches() -> Result<(), ExternError> {
    assert!(eval::<bool>(
        r#"
        local calls = 0
        package.preload["counted"] = function() calls = calls + 1 return { n = calls } end
        local first = require("counted")
        local second = require("counted")
        -- Loaded once, cached thereafter, and the same table both times.
        return calls == 1 and rawequal(first, second)
    "#
    )?);
    Ok(())
}

/// A loader returning nothing still marks the module loaded, so it does not run twice.
#[test]
fn a_loader_returning_nil_records_true() -> Result<(), ExternError> {
    assert!(eval::<bool>(
        r#"
        local calls = 0
        package.preload["quiet"] = function() calls = calls + 1 end
        local value = require("quiet")
        require("quiet")
        return value == true and calls == 1
    "#
    )?);
    Ok(())
}
