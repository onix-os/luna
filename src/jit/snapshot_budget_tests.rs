use super::*;
use allocator_api2::vec::Vec;

#[test]
fn default_and_usage_queries_do_not_materialize_snapshot_ownership() {
    assert_eq!(
        std::mem::size_of::<OnceCell<BudgetAllocator>>(),
        std::mem::size_of::<BudgetAllocator>()
    );
    assert_eq!(
        std::mem::align_of::<OnceCell<BudgetAllocator>>(),
        std::mem::align_of::<BudgetAllocator>()
    );
    let runtime = Runtime::new();
    let manager = runtime.0.borrow();
    let expected = 2 * LedgerRef::allocation_bytes() + RuntimeOwner::allocation_bytes();
    for _ in 0..4 {
        assert!(manager.snapshots.get().is_none());
        assert_eq!(manager.snapshot_current_bytes(), 0);
        assert_eq!(manager.snapshot_peak_bytes(), 0);
        assert_eq!(manager.host.accounted(), expected);
        assert_eq!(manager.host.bootstrap_bytes(), expected);
    }
}

#[test]
fn snapshot_owner_materializes_once_and_clones_share_live_storage() {
    let manager = Manager::default();
    let before = manager.host.accounted();
    let fee = LedgerRef::allocation_bytes();
    let first = manager.snapshot_allocator().unwrap();
    let second = manager.snapshot_allocator().unwrap();
    assert!(LedgerRef::ptr_eq(&first.0, &second.0));
    assert_eq!(manager.host.accounted(), before + fee);
    let mut bytes = Vec::<u8, _>::new_in(first);
    bytes.try_reserve_exact(19).unwrap();
    assert_eq!(second.0.current(), 19);
    assert_eq!(manager.snapshot_current_bytes(), 19);
    assert_eq!(manager.snapshot_peak_bytes(), 19);
    assert_eq!(manager.host.accounted(), before + fee + 19);
    drop(bytes);
    assert_eq!(manager.snapshot_current_bytes(), 0);
    assert_eq!(manager.snapshot_peak_bytes(), 19);
    assert_eq!(manager.host.accounted(), before + fee);
}

#[test]
fn refused_header_keeps_cell_absent_and_recovers_under_latest_limits() {
    let mut manager = Manager::default();
    let before = manager.host.accounted();
    let fee = LedgerRef::allocation_bytes();
    manager.host.set_limit(before + fee - 1);
    assert!(matches!(
        manager.snapshot_allocator(),
        Err(CompileError::ResourceLimit("JIT snapshot ledger"))
    ));
    assert!(manager.snapshots.get().is_none());
    assert_eq!(manager.host.accounted(), before);
    assert_eq!(manager.host.refusals(), 1);
    manager.configure(JitConfig {
        max_snapshot_bytes: 17,
        ..Default::default()
    });
    assert!(manager.snapshots.get().is_none());
    manager.host.set_limit(usize::MAX);
    let allocator = manager.snapshot_allocator().unwrap();
    let mut first = Vec::<u8, _>::new_in(allocator.clone());
    first.try_reserve_exact(17).unwrap();
    let mut refused = Vec::<u8, _>::new_in(allocator);
    assert!(refused.try_reserve_exact(1).is_err());
    assert_eq!(manager.snapshot_current_bytes(), 17);
    assert_eq!(manager.host.accounted(), before + fee + 17);
}

#[test]
fn initialized_budget_and_peaks_survive_disable_and_configuration_changes() {
    let mut manager = Manager::default();
    manager.configure(JitConfig {
        mode: JitMode::Auto,
        max_snapshot_bytes: 32,
        ..Default::default()
    });
    assert!(manager.snapshots.get().is_none());
    let allocator = manager.snapshot_allocator().unwrap();
    let mut live = Vec::<u8, _>::new_in(allocator.clone());
    live.try_reserve_exact(17).unwrap();
    manager.configure(JitConfig {
        mode: JitMode::Off,
        max_snapshot_bytes: 8,
        ..Default::default()
    });
    let retained = manager.snapshot_allocator().unwrap();
    assert!(LedgerRef::ptr_eq(&allocator.0, &retained.0));
    assert_eq!(manager.snapshot_current_bytes(), 17);
    assert_eq!(manager.snapshot_peak_bytes(), 17);
    let mut refused = Vec::<u8, _>::new_in(retained);
    assert!(refused.try_reserve_exact(1).is_err());
    drop(live);
    assert_eq!(manager.snapshot_current_bytes(), 0);
    manager.configure(JitConfig {
        max_snapshot_bytes: 18,
        ..Default::default()
    });
    refused.try_reserve_exact(18).unwrap();
    assert_eq!(manager.snapshot_current_bytes(), 18);
    assert_eq!(manager.snapshot_peak_bytes(), 18);
}

#[test]
fn off_and_idle_auto_execution_preserve_absent_snapshot_owner() {
    use crate::{Closure, Executor, Lua};
    let mut lua = Lua::core();
    let runtime = lua.enter(|ctx| ctx.jit().clone());
    for mode in [JitMode::Off, JitMode::Auto, JitMode::Off] {
        lua.set_jit_config(JitConfig {
            mode,
            hot_threshold: u32::MAX,
            ..Default::default()
        })
        .unwrap();
        let executor = lua.enter(|ctx| {
            let closure =
                Closure::load(ctx, None, b"local n=0 for i=1,16 do n=n+i end return n").unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 136);
        let stats = lua.jit_stats();
        assert_eq!(stats.snapshot_bytes, 0);
        assert_eq!(stats.snapshot_peak_bytes, 0);
        assert_eq!(stats.queued_requests, 0);
        assert!(runtime.0.borrow().snapshots.get().is_none());
        lua.gc_collect();
    }
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn live_source_service_refusal_recovers_and_retains_owner_after_clear() {
    use crate::{Closure, Executor, Lua};
    let mut lua = Lua::empty();
    let runtime = lua.enter(|ctx| ctx.jit().clone());
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        let closure =
            Closure::load(ctx, None, b"local n=0 for i=1,64 do n=n+i end return n").unwrap();
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, closure.prototype())
            .unwrap();
        ctx.jit().0.borrow_mut().enqueue(id, true);
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    lua.set_memory_limit(Some(0));
    assert!(matches!(lua.service_jit(), Err(JitError::ResourceLimit(_))));
    assert!(runtime.0.borrow().snapshots.get().is_none());
    lua.set_memory_limit(None);
    assert!(lua.prepare_jit().unwrap() > 0);
    let allocator = runtime.0.borrow().snapshot_allocator().unwrap();
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 2080);
    assert!(lua.jit_stats().native_instructions > 0);
    assert!(lua.jit_stats().snapshot_peak_bytes > 0);
    lua.set_jit_config(JitConfig::default()).unwrap();
    lua.clear_jit_cache();
    assert!(LedgerRef::ptr_eq(
        &allocator.0,
        &runtime.0.borrow().snapshot_allocator().unwrap().0
    ));
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    assert!(lua.jit_stats().snapshot_peak_bytes > 0);
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn collected_queued_source_is_discarded_before_snapshot_owner_initialization() {
    use crate::{Closure, Lua};
    let mut lua = Lua::empty();
    let runtime = lua.enter(|ctx| ctx.jit().clone());
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, closure.prototype())
            .unwrap();
        ctx.jit().0.borrow_mut().enqueue(id, true);
    });
    lua.gc_collect();
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert!(runtime.0.borrow().snapshots.get().is_none());
    assert_eq!(lua.jit_stats().queued_requests, 0);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}
