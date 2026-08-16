//! Divergences from PUC-Rio 5.4 that silently changed results.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

/// `i as f64` loses precision above 2^53, which corrupted sorts and range checks on large ids.
#[test]
fn integers_and_floats_compare_exactly() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return (math.maxinteger == (math.maxinteger + 0.0)) == false
            and ((math.maxinteger - 1) < (math.maxinteger + 0.0)) == true
            and (math.mininteger == (math.mininteger + 0.0)) == true
            and (9007199254740993 == 9007199254740992.0) == false
    "#
    )?);
    Ok(())
}

/// NaN compares false against everything — it must not become "cannot compare".
#[test]
fn nan_compares_false_rather_than_erroring() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local nan = 0/0
        return (nan == 0) == false and (nan > 0) == false and (nan < 0) == false
            and (nan >= 0) == false and (nan <= 0) == false and (0 < nan) == false
    "#
    )?);
    Ok(())
}

#[test]
fn modf_keeps_a_float_integral_part() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local i, f = math.modf(1e100)
        local i2 = math.modf(3.7)
        return i == 1e100 and f == 0.0 and math.type(i2) == "float" and i2 == 3.0
    "#
    )?);
    Ok(())
}

#[test]
fn fmod_is_integral_and_refuses_zero() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return math.type(math.fmod(5, 3)) == "integer"
            and math.fmod(5, 3) == 2
            and pcall(math.fmod, 5, 0) == false
            and math.type(math.fmod(5.0, 3)) == "float"
    "#
    )?);
    Ok(())
}

/// The point of `tointeger` is "convertible without loss"; a string is not a number.
#[test]
fn tointeger_rejects_strings() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return math.tointeger("3") == nil and math.tointeger(3.0) == 3
            and math.tointeger(3) == 3 and math.tointeger(3.5) == nil
    "#
    )?);
    Ok(())
}

/// Float-ness is contagious, so a coerced string that is integral must stay an integer.
#[test]
fn string_arithmetic_keeps_integers() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return math.type("10" + 1) == "integer"
            and math.type("10" * 2) == "integer"
            and math.type("10" - 1) == "integer"
            and math.type("10.5" + 1) == "float"
    "#
    )?);
    Ok(())
}

/// PUC-Rio raises for a string operand; being more permissive hides typos.
#[test]
fn bitwise_operators_reject_strings() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return pcall(function() return "10" | 1 end) == false
            and pcall(function() return 1 & "3" end) == false
            and pcall(function() return ~"2" end) == false
            and pcall(function() return "2" << 1 end) == false
            and pcall(function() return 2 >> "1" end) == false
            and (2 | 1) == 3 and (2.0 | 1) == 3
    "#
    )?);
    Ok(())
}

/// The seed is returned so an unseeded run can be reproduced.
#[test]
fn randomseed_returns_its_components() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local a, b = math.randomseed(42)
        return a == 42 and b == 0 and select('#', math.randomseed()) == 2
    "#
    )?);
    Ok(())
}
