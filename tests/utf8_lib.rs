//! The `utf8` library. luna strings are byte strings, which is the same substrate PUC-Rio uses.

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
fn char_and_codepoint_round_trip() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local s = utf8.char(72, 233, 0x4e2d, 0x1F600)
        local a, b, c, d = utf8.codepoint(s, 1, #s)
        return a == 72 and b == 233 and c == 0x4e2d and d == 0x1F600
    "#
    )?);
    Ok(())
}

/// `#s` counts bytes; `utf8.len` counts characters. That difference is the whole point.
#[test]
fn len_counts_characters_not_bytes() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local s = utf8.char(233, 0x4e2d, 0x1F600)
        return #s == 9 and utf8.len(s) == 3
    "#
    )?);
    Ok(())
}

#[test]
fn len_reports_the_position_of_bad_bytes() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local bad = "ok" .. string.char(0xff)
        local n, pos = utf8.len(bad)
        return n == nil and pos == 3
    "#
    )?);
    Ok(())
}

#[test]
fn codes_iterates_characters() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local s = utf8.char(72, 0x4e2d, 0x1F600)
        local positions, points = {}, {}
        for p, c in utf8.codes(s) do
            positions[#positions + 1] = p
            points[#points + 1] = c
        end
        return #points == 3 and points[1] == 72 and points[2] == 0x4e2d
            and points[3] == 0x1F600 and positions[1] == 1 and positions[2] == 2
            and positions[3] == 5
    "#
    )?);
    Ok(())
}

#[test]
fn offset_finds_character_boundaries() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local s = utf8.char(72, 0x4e2d, 0x1F600)
        return utf8.offset(s, 1) == 1 and utf8.offset(s, 2) == 2 and utf8.offset(s, 3) == 5
            and utf8.offset(s, -1) == 5
    "#
    )?);
    Ok(())
}

#[test]
fn charpattern_matches_one_character_at_a_time() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local s = utf8.char(72, 0x4e2d, 0x1F600)
        local n = 0
        for _ in string.gmatch(s, utf8.charpattern) do n = n + 1 end
        return n == 3
    "#
    )?);
    Ok(())
}

/// Overlong encodings and out-of-range code points are rejected, as in PUC-Rio.
#[test]
fn invalid_sequences_are_rejected() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local overlong = string.char(0xc0, 0x80)
        local truncated = string.char(0xe4, 0xb8)
        return utf8.len(overlong) == nil and utf8.len(truncated) == nil
            and pcall(utf8.char, 0x110000) == false
    "#
    )?);
    Ok(())
}
