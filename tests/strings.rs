use std::io::{stdout, Write};

use luna::{Closure, Executor, ExternError, Lua};

fn run_lua_file(name: &str) -> Result<(), ExternError> {
    let source = std::fs::read(name).expect("could not read test file");
    let mut lua = Lua::full();
    let exec = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some(name), &source)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<()>(&exec)?;
    Ok(())
}

#[test]
fn test_strings_lua() {
    let _ = writeln!(stdout(), "running tests/strings.lua");
    match run_lua_file("tests/strings.lua") {
        Ok(()) => {
            let _ = writeln!(stdout(), "tests/strings.lua passed");
        }
        Err(e) => {
            panic!("tests/strings.lua failed: {:?}", e);
        }
    }
}
