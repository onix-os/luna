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

/// `i64::MIN % -1` traps on the hardware instruction, which aborted the host rather than raising.
#[test]
fn integer_division_survives_the_mininteger_edge() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local ok, r = pcall(function() return math.mininteger % -1 end)
        return ok and r == 0 and math.type(r) == "integer"
            and (math.mininteger // -1) == math.mininteger
            and math.type(math.mininteger // -1) == "integer"
            and math.abs(math.mininteger) == math.mininteger
            and pcall(function() return 1 % 0 end) == false
            and pcall(function() return 1 // 0 end) == false
    "#
    )?);
    Ok(())
}

/// `%` follows the sign of the divisor, unlike Rust's remainder.
#[test]
fn integer_modulo_follows_the_divisor_sign() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return (7 % 3) == 1 and (-7 % 3) == 2 and (7 % -3) == -2 and (-7 % -3) == -1
            and (7 // 3) == 2 and (-7 // 3) == -3 and (7 // -3) == -3 and (-7 // -3) == 2
    "#
    )?);
    Ok(())
}

/// Correcting the remainder as `(m + b) % b` turns an infinite divisor into NaN; `luai_nummod`
/// adds the divisor at most once instead.
#[test]
fn float_modulo_handles_infinite_operands() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local huge = math.huge
        return (5 % huge) == 5.0 and math.type(5 % huge) == "float"
            and (-5 % huge) == huge
            and (5 % -huge) == -huge
            and (-5 % -huge) == -5.0
            and (5.5 % 2) == 1.5 and (-5.5 % 2) == 0.5
            and (5.5 % -2) == -0.5 and (-5.5 % -2) == -1.5
    "#
    )?);
    Ok(())
}

/// `maxinteger` is 2^63-1, so the float 2^63 is one past the end of the range.
#[test]
fn two_to_the_sixty_three_is_out_of_integer_range() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return math.tointeger(2^63) == nil
            and math.tointeger(-2^63) == math.mininteger
            and math.tointeger(2^63 - 1024) == 9223372036854774784
            and math.tointeger(math.huge) == nil and math.tointeger(0/0) == nil
            and pcall(function() return 2^63 | 0 end) == false
            and (-2^63 | 0) == math.mininteger
    "#
    )?);
    Ok(())
}

/// An integer is its own floor and its own ceiling; a detour through f64 loses bits past 2^53.
#[test]
fn floor_and_ceil_leave_integers_alone() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return math.floor(math.maxinteger - 1) == math.maxinteger - 1
            and math.ceil(math.maxinteger - 1) == math.maxinteger - 1
            and math.floor(math.mininteger) == math.mininteger
            and math.floor(3.7) == 3 and math.type(math.floor(3.7)) == "integer"
            and math.ceil(3.2) == 4 and math.floor(-3.2) == -4 and math.ceil(-3.7) == -3
            and math.floor(1e100) == 1e100 and math.type(math.floor(1e100)) == "float"
            and math.floor("3.7") == 3
    "#
    )?);
    Ok(())
}

/// String coercion keeps the integer subtype, so `//`, `%` and unary `-` stay integers. `/` and
/// `^` are the two operators that are float-valued whatever they are given.
#[test]
fn string_coercion_keeps_the_integer_subtype() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return math.type("7" // "2") == "integer" and ("7" // "2") == 3
            and math.type("7" % "2") == "integer" and ("7" % "2") == 1
            and math.type(-"3") == "integer" and (-"3") == -3
            and math.type(-"3.0") == "float"
            and math.type("0x10" + 0) == "integer" and ("0x10" + 0) == 16
            and math.type("7.0" // "2") == "float"
            and math.type("7" / "2") == "float"
            and math.type("2" ^ "3") == "float" and ("2" ^ "3") == 8.0
            and math.type(7 / 2) == "float" and math.type(2 ^ 3) == "float"
    "#
    )?);
    Ok(())
}

/// `l_str2d` rejects any string holding an 'n', which is what keeps "inf" and "nan" from being
/// numbers. Hex floats are recognised before that test and must survive it.
#[test]
fn inf_and_nan_are_not_numbers() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return tonumber("inf") == nil and tonumber("nan") == nil
            and tonumber("infinity") == nil and tonumber("-inf") == nil
            and tonumber("NaN") == nil and tonumber("Inf") == nil
            and pcall(function() return 10 + "inf" end) == false
            and pcall(function() return -"nan" end) == false
            and tonumber("0x10") == 16 and tonumber("0x1p4") == 16.0
            and tonumber("1.5e3") == 1500.0 and tonumber("1e400") == math.huge
            and tonumber(" 3.5 ") == 3.5 and tonumber("-2") == -2
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
