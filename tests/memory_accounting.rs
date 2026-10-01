use luna::{Closure, Executor, ExecutorMode, Lua};

#[test]
fn memory_metrics_preserve_collector_only_total() {
    let lua = Lua::empty();
    assert_eq!(lua.total_memory(), lua.gc_metrics().total_allocation());
    assert!(lua.accounted_memory() >= lua.total_memory());
    #[cfg(not(feature = "jit"))]
    assert_eq!(lua.accounted_memory(), lua.total_memory());
}

#[test]
fn final_result_allocation_is_checked_before_finish_returns() {
    let mut lua = Lua::core();
    lua.gc_stop();
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return string.rep('x', 65536)").unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    lua.set_memory_limit(Some(lua.accounted_memory() + 4096));
    lua.finish(&executor).unwrap();
    assert_eq!(
        lua.enter(|ctx| ctx.fetch(&executor).mode()),
        ExecutorMode::Stopped
    );
}
