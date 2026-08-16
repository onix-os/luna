//! `collectgarbage` verbs.
//!
//! Acting on the collector needs `&mut Lua`, which a callback never has, so the verbs leave a
//! request that the host carries out at the end of the slice.

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
fn count_still_reports_kilobytes() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return type(collectgarbage("count")) == "number" and collectgarbage("count") > 0
    "#
    )?);
    Ok(())
}

/// The verbs that used to raise "bad argument" now work.
#[test]
fn the_verbs_are_accepted() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        collectgarbage("collect")
        collectgarbage("step")
        collectgarbage("stop")
        collectgarbage("restart")
        collectgarbage()
        return true
    "#
    )?);
    Ok(())
}

#[test]
fn an_unknown_verb_still_errors() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return pcall(collectgarbage, "nonsense") == false
    "#
    )?);
    Ok(())
}

/// A full collection actually reclaims: allocate a large dead structure, then collect.
#[test]
fn collect_reclaims_dead_values() {
    let mut lua = Lua::core();

    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                &br#"
                    for i = 1, 200 do
                        local t = {}
                        for j = 1, 500 do t[j] = "garbage" .. j end
                    end
                    collectgarbage("collect")
                    return true
                "#[..],
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    lua.execute::<bool>(&executor).unwrap();
    let after = lua.total_memory();

    // The dead tables are gone rather than accumulating across all 200 iterations.
    assert!(after < 4 * 1024 * 1024, "still holding {after} bytes");
}

#[test]
fn stop_and_restart_are_reportable_from_rust() {
    let mut lua = Lua::core();
    assert!(lua.gc_is_running());
    lua.gc_stop();
    assert!(!lua.gc_is_running());
    lua.gc_restart();
    assert!(lua.gc_is_running());
}
