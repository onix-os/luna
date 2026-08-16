//! `__metatable`, `__name`, metatables on non-table values, and PUC-Rio float formatting.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

/// The only mechanism a library has to make a metatable tamper-proof.
#[test]
fn metatable_field_protects_the_metatable() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local guarded = setmetatable({}, { __metatable = "locked", __index = function() return 7 end })
        local handed_out = getmetatable(guarded)
        local replaced = pcall(setmetatable, guarded, {})
        return handed_out == "locked" and replaced == false and guarded.anything == 7
    "#
    )?);
    Ok(())
}

#[test]
fn getmetatable_works_on_strings() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local mt = getmetatable("")
        return mt ~= nil and mt.__index == string
    "#
    )?);
    Ok(())
}

#[test]
fn getmetatable_returns_nil_rather_than_erroring() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return getmetatable(42) == nil and getmetatable(nil) == nil
            and getmetatable(true) == nil and getmetatable({}) == nil
    "#
    )?);
    Ok(())
}

#[test]
fn name_metafield_improves_tostring() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local point = setmetatable({}, { __name = "Point" })
        return tostring(point):find("^Point: ") ~= nil
    "#
    )?);
    Ok(())
}

/// `__tostring` still wins over `__name`.
#[test]
fn tostring_metamethod_beats_name() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local p = setmetatable({}, { __name = "Point", __tostring = function() return "explicit" end })
        return tostring(p) == "explicit"
    "#
    )?);
    Ok(())
}

/// Telling an integer from a float by printing it is how 5.4 users reason about the split.
#[test]
fn floats_print_as_puc_rio_does() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return tostring(1.0) == "1.0"
            and tostring(0.0) == "0.0"
            and tostring(-1203 + 0.0) == "-1203.0"
            and tostring(1203.125) == "1203.125"
            and tostring(1/3) == "0.33333333333333"
            and tostring(1e100) == "1e+100"
            and tostring(1e-100) == "1e-100"
            and tostring(7 // 2.0) == "3.0"
            and tostring(12) == "12"
    "#
    )?);
    Ok(())
}

/// Concatenation coerces the same way, or `12.0 .. ""` would lose its float-ness.
#[test]
fn concat_coerces_floats_the_same_way() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return (12.0 .. "") == "12.0" and ("" .. 12) == "12" and (1e100 .. "") == "1e+100"
    "#
    )?);
    Ok(())
}

#[test]
fn infinities_and_nan_still_print() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return tostring(1/0) == "inf" and tostring(-1/0) == "-inf"
            and tostring(0/0):find("nan") ~= nil
    "#
    )?);
    Ok(())
}

/// `gsub`'s replacement table used a raw lookup, so a class-style `__index` found nothing.
#[test]
fn gsub_follows_index_tables() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local base = { cat = "CAT" }
        local repl = setmetatable({}, { __index = base })
        local out = string.gsub("a cat here", "cat", repl)
        return out == "a CAT here"
    "#
    )?);
    Ok(())
}

/// `table.clear` releases the buckets, unlike setting each key to nil.
#[test]
fn table_clear_empties_and_respects_freezing() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = { 1, 2, 3, x = 1 }
        table.clear(t)
        local emptied = #t == 0 and next(t) == nil
        local refused = pcall(table.clear, table.freeze({})) == false
        return emptied and refused
    "#
    )?);
    Ok(())
}

/// `gmatch` used to discard its third argument silently.
#[test]
fn gmatch_honours_init() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local out = {}
        for w in string.gmatch("one two three", "%a+", 5) do out[#out + 1] = w end
        return table.concat(out, ",") == "two,three"
    "#
    )?);
    Ok(())
}

/// `ipairs` returns the triple the manual specifies, not just a pair.
#[test]
fn ipairs_returns_three_values() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local f, s, var = ipairs({ 10, 20 })
        local total = 0
        for _, v in ipairs({ 10, 20, 30 }) do total = total + v end
        return type(f) == "function" and type(s) == "table" and var == 0 and total == 60
    "#
    )?);
    Ok(())
}
