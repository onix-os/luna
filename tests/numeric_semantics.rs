//! Divergences from PUC-Rio 5.4 that silently changed results.

use luna::{Closure, Executor, ExternError};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = common::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

mod common;

#[test]
fn integer_loops_stop_on_overflow_and_respect_direction() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local function collect(first, limit, step, stop)
            local count, last = 0, nil
            for i=first,limit,step do
                count, last = count+1, i
                assert(count <= 4)
                if count == stop then break end
            end
            return count, last
        end
        local function check(first, limit, step, expected_count, expected_last, stop)
            local count, last = collect(first, limit, step, stop)
            assert(count == expected_count and last == expected_last)
        end
        local lo, hi = math.mininteger, math.maxinteger
        check(hi-2, hi, 1, 3, hi)
        check(lo+2, lo, -1, 3, lo)
        check(1, hi, hi, 1, 1)
        check(-1, lo, -hi, 2, lo)
        check(lo, lo, lo, 1, lo)
        check(5, 3, 1, 0, nil)
        check(3, 5, -1, 0, nil)
        assert(not pcall(collect, 2, 2, 0, 3))
        assert(not pcall(collect, 3, 2, 0, 3))
        check(-3, -1, 1, 3, -1)
        return true
        "#
    )?);
    Ok(())
}

#[test]
fn direct_operators_match_constant_semantics() {
    use luna::{meta_ops, Context, Value};

    fn check<'gc>(ctx: Context<'gc>, a: Value<'gc>, b: Value<'gc>) {
        let left = a.to_constant().unwrap();
        let right = b.to_constant().unwrap();
        for operation in 0..5 {
            let (actual, expected) = match operation {
                0 => (meta_ops::add(ctx, a, b), left.add(&right)),
                1 => (meta_ops::subtract(ctx, a, b), left.subtract(&right)),
                2 => (meta_ops::multiply(ctx, a, b), left.multiply(&right)),
                3 => (
                    meta_ops::less_than(ctx, a, b),
                    left.less_than(&right).map(luna::Constant::Boolean),
                ),
                _ => (
                    meta_ops::less_equal(ctx, a, b),
                    left.less_equal(&right).map(luna::Constant::Boolean),
                ),
            };
            match (actual, expected.map(Value::from)) {
                (Ok(meta_ops::MetaResult::Value(Value::Number(a))), Some(Value::Number(b))) => {
                    assert!(a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()));
                }
                (Ok(meta_ops::MetaResult::Value(Value::Integer(a))), Some(Value::Integer(b))) => {
                    assert_eq!(a, b);
                }
                (Ok(meta_ops::MetaResult::Value(Value::Boolean(a))), Some(Value::Boolean(b))) => {
                    assert_eq!(a, b);
                }
                (Err(_), None) => {}
                result => panic!("operation={operation} operands={a:?},{b:?}: {result:?}"),
            }
        }
    }

    let mut lua = luna::Lua::empty();
    lua.enter(|ctx| {
        let mut values = vec![Value::Nil, Value::Boolean(false), Value::Boolean(true)];
        for integer in [
            i64::MIN,
            i64::MAX,
            -9_007_199_254_740_993,
            9_007_199_254_740_993,
            -1,
            0,
            1,
        ] {
            values.push(Value::Integer(integer));
            values.push(Value::Number(integer as f64));
        }
        for bits in [
            0,
            1,
            0x000f_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0x3fe0_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_1234,
        ] {
            values.push(Value::Number(f64::from_bits(bits)));
            values.push(Value::Number(f64::from_bits(bits | (1 << 63))));
        }
        for bytes in [
            &b""[..],
            b" \t42\n",
            b"-0.0",
            b"0x1.8p+2",
            b"1e400",
            b"1e-400",
            b"9223372036854775808",
            b"9007199254740993",
            b"nan",
            b"inf",
            b"1x",
            b"\xff",
        ] {
            values.push(Value::String(ctx.intern(bytes)));
        }
        for &a in &values {
            for &b in &values {
                check(ctx, a, b);
            }
        }
        let mut state = 0x2348_690a_bcef_d123u64;
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let left = [
                Value::Integer(state as i64),
                Value::Number(f64::from_bits(state)),
            ];
            state = state.rotate_left(19).wrapping_add(0x9e37_79b9_7f4a_7c15);
            let right = [
                Value::Integer(state as i64),
                Value::Number(f64::from_bits(state)),
            ];
            for a in left {
                for b in right {
                    check(ctx, a, b);
                }
            }
        }
    });
}

#[test]
fn comparison_fallback_preserves_metamethod_order_and_errors() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local left, right = {}, {}
        local calls = 0
        local function less(a, b)
            calls = calls + 1
            assert(a == left and b == right)
            return true
        end
        setmetatable(left, { __lt=less, __le=less })
        setmetatable(right, { __lt=function() error('wrong side') end,
                              __le=function() error('wrong side') end })
        assert(left < right and left <= right)
        setmetatable(left, {})
        setmetatable(right, { __lt=less, __le=less })
        assert(left < right and left <= right)
        assert(calls == 4)
        assert('10' < '2' and '2' <= '2')
        assert(not pcall(function() return '2' < 3 end))
        assert(not pcall(function() return 3 <= '4' end))
        assert(not pcall(function() return nil < 0 end))
        assert(not pcall(function() return false <= true end))
        return true
    "#
    )?);
    Ok(())
}

#[test]
fn arithmetic_fallback_preserves_metamethod_operands() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local count = 0
        local object = {}
        local function operand(value)
            if value == object then return 7 end
            assert(value == 2)
            return value
        end
        setmetatable(object, {
            __add = function(a, b) count=count+1; return operand(a)+operand(b) end,
            __sub = function(a, b) count=count+1; return operand(a)-operand(b) end,
            __mul = function(a, b) count=count+1; return operand(a)*operand(b) end,
        })
        assert(object+2 == 9 and 2+object == 9)
        assert(object-2 == 5 and 2-object == -5)
        assert(object*2 == 14 and 2*object == 14)
        assert(count == 6)
        assert(not pcall(function() return true+2 end))
        assert(not pcall(function() return 2-"invalid" end))
        assert(not pcall(function() return nil*2 end))
        return true
        "#
    )?);
    Ok(())
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

#[test]
fn mixed_negative_fractions_preserve_order_and_equality() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return (-1 == -1.5) == false and (-1 < -1.5) == false
            and (-1 <= -1.5) == false and (-1.5 < -1) == true
            and (-1.5 <= -1) == true and (0 == -0.5) == false
            and (0 <= -0.5) == false and (-0.5 < 0) == true
            and (-1 == -1.0) == true and (0 == -0.0) == true
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

#[test]
fn two_component_randomseed_replays_and_uses_both_components() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local high, low = 0x11223344556677, 0x778899aabbccdd
        local a, b = math.randomseed(high, low)
        assert(a == high and b == low)
        local sequence = {}
        for i=1,16 do sequence[i] = math.random(0) end
        math.randomseed(high, low)
        for i=1,16 do assert(math.random(0) == sequence[i]) end
        local function differs(h, l)
            math.randomseed(h, l)
            for i=1,16 do
                if math.random(0) ~= sequence[i] then return true end
            end
            return false
        end
        return differs(high + 1, low) and differs(high, low + 1)
        "#
    )?);
    Ok(())
}
