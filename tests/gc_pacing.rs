//! Collection is a job the host schedules, not something that happens to it.
//!
//! `Lua::enter` used to run the mutator *and* decide whether to collect, welded together. The
//! consequence was that `gc_stop` drove the arena's debt to zero, which silently disabled an
//! explicit `gc_step` as well — you could stop the collector but then not step it by hand.

use luna::{Closure, Executor, Lua};

/// Allocate a few thousand dead tables and report memory before and after.
fn churn(lua: &mut Lua) {
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                &br#"
                    for i = 1, 3000 do local t = { i, i, i, i } end
                    return true
                "#[..],
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    lua.execute::<bool>(&executor).unwrap();
}

#[test]
fn pacing_is_automatic_by_default() {
    let lua = Lua::core();
    assert!(lua.gc_is_automatic());
    assert!(lua.gc_is_running());
}

/// The bug this phase existed to fix: after `stop`, an explicit `step` must still do work.
///
/// It never did before, because stopping was implemented by driving the arena's debt to zero and
/// a step had nothing to act on.
#[test]
fn an_explicit_step_works_after_stop() {
    let mut lua = Lua::core();
    lua.gc_stop();
    assert!(!lua.gc_is_automatic());

    churn(&mut lua);
    let before = lua.total_memory();

    // `None` means "regardless of debt", which is what asking explicitly should mean. Enough
    // slices to get through a whole cycle twice — see `gc_collect_works_after_stop` for why two.
    for _ in 0..2000 {
        lua.gc_step(None);
    }
    let after = lua.total_memory();

    assert!(
        after < before,
        "an explicit step after stop must reclaim: {before} -> {after}"
    );
}

/// A full collection also still works with the collector stopped.
///
/// **Two cycles are required, and that is by design.** Two-stage finalization *resurrects*
/// every registered thread during `prepare` so that its open upvalues can be marked; the object
/// is only found genuinely dead on the following cycle. PUC-Rio needs two `collectgarbage("collect")`
/// calls to reclaim finalizable objects for the same reason.
#[test]
fn gc_collect_works_after_stop() {
    let mut lua = Lua::core();
    lua.gc_stop();
    churn(&mut lua);
    let before = lua.total_memory();
    lua.gc_collect();
    lua.gc_collect();
    assert!(
        lua.total_memory() < before,
        "two collections after stop must reclaim: {before} -> {}",
        lua.total_memory()
    );
}

#[test]
fn restart_returns_to_automatic() {
    let mut lua = Lua::core();
    lua.gc_stop();
    lua.gc_restart();
    assert!(lua.gc_is_automatic());
    assert!(lua.gc_is_running());
}

/// Taking the schedule over stops `enter` collecting on its own.
#[test]
fn set_gc_pacing_hands_the_schedule_to_the_host() {
    let mut lua = Lua::core();
    lua.set_gc_pacing(false);
    assert!(!lua.gc_is_automatic());
    lua.set_gc_pacing(true);
    assert!(lua.gc_is_automatic());
}

/// The default path is unchanged: a host that never touches any of this still gets collection.
#[test]
fn the_default_path_still_collects() {
    let mut lua = Lua::core();
    churn(&mut lua);
    let after_churn = lua.total_memory();
    churn(&mut lua);
    // Memory should not grow without bound across repeated churn.
    assert!(lua.total_memory() < after_churn * 4);
}
