//! `collectgarbage` verbs.
//!
//! Acting on the collector needs `&mut Lua`, which a callback never has, so the verbs leave a
//! request that the host carries out at the end of the slice.

use luna::{Closure, Executor, ExternError};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = common::core();
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
    let mut lua = common::core();

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
    let mut lua = common::core();
    assert!(lua.gc_is_running());
    lua.gc_stop();
    assert!(!lua.gc_is_running());
    lua.gc_restart();
    assert!(lua.gc_is_running());
}

mod common;

#[test]
fn lua_reports_the_host_pacing_state_after_allocations() -> Result<(), ExternError> {
    let mut lua = common::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &b"local t={} for i=1,100 do t[i]={i} end return collectgarbage('isrunning')"[..],
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.gc_stop();
    assert!(!lua.execute::<bool>(&executor)?);
    assert!(!lua.gc_is_running());
    Ok(())
}

#[test]
fn stop_survives_collection_and_restart_is_visible_immediately() -> Result<(), ExternError> {
    for request in [
        "collectgarbage()",
        "collectgarbage(nil)",
        "collectgarbage('collect')",
        "collectgarbage('step')",
    ] {
        let mut lua = common::core();
        let source = format!("collectgarbage('stop'); {request}; return true");
        let executor = lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, None, source.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        assert!(lua.execute::<bool>(&executor)?);
        assert!(!lua.gc_is_running(), "{request}");
    }
    assert!(eval(
        r#"
        collectgarbage('stop')
        assert(not collectgarbage('isrunning'))
        collectgarbage('restart')
        assert(collectgarbage('isrunning'))
        collectgarbage('stop')
        return not collectgarbage('isrunning')
    "#
    )?);
    Ok(())
}

/// A script can observe its own `collectgarbage("collect")`.
///
/// Collection cannot happen while the arena is borrowed, so the verb is a request the host carries
/// out between slices. It also interrupts the slice, which is what makes "between slices" land
/// before the next statement rather than whenever the fuel happens to run out — without that, a
/// script could collect and still see its own garbage on the following line.
#[test]
fn collect_takes_effect_before_the_next_statement() -> Result<(), ExternError> {
    for request in [
        "collectgarbage(\"collect\")",
        "collectgarbage()",
        "collectgarbage(nil)",
    ] {
        assert!(
            eval(
                &r#"
            local finalized = false
            local weak = setmetatable({}, { __mode = "v" })
            local function make()
                weak.entry = setmetatable({}, { __gc = function() finalized = true end })
            end
            make()
            GC_REQUEST
            GC_REQUEST
            return weak.entry == nil and finalized
        "#
                .replace("GC_REQUEST", request)
            )?,
            "{request}"
        );
    }
    Ok(())
}

#[test]
fn default_and_nil_collection_return_the_explicit_result() -> Result<(), ExternError> {
    assert!(eval(
        "return collectgarbage()==0 and collectgarbage(nil)==0 and collectgarbage('collect')==0"
    )?);
    Ok(())
}
