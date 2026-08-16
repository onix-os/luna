//! `string.pack`, `string.unpack` and `string.packsize`.

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
fn integers_round_trip() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local packed = string.pack("<i4i4", 1, -2)
        local a, b, pos = string.unpack("<i4i4", packed)
        return #packed == 8 and a == 1 and b == -2 and pos == 9
    "#
    )?);
    Ok(())
}

/// Sign extension from a narrower packed width is the easy thing to get wrong.
#[test]
fn narrow_signed_integers_sign_extend() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local a = string.unpack("<i2", string.pack("<i2", -300))
        local b = string.unpack("<b", string.pack("<b", -1))
        local c = string.unpack("<B", string.pack("<B", 255))
        return a == -300 and b == -1 and c == 255
    "#
    )?);
    Ok(())
}

#[test]
fn endianness_is_honoured() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local le = string.pack("<i2", 1)
        local be = string.pack(">i2", 1)
        return le:byte(1) == 1 and le:byte(2) == 0
            and be:byte(1) == 0 and be:byte(2) == 1
            and string.unpack(">i2", be) == 1
    "#
    )?);
    Ok(())
}

#[test]
fn floats_and_doubles_round_trip() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local d = string.unpack("<d", string.pack("<d", 0.5))
        local f = string.unpack("<f", string.pack("<f", 0.25))
        return d == 0.5 and f == 0.25
    "#
    )?);
    Ok(())
}

#[test]
fn strings_round_trip_in_all_three_forms() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local z = string.unpack("z", string.pack("z", "hello"))
        local s = string.unpack("<s4", string.pack("<s4", "world"))
        local c = string.unpack("c5", string.pack("c5", "abc"))
        return z == "hello" and s == "world" and c == "abc\0\0"
    "#
    )?);
    Ok(())
}

#[test]
fn packsize_measures_fixed_formats() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return string.packsize("<i4i4") == 8 and string.packsize("<i2d") == 10
            and string.packsize("<c7") == 7 and string.packsize("<i4x") == 5
    "#
    )?);
    Ok(())
}

/// Variable-length formats have no fixed size, and Lua errors rather than guessing.
#[test]
fn packsize_refuses_variable_formats() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return pcall(string.packsize, "z") == false
            and pcall(string.packsize, "s4") == false
    "#
    )?);
    Ok(())
}

#[test]
fn truncated_data_errors() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return pcall(string.unpack, "<i4i4", string.pack("<i4", 1)) == false
    "#
    )?);
    Ok(())
}

#[test]
fn unpack_can_start_at_a_position() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local packed = string.pack("<i4i4", 7, 9)
        return string.unpack("<i4", packed, 5) == 9
    "#
    )?);
    Ok(())
}
