use std::io::{stdout, Write};

use luna::{Closure, Executor, ExternError};

fn run_lua_file(name: &str) -> Result<(), ExternError> {
    let source = std::fs::read(name).expect("could not read test file");
    let mut lua = common::full();
    #[cfg(feature = "jit")]
    if name == "tests/strings.lua" {
        lua.allow_jit_resource_refusal("IR blocks");
    }
    let exec = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some(name), &source)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<()>(&exec)?;
    #[cfg(feature = "jit")]
    if name == "tests/strings.lua"
        && std::env::var("LUNA_TEST_JIT_MODE").as_deref() == Ok("force")
        && lua.jit_capabilities().supported_target
    {
        let report = lua.preparation_report();
        assert!(report.resource_refusals > 0);
        assert_eq!(report.last_resource_refusal, Some("IR blocks"));
    }
    Ok(())
}

mod common;

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
