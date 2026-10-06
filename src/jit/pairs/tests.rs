use super::*;
#[path = "../../../examples/jit_support/workloads.rs"]
mod workloads;
use crate::{
    opcode::Operation, thread::activation::with_test_thread, Closure, Fuel, Lua, StashedClosure,
};

const SOURCE: &[u8] = b"local n=7 local function f(v) n=n+v end f(2) return n";

fn fixture() -> (Lua, StashedClosure, Key) {
    fixture_source(SOURCE)
}

fn fixture_source(source: &[u8]) -> (Lua, StashedClosure, Key) {
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| ctx.jit().test_call_pairs(true));
    let (closure, key) = load_pair(&mut lua, source);
    (lua, closure, key)
}

fn load_pair(lua: &mut Lua, source: &[u8]) -> (StashedClosure, Key) {
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, source).unwrap();
        let prototype = closure.prototype();
        let registry = ctx.jit_registry().borrow();
        let key = Key {
            caller: registry.identity(ctx, prototype).unwrap(),
            callee: registry.identity(ctx, prototype.prototypes[0]).unwrap(),
            pc: prototype
                .opcodes
                .iter()
                .position(|opcode| matches!(opcode.decode(), Operation::Call { .. }))
                .unwrap(),
        };
        (ctx.stash(closure), key)
    })
}

fn compile_pair(lua: &mut Lua, key: Key) {
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(lua.service_jit().unwrap(), 1);
    assert!(lua.enter(
        |ctx| ctx.jit().0.borrow().pairs.as_ref().unwrap().entries[&key]
            .program
            .is_some()
    ));
}

fn cached_lease(lua: &mut Lua, _closure: &StashedClosure, key: Key) -> super::super::PreparedPair {
    lua.enter(|ctx| super::super::PreparedPair {
        program: ctx.jit().pair_lease(key).unwrap(),
    })
}

#[test]
fn scoped_cached_lease_updates_shared_recency_without_cloning() {
    let (mut lua, closure, key) = fixture();
    compile_pair(&mut lua, key);
    let cached = cached_lease(&mut lua, &closure, key);
    lua.enter(|ctx| {
        let before = ctx.jit().0.borrow().stats;
        let clock = ctx.jit().0.borrow().clock;
        let owners = Shared::strong_count(&cached.program);
        assert!(ctx.jit().touch_pair(&cached));
        assert_eq!(Shared::strong_count(&cached.program), owners);
        let manager = ctx.jit().0.borrow();
        assert_eq!(manager.clock, clock + 1);
        assert_eq!(
            manager.pairs.as_ref().unwrap().entries[&key].last_used,
            manager.clock
        );
        assert_eq!(manager.stats.code_leases, before.code_leases);
        assert_eq!(manager.stats.code_lookups, before.code_lookups + 1);
        assert_eq!(
            manager.stats.native_pair_cache_hits,
            before.native_pair_cache_hits + 1
        );
        drop(manager);
        let mut manager = ctx.jit().0.borrow_mut();
        manager.clock = u64::MAX;
        manager.stats.native_pair_cache_hits = u64::MAX;
        drop(manager);
        assert!(ctx.jit().touch_pair(&cached));
        assert_eq!(ctx.jit().0.borrow().clock, u64::MAX);
        assert_eq!(ctx.jit().0.borrow().stats.native_pair_cache_hits, u64::MAX);
    });
}

#[test]
fn scoped_cached_lease_refuses_retired_replaced_off_and_foreign_owners() {
    let (mut lua, closure, key) = fixture();
    compile_pair(&mut lua, key);
    let cached = cached_lease(&mut lua, &closure, key);
    lua.clear_jit_cache();
    assert!(!lua.enter(|ctx| ctx.jit().touch_pair(&cached)));
    compile_pair(&mut lua, key);
    assert!(!lua.enter(|ctx| ctx.jit().touch_pair(&cached)));
    let fresh = cached_lease(&mut lua, &closure, key);
    assert!(lua.enter(|ctx| ctx.jit().touch_pair(&fresh)));
    let (mut other, _closure, other_key) = fixture();
    assert_eq!(key, other_key);
    compile_pair(&mut other, other_key);
    assert!(!other.enter(|ctx| ctx.jit().touch_pair(&fresh)));
    let before = lua.jit_stats().native_pair_cache_hits;
    let mut config = lua.jit_config();
    config.mode = JitMode::Off;
    lua.set_jit_config(config).unwrap();
    assert!(!lua.enter(|ctx| ctx.jit().touch_pair(&fresh)));
    assert_eq!(lua.jit_stats().native_pair_cache_hits, before);
    drop(cached);
    drop(fresh);
    assert_eq!(lua.jit_stats().code_bytes, 0);
}

fn load_scalar(lua: &mut Lua) -> (StashedClosure, u64) {
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"return 42"[..]).unwrap();
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, closure.prototype())
            .unwrap();
        (ctx.stash(closure), id)
    })
}

fn compile_scalar(lua: &mut Lua, id: u64) {
    lua.enter(|ctx| ctx.jit().0.borrow_mut().enqueue(id, true));
    assert_eq!(lua.service_jit().unwrap(), 1);
    assert!(lua.enter(|ctx| ctx.jit().0.borrow().code.contains_key(&id)));
}

#[test]
fn pair_pressure_evicts_lru_and_bounds_retries_without_resetting_victim_attempts() {
    let (mut lua, _first, first) = fixture();
    compile_pair(&mut lua, first);
    let bytes = lua.jit_stats().code_bytes;
    let (_second, second) = load_pair(&mut lua, SOURCE);
    compile_pair(&mut lua, second);
    assert_eq!(lua.jit_stats().code_bytes, 2 * bytes);
    lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().config.max_code_bytes = 2 * bytes;
        drop(ctx.jit().pair_lease(first).unwrap());
    });
    let (_third, third) = load_pair(&mut lua, SOURCE);
    compile_pair(&mut lua, third);
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        let entries = &manager.pairs.as_ref().unwrap().entries;
        assert!(entries[&first].program.is_some());
        assert!(entries[&second].program.is_none());
        assert!(entries[&third].program.is_some());
        assert_eq!(
            (entries[&second].attempts, entries[&second].hotness),
            (1, 0)
        );
        assert_eq!(entries[&third].attempts, 2);
        assert_eq!(
            (
                manager.stats.cache_evictions,
                manager.stats.compilation_failures
            ),
            (1, 1)
        );
    });
    assert_eq!(lua.jit_stats().code_bytes, 2 * bytes);
    lua.enter(|ctx| ctx.jit().observe_pair(second));
    assert!(matches!(
        lua.service_jit(),
        Err(JitError::ResourceLimit("native mappings"))
    ));
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        assert_eq!(manager.pairs.as_ref().unwrap().entries[&second].attempts, 2);
        assert_eq!(
            (
                manager.stats.cache_evictions,
                manager.stats.compilation_failures
            ),
            (1, 2)
        );
        assert_eq!(manager.snapshots.0.current(), 0);
    });
    lua.enter(|ctx| ctx.jit().observe_pair(second));
    assert_eq!(lua.service_jit().unwrap(), 0);
}

#[test]
fn mapping_pressure_shares_one_lru_order_between_pair_and_scalar_caches() {
    for target_pair in [false, true] {
        let (mut lua, _pair_root, pair) = fixture();
        compile_pair(&mut lua, pair);
        let pair_bytes = lua.jit_stats().code_bytes;
        let (_scalar_root, scalar) = load_scalar(&mut lua);
        compile_scalar(&mut lua, scalar);
        let scalar_bytes = lua.jit_stats().code_bytes - pair_bytes;
        lua.enter(|ctx| {
            let limit = pair_bytes
                + scalar_bytes
                + if target_pair {
                    pair_bytes.saturating_sub(scalar_bytes)
                } else {
                    scalar_bytes.saturating_sub(pair_bytes)
                };
            ctx.jit().0.borrow_mut().config.max_code_bytes = limit;
            if target_pair {
                drop(ctx.jit().pair_lease(pair).unwrap());
            } else {
                drop(ctx.jit().lookup(scalar).unwrap());
            }
        });
        let (_target_root, target) = if target_pair {
            let (root, key) = load_pair(&mut lua, SOURCE);
            compile_pair(&mut lua, key);
            (root, key.caller)
        } else {
            let (root, id) = load_scalar(&mut lua);
            compile_scalar(&mut lua, id);
            (root, id)
        };
        lua.enter(|ctx| {
            let manager = ctx.jit().0.borrow();
            let entries = &manager.pairs.as_ref().unwrap().entries;
            assert_eq!(entries[&pair].program.is_some(), target_pair);
            assert_eq!(manager.code.contains_key(&scalar), !target_pair);
            if target_pair {
                assert_eq!(
                    (
                        manager.tracked[&scalar].attempts,
                        manager.tracked[&scalar].hotness
                    ),
                    (1, 0)
                );
                assert_eq!(
                    entries
                        .iter()
                        .find(|(key, _)| key.caller == target)
                        .unwrap()
                        .1
                        .attempts,
                    2
                );
            } else {
                assert_eq!((entries[&pair].attempts, entries[&pair].hotness), (1, 0));
                assert_eq!(manager.tracked[&target].attempts, 2);
                assert!(manager.code.contains_key(&target));
            }
            assert_eq!(
                (
                    manager.stats.cache_evictions,
                    manager.stats.compilation_failures
                ),
                (1, 1)
            );
        });
        assert_eq!(lua.jit_stats().snapshot_bytes, 0);
        assert!(lua.jit_stats().code_bytes <= lua.jit_config().max_code_bytes);
    }
}

#[test]
fn pinned_pair_leases_refuse_eviction_then_recover_without_resetting_attempts() {
    let (mut lua, closure, key) = fixture();
    compile_pair(&mut lua, key);
    let bytes = lua.jit_stats().code_bytes;
    let lease = lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().config.max_code_bytes = bytes;
        ctx.jit().pair_lease(key).unwrap()
    });
    let (_next_root, next) = load_pair(&mut lua, SOURCE);
    lua.enter(|ctx| ctx.jit().observe_pair(next));
    assert!(matches!(
        lua.service_jit(),
        Err(JitError::ResourceLimit("native mappings"))
    ));
    assert_eq!(lua.jit_stats().cache_eviction_refusals, 1);
    assert_eq!(lua.jit_stats().cache_evictions, 0);
    assert_eq!(lua.jit_stats().code_bytes, bytes);
    lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().config.hot_threshold = u32::MAX;
        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
            assert!(host.run(ctx, 1, key.pc as u32, 4).result.is_ok());
            assert_eq!(lease.invoke(ctx, host, 64), Some((1, 1)));
        });
        ctx.jit().0.borrow_mut().config.hot_threshold = 1;
    });
    drop(lease);
    let (_recovery_root, recovery) = load_pair(&mut lua, SOURCE);
    compile_pair(&mut lua, recovery);
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        let entries = &manager.pairs.as_ref().unwrap().entries;
        assert!(entries[&key].program.is_none());
        assert_eq!(entries[&key].attempts, 1);
        assert_eq!(entries[&next].attempts, 1);
        assert_eq!(entries[&recovery].attempts, 2);
    });
    assert_eq!(lua.jit_stats().cache_evictions, 1);
    assert_eq!(lua.jit_stats().code_bytes, bytes);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn exhausted_pair_retry_budget_preserves_an_unpinned_peer() {
    let (mut lua, _root, first) = fixture();
    compile_pair(&mut lua, first);
    let bytes = lua.jit_stats().code_bytes;
    lua.enter(|ctx| {
        let mut manager = ctx.jit().0.borrow_mut();
        manager.config.max_code_bytes = bytes;
        manager.config.max_compile_attempts = 1;
    });
    let (_next_root, next) = load_pair(&mut lua, SOURCE);
    lua.enter(|ctx| ctx.jit().observe_pair(next));
    assert!(matches!(
        lua.service_jit(),
        Err(JitError::ResourceLimit("native mappings"))
    ));
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        let entries = &manager.pairs.as_ref().unwrap().entries;
        assert!(entries[&first].program.is_some());
        assert_eq!(entries[&next].attempts, 1);
        assert_eq!(
            (
                manager.stats.cache_evictions,
                manager.stats.cache_eviction_refusals
            ),
            (0, 0)
        );
    });
    lua.enter(|ctx| ctx.jit().observe_pair(next));
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert_eq!(lua.jit_stats().code_bytes, bytes);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn saturated_pair_recency_uses_source_identity_tiebreak() {
    let (mut lua, _first_root, first) = fixture();
    compile_pair(&mut lua, first);
    let bytes = lua.jit_stats().code_bytes;
    let (_second_root, second) = load_pair(&mut lua, SOURCE);
    compile_pair(&mut lua, second);
    lua.enter(|ctx| {
        let mut manager = ctx.jit().0.borrow_mut();
        manager.config.max_code_bytes = 2 * bytes;
        manager.clock = u64::MAX;
        drop(manager);
        drop(ctx.jit().pair_lease(second).unwrap());
        drop(ctx.jit().pair_lease(first).unwrap());
    });
    let (_third_root, third) = load_pair(&mut lua, SOURCE);
    compile_pair(&mut lua, third);
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        let entries = &manager.pairs.as_ref().unwrap().entries;
        assert_eq!(manager.clock, u64::MAX);
        assert!(entries[&first].program.is_none());
        assert!(entries[&second].program.is_some());
        assert!(entries[&third].program.is_some());
        assert_eq!(entries[&second].last_used, u64::MAX);
        assert_eq!(entries[&third].last_used, u64::MAX);
    });
}

#[test]
fn retry_owner_refusal_releases_mappings_without_resetting_victim_attempts() {
    let (mut lua, _root, first) = fixture();
    compile_pair(&mut lua, first);
    let bytes = lua.jit_stats().code_bytes;
    lua.enter(|ctx| {
        let mut manager = ctx.jit().0.borrow_mut();
        manager.config.max_code_bytes = bytes;
        manager.memory_failure = super::super::backend::Failure::RefuseOwnerAllocation;
    });
    let (_next_root, next) = load_pair(&mut lua, SOURCE);
    lua.enter(|ctx| ctx.jit().observe_pair(next));
    assert!(matches!(
        lua.service_jit(),
        Err(JitError::ResourceLimit("JIT metadata"))
    ));
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        let entries = &manager.pairs.as_ref().unwrap().entries;
        assert!(entries[&first].program.is_none());
        assert!(entries[&next].program.is_none());
        assert_eq!((entries[&first].attempts, entries[&first].hotness), (1, 0));
        assert_eq!(entries[&next].attempts, 2);
        assert_eq!(
            (
                manager.stats.cache_evictions,
                manager.stats.compilation_failures
            ),
            (1, 2)
        );
    });
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn service_prepares_weak_source_pair_outside_arena_and_lease_executes_canonical_frames() {
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| {
        ctx.jit().observe_pair(key);
        ctx.jit().observe_pair(key);
        assert_eq!(ctx.jit().0.borrow().stats.queued_requests, 1);
        assert!(ctx.jit().pair_lease(key).is_none());
    });
    assert_eq!(lua.service_jit().unwrap(), 1);
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert_eq!(lua.jit_stats().queued_requests, 0);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    assert!(lua.jit_stats().code_bytes > 0);
    let lease = lua.enter(|ctx| ctx.jit().pair_lease(key).unwrap());
    lua.clear_jit_cache();
    assert!(lua.enter(|ctx| ctx.jit().pair_lease(key).is_none()));
    lua.enter(|ctx| {
        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
            assert!(host.run(ctx, 1, key.pc as u32, 4).result.is_ok());
            assert_eq!(lease.invoke(ctx, host, 64), Some((1, 1)));
            host.with_registers(|_, registers| {
                assert!(registers
                    .stack_frame
                    .iter()
                    .any(|value| matches!(value, crate::Value::Integer(9))))
            });
        });
    });
    assert!(lua.jit_stats().code_bytes > 0);
    drop(lease);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
}

#[test]
fn dead_sources_cancel_pair_requests_and_retire_code_without_revoking_a_lease() {
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    drop(closure);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().queued_requests, 0);
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert_eq!(lua.jit_stats().registered_prototypes, 0);
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(lua.service_jit().unwrap(), 1);
    let lease = lua.enter(|ctx| ctx.jit().pair_lease(key).unwrap());
    drop(closure);
    lua.gc_collect();
    lua.gc_collect();
    assert!(lua.enter(|ctx| ctx.jit().pair_lease(key).is_none()));
    assert_eq!(lua.jit_stats().registered_prototypes, 0);
    assert!(lua.jit_stats().code_bytes > 0);
    drop(lease);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
}

#[test]
fn shared_queue_capacity_and_hotness_preserve_normal_requests() {
    let (mut lua, _closure, key) = fixture();
    let mut config = lua.jit_config();
    config.hot_threshold = 2;
    config.max_queue_entries = 1;
    lua.set_jit_config(config).unwrap();
    lua.enter(|ctx| {
        ctx.jit().observe_pair(key);
        assert_eq!(ctx.jit().0.borrow().stats.queued_requests, 0);
        ctx.jit().0.borrow_mut().enqueue(key.caller, true);
        ctx.jit().observe_pair(key);
        assert_eq!(ctx.jit().0.borrow().stats.queued_requests, 1);
        assert_eq!(ctx.jit().0.borrow_mut().next_request(), Some(key.caller));
        ctx.jit().observe_pair(key);
        ctx.jit().0.borrow_mut().enqueue(key.callee, true);
        assert_eq!(ctx.jit().0.borrow().stats.queued_requests, 1);
        assert!(ctx.jit().0.borrow_mut().next_request().is_none());
        assert_eq!(ctx.jit().next_pair_request(), Some(key));
    });
}

#[test]
fn vm_observes_real_call_pairs_but_never_compiles_within_a_step() {
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| {
        let executor = crate::Executor::start(ctx, ctx.fetch(&closure).into(), ());
        let mut fuel = Fuel::with(65536);
        while !executor.step(ctx, &mut fuel).unwrap() {}
        assert_eq!(executor.take_result::<i64>(ctx).unwrap().unwrap(), 9);
        let manager = ctx.jit().0.borrow();
        assert!(manager.pairs.as_ref().unwrap().entries[&key].queued);
        assert_eq!(manager.stats.installed_regions, 0);
        assert_eq!(manager.memory.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(manager.snapshots.0.current(), 0);
    });
    let mut serviced = 0;
    while lua.service_jit().unwrap() != 0 {
        serviced += 1;
        assert!(serviced <= 3);
    }
    assert_eq!(serviced, 3);
    assert!(lua.enter(|ctx| ctx.jit().pair_lease(key).is_some()));
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn queue_shrinking_and_off_configuration_clear_flags_and_owned_programs() {
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    let mut config = lua.jit_config();
    config.max_queue_entries = 1;
    lua.enter(|ctx| ctx.jit().0.borrow_mut().enqueue(key.caller, true));
    lua.set_jit_config(config).unwrap();
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        assert_eq!(manager.stats.queued_requests, 1);
        assert!(!manager.pairs.as_ref().unwrap().entries[&key].queued);
    });
    lua.clear_jit_cache();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(lua.service_jit().unwrap(), 1);
    let lease = lua.enter(|ctx| ctx.jit().pair_lease(key).unwrap());
    let mut config = lua.jit_config();
    config.mode = JitMode::Off;
    lua.set_jit_config(config).unwrap();
    assert!(lua.enter(|ctx| ctx.jit().pair_lease(key).is_none()));
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(lua.jit_stats().queued_requests, 0);
    lua.enter(|ctx| {
        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
            assert!(host.run(ctx, 1, key.pc as u32, 4).result.is_ok());
            let fuel = host.fuel().remaining();
            assert_eq!(lease.invoke(ctx, host, 64), None);
            assert_eq!(host.fuel().remaining(), fuel);
            host.with_registers(|_, registers| assert_eq!(*registers.pc, key.pc));
        });
    });
    assert!(lua.jit_stats().code_bytes > 0);
    drop(lease);
    assert_eq!(lua.jit_stats().code_bytes, 0);
}

#[test]
fn snapshot_mapping_and_late_owner_refusals_reclaim_resources_and_bound_retries() {
    use crate::jit::backend::Failure;

    for failure in [
        Failure::Allocate,
        Failure::Protect,
        Failure::ProtectAfterFirst,
        Failure::RefuseOwnerStorage,
        Failure::RefuseOwnerAllocation,
    ] {
        let (mut lua, _closure, key) = fixture();
        lua.enter(|ctx| {
            ctx.jit().0.borrow_mut().memory_failure = failure;
            ctx.jit().observe_pair(key);
        });
        assert!(lua.service_jit().is_err());
        assert_eq!(lua.jit_stats().snapshot_bytes, 0);
        assert_eq!(lua.jit_stats().code_bytes, 0);
        assert_eq!(lua.jit_stats().code_requested_bytes, 0);
        assert!(lua.enter(|ctx| ctx.jit().pair_lease(key).is_none()));
        assert_eq!(lua.jit_stats().compilation_failures, 1);
    }
    let (mut lua, _closure, key) = fixture();
    let mut config = lua.jit_config();
    config.max_snapshot_bytes = 1;
    lua.set_jit_config(config).unwrap();
    for attempt in 1..=2 {
        lua.enter(|ctx| ctx.jit().observe_pair(key));
        assert!(lua.service_jit().is_err());
        assert_eq!(lua.jit_stats().compilation_failures, attempt);
        assert_eq!(lua.jit_stats().snapshot_bytes, 0);
        assert_eq!(lua.jit_stats().code_bytes, 0);
    }
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert_eq!(lua.jit_stats().queued_requests, 0);
}

#[test]
fn metadata_queue_refusal_is_optional_and_recovers_after_headroom_returns() {
    let (mut lua, _closure, key) = fixture();
    lua.enter(|ctx| {
        let metadata = ctx.jit().0.borrow().metadata.0.clone();
        let limit = metadata.limit();
        let before = metadata.current();
        metadata.set_limit(before);
        ctx.jit().observe_pair(key);
        assert_eq!(ctx.jit().0.borrow().stats.queued_requests, 0);
        assert_eq!(metadata.current(), before);
        metadata.set_limit(limit);
        ctx.jit().observe_pair(key);
    });
    assert_eq!(lua.service_jit().unwrap(), 1);
    assert!(lua.enter(|ctx| ctx.jit().pair_lease(key).is_some()));
}

#[test]
fn retirement_during_compile_prevents_late_installation() {
    let (mut lua, _closure, key) = fixture();
    lua.enter(|ctx| {
        ctx.jit().observe_pair(key);
        let owner = ctx.jit().0.clone();
        ctx.jit().0.borrow_mut().before_compile =
            Some(Box::new(move || owner.borrow_mut().retire(key.callee)));
    });
    assert_eq!(lua.service_jit().unwrap(), 1);
    assert!(lua.enter(|ctx| ctx.jit().pair_lease(key).is_none()));
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn leases_reject_other_states_even_when_their_source_ids_coincide() {
    let (mut origin, _closure, key) = fixture();
    origin.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(origin.service_jit().unwrap(), 1);
    let lease = origin.enter(|ctx| ctx.jit().pair_lease(key).unwrap());
    let (mut other, closure, other_key) = fixture();
    assert_eq!(key, other_key);
    other.enter(|ctx| {
        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
            assert!(host.run(ctx, 1, key.pc as u32, 4).result.is_ok());
            let original = host.test_trace();
            assert_eq!(lease.invoke(ctx, host, 64), None);
            let current = host.test_trace();
            assert_eq!(original.0, current.0);
            assert_eq!((original.2, original.3), (current.2, current.3));
            host.with_registers(|_, registers| assert_eq!(*registers.pc, key.pc));
            assert_eq!(ctx.jit().0.borrow().stats.native_instructions, 0);
        });
    });
}

#[test]
fn mapping_quota_refusal_preserves_and_executes_an_existing_peer_lease() {
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(lua.service_jit().unwrap(), 1);
    let lease = lua.enter(|ctx| ctx.jit().pair_lease(key).unwrap());
    let retained = (
        lua.jit_stats().code_bytes,
        lua.jit_stats().code_requested_bytes,
    );
    let mut config = lua.jit_config();
    config.max_code_bytes = retained.0;
    lua.set_jit_config(config).unwrap();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert!(matches!(
        lua.service_jit(),
        Err(JitError::ResourceLimit("native mappings"))
    ));
    assert_eq!(
        (
            lua.jit_stats().code_bytes,
            lua.jit_stats().code_requested_bytes
        ),
        retained
    );
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    lua.enter(|ctx| {
        with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(10000), |host| {
            assert!(host.run(ctx, 1, key.pc as u32, 4).result.is_ok());
            assert_eq!(lease.invoke(ctx, host, 64), Some((1, 1)));
        });
    });
    drop(lease);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
}

#[test]
fn collected_independent_callee_cancels_requests_without_retiring_the_caller() {
    let (mut lua, _closure, mut key) = fixture();
    let callee_root = lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, SOURCE).unwrap();
        key.callee = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, closure.prototype().prototypes[0])
            .unwrap();
        ctx.stash(closure)
    });
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    drop(callee_root);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().queued_requests, 0);
    assert_eq!(lua.jit_stats().registered_prototypes, 2);
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert!(lua.enter(|ctx| ctx
        .jit_registry()
        .borrow()
        .resolve(ctx, key.caller)
        .is_some()));
}

#[test]
fn second_snapshot_refusal_releases_the_first_snapshot_and_consumes_one_attempt() {
    let (mut lua, closure, key) = fixture();
    let caller_bytes = lua.enter(|ctx| {
        let allocator = ctx.jit().0.borrow().snapshots.clone();
        let snapshot =
            Snapshot::new_in(&ctx.fetch(&closure).prototype(), 4096, allocator.clone()).unwrap();
        let bytes = allocator.0.current();
        drop(snapshot);
        assert_eq!(allocator.0.current(), 0);
        bytes
    });
    let mut config = lua.jit_config();
    config.max_snapshot_bytes = caller_bytes + 1;
    lua.set_jit_config(config).unwrap();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert!(matches!(lua.service_jit(), Err(JitError::ResourceLimit(_))));
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().compilation_failures, 1);
    assert_eq!(lua.jit_stats().queued_requests, 0);
    lua.enter(|ctx| {
        assert_eq!(
            ctx.jit().0.borrow().pairs.as_ref().unwrap().entries[&key].attempts,
            1
        )
    });
}

#[test]
fn selected_pairs_match_canonical_prefixes_fuel_limits_and_declined_fallbacks() {
    for source in [
        SOURCE,
        &b"local n=7.5 local function f(v) n=n+v end f(2) return n"[..],
        &b"local n=7 local function f(v) n=n+v end f(2.5) return n"[..],
    ] {
        let (mut lua, closure, key) = fixture_source(source);
        lua.enter(|ctx| ctx.jit().observe_pair(key));
        assert_eq!(lua.service_jit().unwrap(), 1);
        for budget in [1, 2, 3, 4, 5, 64] {
            for limit in [1, 2, 64] {
                for fuel in [-1, 0, 1, 8, 9, 10, 16, 20, 64, 10000] {
                    let run = |lua: &mut Lua, select| {
                        lua.enter(|ctx| {
                            with_test_thread(
                                ctx,
                                ctx.fetch(&closure),
                                &mut Fuel::with(fuel),
                                |host| {
                                    host.test_pair_selection(select);
                                    let before = ctx.jit().0.borrow().stats;
                                    let outcome = host.run(ctx, limit, budget, 4);
                                    assert!(outcome.result.is_ok());
                                    let after = ctx.jit().0.borrow().stats;
                                    let (frames, values, open, remaining) = host.test_trace();
                                    let slots = values
                                        .into_iter()
                                        .map(|value| {
                                            let slot = crate::jit::abi::Slot::from_value(value);
                                            (slot.tag, slot.bits)
                                        })
                                        .collect::<Vec<_>>();
                                    (
                                        frames,
                                        slots,
                                        open,
                                        remaining,
                                        outcome.activations,
                                        after.total_dispatches - before.total_dispatches,
                                        after.native_instructions + after.interpreted_instructions
                                            - before.native_instructions
                                            - before.interpreted_instructions,
                                    )
                                },
                            )
                        })
                    };
                    let reference = run(&mut lua, false);
                    let selected = run(&mut lua, true);
                    assert_eq!(
                        selected, reference,
                        "source={source:?} budget={budget} limit={limit} fuel={fuel}"
                    );
                }
            }
        }
        let completed = lua.enter(|ctx| ctx.jit().test_pair_executions().1);
        assert_eq!(completed > 0, source == SOURCE);
    }
}

#[test]
fn cached_native_caller_prefixes_handoff_and_release_before_nested_callbacks() {
    use crate::{Callback, CallbackReturn, Executor};
    use std::{cell::Cell, rc::Rc};

    let source = b"local cb=... local n=0 local function f(v) n=n+v end for i=1,30 do f(2) end cb() return n";
    let (mut lua, closure, key) = fixture_source(source);
    lua.enter(|ctx| {
        ctx.jit().test_activation_host(64);
        ctx.jit().0.borrow_mut().enqueue(key.caller, true);
        ctx.jit().0.borrow_mut().enqueue(key.callee, true);
        ctx.jit().observe_pair(key);
    });
    for _ in 0..3 {
        assert_eq!(lua.service_jit().unwrap(), 1);
    }
    assert_eq!(lua.service_jit().unwrap(), 0);
    let called = Rc::new(Cell::new(false));
    let saved = called.clone();
    let executor = lua.enter(|ctx| {
        let child = Closure::load(ctx, None, b"local n=0 for i=1,7 do n=n+i end return n").unwrap();
        let callback = Callback::from_fn_with(&ctx, child, move |child, ctx, _, mut stack| {
            assert!(ctx.jit().test_pair_executions().1 > 0);
            let nested = Executor::start(ctx, (*child).into(), ());
            let mut fuel = Fuel::with(65536);
            while !nested.step(ctx, &mut fuel).unwrap() {}
            assert_eq!(nested.take_result::<i64>(ctx).unwrap().unwrap(), 28);
            saved.set(true);
            stack.clear();
            Ok(CallbackReturn::Return)
        });
        ctx.stash(Executor::start(
            ctx,
            ctx.fetch(&closure).into(),
            (callback,),
        ))
    });
    let mut done = false;
    for _ in 0..1000 {
        done = lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut Fuel::with(64)).unwrap());
        lua.gc_collect();
        if done {
            break;
        }
    }
    assert!(done && called.get());
    lua.enter(|ctx| {
        assert_eq!(
            ctx.fetch(&executor)
                .take_result::<i64>(ctx)
                .unwrap()
                .unwrap(),
            60
        );
        assert!(ctx.jit().test_pair_executions().1 > 0);
    });
}

#[test]
fn native_transition_handoffs_preserve_single_host_run_traces() {
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| {
        ctx.jit().test_activation_host(64);
        ctx.jit().0.borrow_mut().enqueue(key.caller, true);
        ctx.jit().0.borrow_mut().enqueue(key.callee, true);
        ctx.jit().observe_pair(key);
    });
    for _ in 0..3 {
        assert_eq!(lua.service_jit().unwrap(), 1);
    }
    for fuel in [-1, 0, 1, 8, 9, 10, 16, 20, 64, 10000] {
        let run = |lua: &mut Lua, selected| {
            lua.enter(|ctx| {
                with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(fuel), |host| {
                    host.test_pair_selection(selected);
                    let before = ctx.jit().0.borrow().stats;
                    let outcome = host.run(ctx, 64, 64, 4);
                    assert!(outcome.result.is_ok());
                    let after = ctx.jit().0.borrow().stats;
                    let (frames, values, open, fuel) = host.test_trace();
                    let slots = values
                        .into_iter()
                        .map(|value| {
                            let slot = crate::jit::abi::Slot::from_value(value);
                            (slot.tag, slot.bits)
                        })
                        .collect::<Vec<_>>();
                    (
                        frames,
                        slots,
                        open,
                        fuel,
                        outcome.activations,
                        after.total_dispatches - before.total_dispatches,
                        after.native_instructions + after.interpreted_instructions
                            - before.native_instructions
                            - before.interpreted_instructions,
                    )
                })
            })
        };
        assert_eq!(run(&mut lua, true), run(&mut lua, false), "fuel={fuel}");
    }
    assert!(lua.enter(|ctx| ctx.jit().test_pair_executions().1 > 0));
}

#[test]
fn interrupted_and_error_fallbacks_do_not_repeat_or_drop_caller_prefix_work() {
    for source in [
        SOURCE,
        &b"local n=7 local function f(v) n=n+v end f({}) return n"[..],
    ] {
        let (mut lua, closure, key) = fixture_source(source);
        lua.enter(|ctx| ctx.jit().observe_pair(key));
        assert_eq!(lua.service_jit().unwrap(), 1);
        for interrupted in [false, true] {
            let run = |lua: &mut Lua, select| {
                lua.enter(|ctx| {
                    let mut fuel = Fuel::with(65536);
                    if interrupted {
                        fuel.interrupt();
                    }
                    with_test_thread(ctx, ctx.fetch(&closure), &mut fuel, |host| {
                        host.test_pair_selection(select);
                        let before = ctx.jit().0.borrow().stats;
                        let outcome = host.run(ctx, 64, 64, 4);
                        let result = format!("{:?}", outcome.result);
                        let after = ctx.jit().0.borrow().stats;
                        let (frames, values, open, remaining) = host.test_trace();
                        let slots = values
                            .into_iter()
                            .map(|value| {
                                let slot = crate::jit::abi::Slot::from_value(value);
                                (slot.tag, slot.bits)
                            })
                            .collect::<Vec<_>>();
                        (
                            result,
                            frames,
                            slots,
                            open,
                            remaining,
                            outcome.activations,
                            after.total_dispatches - before.total_dispatches,
                            after.native_instructions + after.interpreted_instructions
                                - before.native_instructions
                                - before.interpreted_instructions,
                        )
                    })
                })
            };
            assert_eq!(
                run(&mut lua, true),
                run(&mut lua, false),
                "source={source:?} interrupted={interrupted}"
            );
        }
    }
}

#[test]
fn variable_stack_protocol_errors_preserve_unsuccessful_slice_fuel_accounting() {
    let (mut lua, closure, key) = fixture();
    lua.enter(|ctx| ctx.jit().observe_pair(key));
    assert_eq!(lua.service_jit().unwrap(), 1);
    let run = |lua: &mut Lua, select| {
        lua.enter(|ctx| {
            with_test_thread(ctx, ctx.fetch(&closure), &mut Fuel::with(65536), |host| {
                host.test_pair_selection(select);
                host.test_variable_stack();
                let outcome = host.run(ctx, 64, 64, 4);
                assert!(matches!(
                    outcome.result,
                    Err(crate::thread::VMError::ExpectedVariableStack(false))
                ));
                let (frames, _, open, fuel) = host.test_trace();
                (frames, open, fuel, outcome.activations)
            })
        })
    };
    assert_eq!(run(&mut lua, true), run(&mut lua, false));
    assert_eq!(lua.enter(|ctx| ctx.jit().test_pair_executions()), (0, 0));
}

#[test]
fn frozen_upvalue_corpus_executes_selected_pairs_and_keeps_its_expected_result() {
    assert!(!workloads::COLD_SOURCE.is_empty() && !workloads::PREDICATE_SOURCE.is_empty());
    let workload = workloads::WORKLOADS
        .iter()
        .find(|case| case.name == "closure_upvalue")
        .unwrap();
    let (mut lua, closure, key) = fixture_source(workload.source);
    lua.enter(|ctx| {
        ctx.jit().test_activation_host(64);
        ctx.jit().0.borrow_mut().enqueue(key.caller, true);
        ctx.jit().0.borrow_mut().enqueue(key.callee, true);
        ctx.jit().observe_pair(key);
    });
    for _ in 0..3 {
        assert_eq!(lua.service_jit().unwrap(), 1);
    }
    lua.enter(|ctx| {
        let executor = crate::Executor::start(ctx, ctx.fetch(&closure).into(), ());
        let mut fuel = Fuel::with(1_000_000);
        while !executor.step(ctx, &mut fuel).unwrap() {}
        assert_eq!(
            executor.take_result::<i64>(ctx).unwrap().unwrap(),
            workload.expected
        );
        assert!(ctx.jit().test_pair_executions().1 > 0);
    });
}
