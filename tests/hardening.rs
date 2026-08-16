//! Scripts a host cannot trust: the ones that used to hang the VM or exhaust memory rather than
//! raising an error the host can catch.

use luna::{Closure, Executor, ExternError, Lua};

fn eval<T: for<'gc> luna::FromMultiValue<'gc> + 'static>(source: &str) -> Result<T, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<T>(&executor)
}

#[test]
fn cyclic_index_chain_errors() -> Result<(), ExternError> {
    // Two tables whose `__index` points at each other used to spin forever; the chain is now
    // bounded and the script sees an ordinary error.
    let message = eval::<String>(
        r#"
        local a, b = {}, {}
        setmetatable(a, { __index = b })
        setmetatable(b, { __index = a })
        local ok, err = pcall(function() return a.missing end)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("'__index' chain too long"), "{message}");
    Ok(())
}

#[test]
fn cyclic_newindex_chain_errors() -> Result<(), ExternError> {
    let message = eval::<String>(
        r#"
        local a, b = {}, {}
        setmetatable(a, { __newindex = b })
        setmetatable(b, { __newindex = a })
        local ok, err = pcall(function() a.x = 1 end)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("'__newindex' chain too long"), "{message}");
    Ok(())
}

#[test]
fn a_long_but_finite_index_chain_still_resolves() -> Result<(), ExternError> {
    // The depth limit has to sit above anything a real program builds, so a deep-but-terminating
    // prototype chain must still reach its value.
    assert_eq!(
        eval::<i64>(
            r#"
            local base = { found = 7 }
            for _ = 1, 200 do base = setmetatable({}, { __index = base }) end
            return base.found
        "#
        )?,
        7
    );
    Ok(())
}

#[test]
fn repeated_concat_errors_instead_of_exhausting_memory() -> Result<(), ExternError> {
    // Doubling a string 60 times is 2^60 bytes. Without a cap this killed the process rather than
    // raising anything the host could catch.
    let message = eval::<String>(
        r#"
        local ok, err = pcall(function()
            local s = "x"
            for _ = 1, 60 do s = s .. s end
            return #s
        end)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("too long"), "{message}");
    Ok(())
}

#[test]
fn print_uses_the_tostring_metamethod() -> Result<(), ExternError> {
    // `print` formats through `__tostring` like PUC-Rio's does, which means calling back into Lua
    // mid-print. The flag proves the metamethod ran; its return value goes to stdout.
    assert!(eval::<bool>(
        r#"
        local called = false
        local o = setmetatable({}, { __tostring = function() called = true return "CUSTOM" end })
        print(o, 1, o)
        return called
    "#
    )?);
    Ok(())
}

#[test]
fn print_rejects_a_tostring_that_returns_a_non_string() -> Result<(), ExternError> {
    let message = eval::<String>(
        r#"
        local o = setmetatable({}, { __tostring = function() return 5 end })
        local ok, err = pcall(print, o)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("must return a string"), "{message}");
    Ok(())
}

#[test]
fn an_error_inside_tostring_propagates_out_of_print() -> Result<(), ExternError> {
    let message = eval::<String>(
        r#"
        local o = setmetatable({}, { __tostring = function() error("inner") end })
        local ok, err = pcall(print, o)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("inner"), "{message}");
    Ok(())
}
