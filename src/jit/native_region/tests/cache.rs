use super::*;
use crate::jit::{owner::Shared, CacheKey};

#[test]
fn real_compile_pressure_evicts_cached_region_and_recovers_after_lease_release() {
    for pin in [false, true] {
        fixture(|ctx, _, region, _| {
            let key = region.pair.program.key();
            ctx.jit().install_region(region).unwrap();
            let mut lease = pin.then(|| ctx.jit().region_lease(key).unwrap());
            let source = Closure::load(ctx, None, b"return 42").unwrap();
            let id = ctx
                .jit_registry()
                .borrow()
                .identity(ctx, source.prototype())
                .unwrap();
            let (memory, allocator) = {
                let mut manager = ctx.jit().0.borrow_mut();
                manager.config.max_code_bytes =
                    manager.memory.load(std::sync::atomic::Ordering::Relaxed);
                manager.enqueue(id, true);
                assert_eq!(manager.next_request(), Some(id));
                (manager.memory.clone(), manager.snapshots.clone())
            };
            let requested = memory.requested();
            let compile = || {
                ctx.jit().compile(
                    id,
                    crate::jit::ir::Snapshot::new_in(&source.prototype(), 4096, allocator.clone())
                        .unwrap(),
                )
            };
            if pin {
                assert!(matches!(
                    compile(),
                    Err(JitError::ResourceLimit("native mappings"))
                ));
                assert_eq!(memory.requested(), requested);
                assert_eq!(ctx.jit().0.borrow().stats.cache_evictions, 0);
                assert_eq!(ctx.jit().0.borrow().stats.cache_eviction_refusals, 1);
                drop(lease.take());
            }
            compile().unwrap();
            assert!(ctx.jit().region_lease(key).is_none());
            assert!(ctx.jit().lookup(id).is_some());
            assert!(ctx.jit().lookup(key.caller).is_some());
            assert!(ctx.jit().pair_lease(key).is_some());
            assert_eq!(ctx.jit().0.borrow().stats.cache_evictions, 1);
            assert_eq!(ctx.jit().0.borrow().tracked[&id].attempts, 2);
            assert!(memory.requested() <= requested);
            assert!(
                memory.load(std::sync::atomic::Ordering::Relaxed)
                    <= ctx.jit().0.borrow().config.max_code_bytes
            );
            ctx.jit().0.borrow_mut().clear_registrations();
            assert_eq!(memory.requested(), 0);
        });
    }
}

#[test]
fn foreign_and_stale_cache_admission_preserves_existing_regions() {
    fixture(|ctx, closure, region, _| {
        let key = region.pair.program.key();
        ctx.jit().install_region(region).unwrap();
        let original = ctx.jit().region_lease(key).unwrap();
        let metadata = ctx.jit().0.borrow().metadata.0.clone();
        let replacement = Region::new(
            ctx,
            closure,
            PreparedPair {
                program: ctx.jit().pair_lease(key).unwrap(),
            },
        )
        .unwrap();
        metadata.fail_after(0);
        assert!(matches!(
            ctx.jit().install_region(replacement),
            Err(JitError::ResourceLimit("region owner metadata"))
        ));
        metadata.fail_after(usize::MAX);
        assert!(Shared::ptr_eq(
            &original,
            &ctx.jit().region_lease(key).unwrap()
        ));
        let mut foreign = Lua::empty();
        foreign
            .set_jit_config(JitConfig {
                mode: JitMode::Auto,
                ..Default::default()
            })
            .unwrap();
        let foreign_region = foreign.enter(|foreign| {
            let source = Closure::load(
                foreign,
                None,
                b"local n=0 local function f(v) n=n+v end for i=1,20 do f(i) end return n",
            )
            .unwrap();
            build_region(foreign, source).0
        });
        assert_eq!(foreign_region.pair.program.key(), key);
        assert!(matches!(
            ctx.jit().install_region(foreign_region),
            Err(JitError::Compilation(_))
        ));
        assert!(Shared::ptr_eq(
            &original,
            &ctx.jit().region_lease(key).unwrap()
        ));
        ctx.jit().0.borrow_mut().code.remove(&key.caller);
        let before = ctx.jit().0.borrow().clock;
        assert!(ctx.jit().region_lease(key).is_none());
        assert_eq!(ctx.jit().0.borrow().clock, before);
        assert_eq!(Shared::strong_count(&original), 1);
    });
}

#[test]
fn cached_region_executes_and_pressure_releases_only_unleased_dependencies() {
    fixture(|ctx, closure, region, start| {
        let key = region.pair.program.key();
        let memory = ctx.jit().0.borrow().memory.clone();
        let requested = memory.requested();
        ctx.jit().install_region(region).unwrap();
        let clock = ctx.jit().0.borrow().clock;
        assert!(ctx
            .jit()
            .region_lease(crate::jit::pairs::Key {
                pc: key.pc + 1,
                ..key
            })
            .is_none());
        assert_eq!(ctx.jit().0.borrow().clock, clock);
        let region = ctx.jit().region_lease(key).unwrap();
        assert_eq!(Shared::strong_count(&region), 2);
        assert!(!ctx
            .jit()
            .0
            .borrow_mut()
            .evict_cached(CacheKey::Prototype(u64::MAX)));
        assert_eq!(memory.requested(), requested);
        let mut slices = 0;
        let mut run = |native| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                let before = stats(ctx);
                if native {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let result = region.run(ctx, host, 64, 64).unwrap();
                    result.result.unwrap();
                    assert_eq!(result.pairs, 20);
                    slices = result.slices;
                } else {
                    host.run(ctx, slices, 64, 4).result.unwrap();
                }
                trace(ctx, host, before)
            })
        };
        let native = run(true);
        assert_eq!(native, run(false));
        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
        drop(region);
        assert!(ctx
            .jit()
            .0
            .borrow_mut()
            .evict_cached(CacheKey::Prototype(u64::MAX)));
        assert!(memory.requested() < requested);
        assert!(ctx.jit().region_lease(key).is_none());
        assert!(ctx.jit().lookup(key.caller).is_some());
        assert!(ctx.jit().pair_lease(key).is_some());
        for _ in 0..2 {
            assert!(ctx
                .jit()
                .0
                .borrow_mut()
                .evict_cached(CacheKey::Prototype(u64::MAX)));
        }
        assert_eq!(memory.requested(), 0);
        assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
    });
}

#[test]
fn retirement_and_configuration_clear_cache_but_preserve_live_region_leases() {
    for action in 0..4 {
        fixture(|ctx, closure, region, start| {
            let key = region.pair.program.key();
            let memory = ctx.jit().0.borrow().memory.clone();
            let requested = memory.requested();
            ctx.jit().install_region(region).unwrap();
            let lease = ctx.jit().region_lease(key).unwrap();
            {
                let mut manager = ctx.jit().0.borrow_mut();
                match action {
                    0 => manager.retire(key.caller),
                    1 => manager.retire(key.callee),
                    2 => {
                        let mut config = manager.config.clone();
                        config.mode = JitMode::Off;
                        manager.configure(config);
                    }
                    3 => manager.clear_registrations(),
                    _ => unreachable!(),
                }
                manager.clear();
            }
            assert_eq!(Shared::strong_count(&lease), 1);
            assert_eq!(memory.requested(), requested);
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let before_stats = stats(ctx);
                let before = trace(ctx, host, before_stats);
                assert!(ctx.jit().region_lease(key).is_none());
                assert!(lease.run(ctx, host, 64, 64).is_none());
                assert_eq!(trace(ctx, host, before_stats), before);
            });
            drop(lease);
            assert_eq!(memory.requested(), 0);
            assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
        });
    }
}

#[test]
fn source_collection_retires_cached_regions_without_rooting_lua_objects() {
    for retain in [false, true] {
        let mut lua = Lua::empty();
        lua.set_gc_pacing(false);
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        })
        .unwrap();
        let (key, memory, lease) = lua.enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                b"local n=0 local function f(v) n=n+v end for i=1,20 do f(i) end return n",
            )
            .unwrap();
            let (region, _) = build_region(ctx, closure);
            let key = region.pair.program.key();
            ctx.jit().install_region(region).unwrap();
            let memory = ctx.jit().0.borrow().memory.clone();
            (
                key,
                memory,
                retain.then(|| ctx.jit().region_lease(key).unwrap()),
            )
        });
        let requested = memory.requested();
        assert!(requested > 0);
        lua.gc_collect();
        lua.gc_collect();
        lua.enter(|ctx| {
            assert!(ctx
                .jit_registry()
                .borrow()
                .resolve(ctx, key.caller)
                .is_none());
            assert!(ctx.jit().region_lease(key).is_none());
            assert_eq!(ctx.jit().0.borrow().stats.registered_prototypes, 0);
        });
        assert_eq!(memory.requested(), if retain { requested } else { 0 });
        drop(lease);
        assert_eq!(memory.requested(), 0);
        assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(lua.jit_stats().metadata_bytes, 0);
    }
}

#[test]
fn successful_dependency_replacement_invalidates_cached_regions() {
    for pair in [false, true] {
        fixture(|ctx, closure, region, start| {
            let key = region.pair.program.key();
            ctx.jit().install_region(region).unwrap();
            let lease = ctx.jit().region_lease(key).unwrap();
            let allocator = ctx.jit().0.borrow().snapshots.clone();
            let caller =
                crate::jit::ir::Snapshot::new_in(&closure.prototype(), 4096, allocator.clone())
                    .unwrap();
            if pair {
                let callee = crate::jit::ir::Snapshot::new_in(
                    &closure.prototype().prototypes[0],
                    4096,
                    allocator,
                )
                .unwrap();
                ctx.jit().compile_pair(key, caller, callee).unwrap();
            } else {
                ctx.jit().compile(key.caller, caller).unwrap();
            }
            assert_eq!(Shared::strong_count(&lease), 1);
            assert!(ctx.jit().region_lease(key).is_none());
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let before_stats = stats(ctx);
                let before = trace(ctx, host, before_stats);
                assert!(lease.run(ctx, host, 64, 64).is_none());
                assert_eq!(trace(ctx, host, before_stats), before);
            });
        });
    }
}

#[test]
fn region_cache_allocation_failures_preserve_installed_dependencies() {
    for (allowance, label) in [
        (0, "region owner metadata"),
        (1, "region cache metadata"),
        (2, "region caller index metadata"),
    ] {
        fixture(|ctx, _, region, _| {
            let key = region.pair.program.key();
            let metadata = ctx.jit().0.borrow().metadata.0.clone();
            let clock = ctx.jit().0.borrow().clock;
            metadata.fail_after(allowance);
            let error = ctx.jit().install_region(region).unwrap_err();
            assert!(matches!(error, JitError::ResourceLimit(found) if found == label));
            metadata.fail_after(usize::MAX);
            assert_eq!(ctx.jit().0.borrow().clock, clock);
            assert!(ctx.jit().region_lease(key).is_none());
            assert!(ctx.jit().lookup(key.caller).is_some());
            assert!(ctx.jit().pair_lease(key).is_some());
            let memory = ctx.jit().0.borrow().memory.clone();
            ctx.jit().0.borrow_mut().clear_registrations();
            assert_eq!(memory.requested(), 0);
        });
    }
}

#[test]
fn cache_owner_and_table_require_exact_metadata_headroom() {
    let mut required = 0;
    fixture(|ctx, _, region, _| {
        let metadata = ctx.jit().0.borrow().metadata.0.clone();
        let before = metadata.current();
        ctx.jit().install_region(region).unwrap();
        required = metadata.current() - before;
        assert!(required > Shared::<Region>::allocation_bytes());
    });
    for headroom in [required - 1, required] {
        let mut ledger = None;
        fixture(|ctx, _, region, _| {
            let key = region.pair.program.key();
            let metadata = ctx.jit().0.borrow().metadata.0.clone();
            let before = metadata.current();
            let limit = before + headroom;
            metadata.set_limit(limit);
            let result = ctx.jit().install_region(region);
            if headroom == required {
                result.unwrap();
                assert_eq!(metadata.current(), limit);
                let lease = ctx.jit().region_lease(key).unwrap();
                assert_eq!(metadata.current(), limit);
                drop(lease);
            } else {
                assert!(matches!(
                    result,
                    Err(JitError::ResourceLimit("region caller index metadata"))
                ));
                assert!(ctx.jit().region_lease(key).is_none());
            }
            assert!(metadata.current() <= limit);
            ledger = Some(metadata);
        });
        assert_eq!(ledger.unwrap().current(), 0);
    }
}

#[test]
fn region_cache_recency_saturation_and_exclusion_select_deterministic_victims() {
    for saturated in [false, true] {
        fixture(|ctx, _, first, _| {
            let first_key = first.pair.program.key();
            let second_closure = Closure::load(
                ctx,
                None,
                b"local n=0 local function f(v) n=n+v end for i=1,20 do f(i) end return n",
            )
            .unwrap();
            let (second, _) = build_region(ctx, second_closure);
            let second_key = second.pair.program.key();
            if saturated {
                ctx.jit().0.borrow_mut().clock = u64::MAX;
            }
            ctx.jit().install_region(first).unwrap();
            ctx.jit().install_region(second).unwrap();
            drop(ctx.jit().region_lease(first_key).unwrap());
            let expected = if saturated { first_key } else { second_key };
            assert_eq!(
                ctx.jit()
                    .0
                    .borrow()
                    .pairs
                    .as_ref()
                    .unwrap()
                    .regions
                    .victim(None)
                    .unwrap()
                    .1,
                expected
            );
            let other = if saturated { second_key } else { first_key };
            assert_eq!(
                ctx.jit()
                    .0
                    .borrow()
                    .pairs
                    .as_ref()
                    .unwrap()
                    .regions
                    .victim(Some(expected))
                    .unwrap()
                    .1,
                other
            );
            assert!(ctx
                .jit()
                .0
                .borrow_mut()
                .evict_cached(CacheKey::Region(other)));
            assert!(ctx.jit().region_lease(expected).is_none());
            assert!(ctx.jit().region_lease(other).is_some());
        });
    }
}
