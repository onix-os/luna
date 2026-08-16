//! The stdlib surface that was previously unimplemented: default streams, `package` search, native
//! `table.sort`/`move` fast paths, and argument positions in conversion errors.

use luna::{BadArgument, Closure, Executor, ExternError, Lua};

fn eval<T: for<'gc> luna::FromMultiValue<'gc> + 'static>(source: &str) -> Result<T, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<T>(&executor)
}

#[test]
fn io_redirects_its_default_streams() -> Result<(), ExternError> {
    // `io.output(name)` must redirect the *existing* `io.write`, not just hand back a new handle.
    assert_eq!(
        eval::<String>(
            r#"
            local path = os.tmpname()
            local previous = io.output()
            io.output(path)
            io.write("redirected")
            io.flush()
            io.output(previous)
            io.input(path)
            local got = io.read("a")
            io.input(io.stdin)
            os.remove(path)
            return got
        "#
        )?,
        "redirected"
    );
    Ok(())
}

#[test]
fn io_lines_with_no_name_reads_the_default_input() -> Result<(), ExternError> {
    assert_eq!(
        eval::<i64>(
            r#"
            local path = os.tmpname()
            local f = io.open(path, "w")
            f:write("a\nb\nc\n")
            f:close()
            io.input(path)
            local n = 0
            for _ in io.lines() do n = n + 1 end
            io.input(io.stdin)
            os.remove(path)
            return n
        "#
        )?,
        3
    );
    Ok(())
}

#[test]
fn tmpfile_round_trips_and_setvbuf_answers() -> Result<(), ExternError> {
    assert_eq!(
        eval::<String>(
            r#"
            local f = io.tmpfile()
            assert(io.type(f) == "file")
            assert(f:setvbuf("full", 512))
            f:write("xyz")
            f:seek("set", 0)
            local got = f:read("a")
            f:close()
            return got
        "#
        )?,
        "xyz"
    );
    Ok(())
}

#[test]
fn package_exposes_config_searchers_and_searchpath() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            -- config is five lines, the first being the directory separator
            local sep = package.config:match("^([^\n]+)")
            assert(sep == "/" or sep == "\\", "bad separator")
            assert(select('#', package.config:gsub("\n", "")) > 0)
            -- searchers reflects what require consults: preload, then the Lua file searcher
            assert(#package.searchers == 2)
            -- searchpath reports every candidate it tried when it finds nothing
            local found, tried = package.searchpath("nope", "/nonexistent/?.lua")
            assert(found == nil and tried:match("no file"))
            return true
        "#
        )?,
        true
    );
    Ok(())
}

/// The native fast path must agree with the Lua implementation it bypasses.
#[test]
fn table_sort_and_move_agree_with_the_fallback() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            local function eq(a, b)
                if #a ~= #b then return false end
                for i = 1, #a do if a[i] ~= b[i] then return false end end
                return true
            end

            -- fast paths
            local n = {5, 3, 1, 4, 2}; table.sort(n); assert(eq(n, {1, 2, 3, 4, 5}))
            local s = {"pear", "apple", "fig"}; table.sort(s)
            assert(eq(s, {"apple", "fig", "pear"}))

            -- a comparator still routes to the Lua version
            local c = {1, 2, 3, 4}
            table.sort(c, function(x, y) return x > y end)
            assert(eq(c, {4, 3, 2, 1}))

            -- overlapping moves, in both directions
            assert(eq(table.move({1,2,3,4,5}, 1, 4, 2), {1,1,2,3,4}))
            assert(eq(table.move({1,2,3,4,5}, 2, 5, 1), {2,3,4,5,5}))
            assert(eq(table.move({1,2,3}, 1, 3, 1, {}), {1,2,3}))

            -- a metatable means __index may run Lua, so the fallback must take over
            local proxy = setmetatable({}, {__index = function(_, k) return k * 10 end})
            assert(eq(table.move(proxy, 1, 3, 1, {}), {10, 20, 30}))
            return true
        "#
        )?,
        true
    );
    Ok(())
}

/// A conversion failure names the argument that caused it.
#[test]
fn conversion_errors_carry_the_argument_position() {
    let mut lua = Lua::full();
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(ctx, Some("probe"), b"return string.rep('x', {})")?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    let err = lua.execute::<()>(&executor).unwrap_err();
    assert!(
        err.to_string().contains("bad argument #2"),
        "expected the second argument to be blamed, got {err}"
    );

    // And the position is available as data, not only as text — which is the point of the type.
    let found = match &err {
        ExternError::Runtime(runtime) => runtime.downcast::<BadArgument>(),
        ExternError::Lua(_) => None,
    }
    .expect("a BadArgument");
    assert_eq!(found.argument, 2);
    assert_eq!(found.source.found, "table");
}
