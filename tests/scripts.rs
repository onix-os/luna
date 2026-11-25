use std::{
    fs::{read_dir, File},
    io::{stdout, Read, Write},
};

use luna::{io, Closure, Executor, ExternError};

fn run_lua_code(name: &str, code: &[u8]) -> Result<(), ExternError> {
    let mut lua = common::full();
    #[cfg(feature = "jit")]
    if std::path::Path::new(name).file_name() == Some(std::ffi::OsStr::new("math.lua")) {
        lua.allow_jit_resource_refusal("IR blocks");
    }

    let exec = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some(name), code)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    lua.execute::<()>(&exec)?;

    #[cfg(feature = "jit")]
    if std::env::var("LUNA_TEST_JIT_MODE").as_deref() == Ok("force")
        && lua.jit_capabilities().supported_target
        && std::path::Path::new(name).file_name() == Some(std::ffi::OsStr::new("math.lua"))
    {
        let report = lua.preparation_report();
        assert!(report.resource_refusals > 0);
        assert_eq!(report.last_resource_refusal, Some("IR blocks"));
    }

    Ok(())
}

mod common;

fn run_tests(dir: &str) -> bool {
    let _ = writeln!(stdout(), "running all test scripts in {dir:?}");

    let mut file_failed = false;
    for dir in read_dir(dir).expect("could not list dir contents") {
        let path = dir.expect("could not read dir entry").path();
        if let Some(ext) = path.extension() {
            if ext == "lua" {
                let mut file = io::buffered_read(File::open(&path).unwrap()).unwrap();
                let mut source = Vec::new();
                file.read_to_end(&mut source).unwrap();

                let _ = writeln!(stdout(), "running {:?}", path);
                if let Err(err) = run_lua_code(path.to_string_lossy().as_ref(), &source) {
                    let _ = writeln!(stdout(), "error encountered running: {:?}", err);
                    file_failed = true;
                }
            } else {
                let _ = writeln!(stdout(), "skipping file {:?}", path);
            }
        } else {
            let _ = writeln!(stdout(), "skipping file {:?}", path);
        }
    }
    file_failed
}

#[test]
fn test_scripts() {
    // There is no longer a `scripts-wishlist` stage: every script that was in it — `attributes`
    // for `<close>` and `globals` for `_G` — now passes and has moved into the required set.
    if run_tests("./tests/scripts") {
        panic!("one or more errors occurred");
    }
}
