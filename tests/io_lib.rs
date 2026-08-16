//! File handles, built on `std::fs` — no C anywhere.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

fn scratch(name: &str) -> String {
    let dir = std::env::temp_dir().join("luna_io_test");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name).display().to_string().replace('\\', "\\\\")
}

#[test]
fn write_then_read_a_whole_file() -> Result<(), ExternError> {
    let path = scratch("whole.txt");
    assert!(eval(&format!(
        r#"
        local f = assert(io.open("{path}", "w"))
        f:write("hello ", 42, "\n")
        f:close()

        local r = assert(io.open("{path}", "r"))
        local all = r:read("a")
        r:close()
        return all == "hello 42\n"
    "#
    ))?);
    Ok(())
}

#[test]
fn read_lines_one_at_a_time() -> Result<(), ExternError> {
    let path = scratch("lines.txt");
    assert!(eval(&format!(
        r#"
        local f = assert(io.open("{path}", "w"))
        f:write("one\ntwo\nthree\n")
        f:close()

        local r = assert(io.open("{path}", "r"))
        local a, b = r:read("l"), r:read("l")
        local rest = r:read("a")
        r:close()
        return a == "one" and b == "two" and rest == "three\n"
    "#
    ))?);
    Ok(())
}

#[test]
fn io_lines_iterates() -> Result<(), ExternError> {
    let path = scratch("iter.txt");
    assert!(eval(&format!(
        r#"
        local f = assert(io.open("{path}", "w"))
        f:write("a\nb\nc\n")
        f:close()

        local out = {{}}
        for line in io.lines("{path}") do out[#out + 1] = line end
        return #out == 3 and out[1] == "a" and out[3] == "c"
    "#
    ))?);
    Ok(())
}

#[test]
fn opening_a_missing_file_returns_nil_and_a_message() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local f, err = io.open("/definitely/not/here.txt", "r")
        return f == nil and type(err) == "string"
    "#
    )?);
    Ok(())
}

#[test]
fn io_type_reports_handles() -> Result<(), ExternError> {
    let path = scratch("typed.txt");
    assert!(eval(&format!(
        r#"
        local f = assert(io.open("{path}", "w"))
        local open_kind = io.type(f)
        f:close()
        local closed_kind = io.type(f)
        return open_kind == "file" and closed_kind == "closed file"
            and io.type(42) == nil and io.type(io.stdout) == "file"
    "#
    ))?);
    Ok(())
}

#[test]
fn standard_streams_exist_and_write() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        io.stderr:write("")
        io.write("")
        return io.stdout ~= nil and io.stderr ~= nil and io.stdin ~= nil
    "#
    )?);
    Ok(())
}

/// Closing a standard stream must not take it away from the process.
#[test]
fn closing_a_standard_stream_is_a_no_op() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        io.stdout:close()
        io.stdout:write("")
        return io.type(io.stdout) == "file"
    "#
    )?);
    Ok(())
}

#[test]
fn appending_and_seeking() -> Result<(), ExternError> {
    let path = scratch("seek.txt");
    assert!(eval(&format!(
        r#"
        local f = assert(io.open("{path}", "w"))
        f:write("0123456789")
        f:close()

        local a = assert(io.open("{path}", "a"))
        a:write("AB")
        a:close()

        local r = assert(io.open("{path}", "r"))
        r:seek("set", 10)
        local tail = r:read("a")
        r:close()
        return tail == "AB"
    "#
    ))?);
    Ok(())
}

#[test]
fn using_a_closed_file_errors() -> Result<(), ExternError> {
    let path = scratch("closed.txt");
    assert!(eval(&format!(
        r#"
        local f = assert(io.open("{path}", "w"))
        f:close()
        local ok = pcall(function() return f:read("a") end)
        return ok == false
    "#
    ))?);
    Ok(())
}
