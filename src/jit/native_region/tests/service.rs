use super::*;
use crate::jit::{pairs::Key, Runtime};

const SOURCE: &[u8] = b"local n=0 local function f(v) n=n+v end for i=1,20 do f(i) end return n";

struct Setup {
    lua: Lua,
    runtime: Runtime,
    source: crate::StashedClosure,
    key: Key,
}

impl Setup {
    fn new(attempts: u32) -> Self {
        let mut lua = Lua::empty();
        lua.set_gc_pacing(false);
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            max_compile_attempts: attempts,
            ..Default::default()
        })
        .unwrap();
        let (source, key, runtime) = lua.enter(|ctx| {
            let source = Closure::load(ctx, None, SOURCE).unwrap();
            let (pair, _) = build_dependencies(ctx, source);
            let key = pair.program.key();
            ctx.jit().0.borrow_mut().config.hot_threshold = 1;
            ctx.jit().test_region_promotions(true);
            (ctx.stash(source), key, ctx.jit().clone())
        });
        Self {
            lua,
            runtime,
            source,
            key,
        }
    }

    fn peer(&mut self) -> (crate::StashedClosure, Key) {
        self.lua.enter(|ctx| {
            let source = Closure::load(ctx, None, SOURCE).unwrap();
            let threshold = ctx.jit().0.borrow().config.hot_threshold;
            ctx.jit().0.borrow_mut().config.hot_threshold = 64;
            let (pair, _) = build_dependencies(ctx, source);
            ctx.jit().0.borrow_mut().config.hot_threshold = threshold;
            (ctx.stash(source), pair.program.key())
        })
    }
}

#[test]
fn real_executor_steps_queue_promotions_and_service_installs_outside_steps() {
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 3,
        ..Default::default()
    })
    .unwrap();
    let (source, key, start, runtime) = lua.enter(|ctx| {
        let source = Closure::load(ctx, None, SOURCE).unwrap();
        let prototype = source.prototype();
        let registry = ctx.jit_registry().borrow();
        let key = Key {
            caller: registry.identity(ctx, prototype).unwrap(),
            callee: registry.identity(ctx, prototype.prototypes[0]).unwrap(),
            pc: prototype
                .opcodes
                .iter()
                .position(|op| matches!(op.decode(), Operation::Call { .. }))
                .unwrap(),
        };
        let start = prototype
            .opcodes
            .iter()
            .position(|op| matches!(op.decode(), Operation::Closure { .. }))
            .unwrap()
            + 1;
        ctx.jit().test_region_promotions(true);
        (ctx.stash(source), key, start, ctx.jit().clone())
    });
    assert_eq!(lua.prepare_jit().unwrap(), 2);
    for pass in 0..2 {
        let before = lua.jit_stats();
        lua.enter(|ctx| {
            let executor = crate::Executor::start(ctx, ctx.fetch(&source).into(), ());
            assert!(executor.step(ctx, &mut Fuel::with(10000)).unwrap());
        });
        let after = lua.jit_stats();
        assert_eq!(after.installed_regions, before.installed_regions);
        assert_eq!(after.code_bytes, before.code_bytes);
        assert_eq!(after.snapshot_bytes, 0);
        assert_eq!(after.queued_requests, 1);
        if pass == 1 {
            assert_eq!(after.native_pair_calls - before.native_pair_calls, 20);
        }
        assert_eq!(lua.service_jit().unwrap(), 1);
        assert_eq!(
            lua.jit_stats().installed_regions,
            before.installed_regions + 1
        );
        assert_eq!(lua.jit_stats().queued_requests, 0);
    }
    let region = runtime.region_lease(key).unwrap();
    lua.enter(|ctx| {
        let source = ctx.fetch(&source);
        let mut slices = 0;
        let mut run = |native| {
            with_test_thread(ctx, source, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                let before = stats(ctx);
                if native {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let outcome = region.run(ctx, host, 64, 64).unwrap();
                    outcome.result.unwrap();
                    assert_eq!(outcome.pairs, 20);
                    slices = outcome.slices;
                } else {
                    host.run(ctx, slices, 64, 4).result.unwrap();
                }
                trace(ctx, host, before)
            })
        };
        let native = run(true);
        assert_eq!(native, run(false));
        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
    });
    for _ in 0..100 {
        runtime.observe_region(key);
    }
    assert_eq!(lua.service_jit().unwrap(), 0);
}

#[test]
fn shared_queue_capacity_hotness_and_shrinking_preserve_other_work() {
    let mut setup = Setup::new(2);
    let (_peer, other) = setup.peer();
    let normal = setup.lua.enter(|ctx| {
        let source = Closure::load(ctx, None, b"return 42").unwrap();
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, source.prototype())
            .unwrap();
        let mut manager = ctx.jit().0.borrow_mut();
        manager.config.hot_threshold = 2;
        manager.config.max_queue_entries = 3;
        manager.enqueue(id, true);
        ctx.stash(source)
    });
    let before = setup.lua.jit_stats().compilation_requests;
    setup.runtime.observe_region(setup.key);
    assert_eq!(setup.lua.jit_stats().queued_requests, 1);
    for _ in 0..3 {
        setup.runtime.observe_region(setup.key);
        setup.runtime.observe_region(other);
    }
    assert_eq!(setup.lua.jit_stats().queued_requests, 3);
    assert_eq!(setup.lua.jit_stats().compilation_requests, before + 2);
    let mut config = setup.lua.jit_config();
    config.max_queue_entries = 1;
    setup.lua.set_jit_config(config).unwrap();
    assert_eq!(setup.lua.jit_stats().queued_requests, 1);
    assert!(setup.runtime.next_region_request().is_none());
    assert_eq!(setup.lua.service_jit().unwrap(), 1);
    setup.runtime.observe_region(other);
    setup.runtime.observe_region(setup.key);
    assert_eq!(setup.lua.jit_stats().queued_requests, 1);
    assert_eq!(setup.runtime.next_region_request(), Some(other));
    setup.runtime.observe_region(setup.key);
    assert_eq!(setup.runtime.next_region_request(), Some(setup.key));
    assert_eq!(setup.lua.jit_stats().queued_requests, 0);
    drop(normal);
}

#[test]
fn snapshot_metadata_and_host_failures_consume_bounded_attempts() {
    for fault in 0..3 {
        let mut setup = Setup::new(2);
        let memory = setup.runtime.0.borrow().memory.clone();
        let snapshots = setup.runtime.0.borrow().snapshots.0.clone();
        let metadata = setup.runtime.0.borrow().metadata.0.clone();
        let requested = memory.requested();
        let initial = setup.lua.jit_stats();
        for attempt in 1..=2 {
            setup.runtime.observe_region(setup.key);
            assert_eq!(setup.lua.jit_stats().queued_requests, 1);
            match fault {
                0 => snapshots.fail_after(0),
                1 => metadata.fail_after(0),
                2 => setup
                    .lua
                    .set_memory_limit(Some(setup.lua.accounted_memory())),
                _ => unreachable!(),
            }
            let result = setup.lua.service_jit();
            if fault == 2 {
                assert!(matches!(
                    result,
                    Err(JitError::ResourceLimit("host memory"))
                ));
            } else {
                assert!(matches!(result, Err(JitError::ResourceLimit(_))));
            }
            snapshots.fail_after(usize::MAX);
            metadata.fail_after(usize::MAX);
            setup.lua.set_memory_limit(None);
            assert_eq!(setup.lua.jit_stats().queued_requests, 0);
            assert_eq!(
                setup.lua.jit_stats().compilation_failures,
                initial.compilation_failures + attempt
            );
            assert_eq!(
                setup.lua.jit_stats().installed_regions,
                initial.installed_regions
            );
            assert_eq!(snapshots.current(), 0);
            assert_eq!(memory.requested(), requested);
            assert!(setup.runtime.lookup(setup.key.caller).is_some());
            assert!(setup.runtime.pair_lease(setup.key).is_some());
        }
        setup.runtime.observe_region(setup.key);
        assert_eq!(setup.lua.service_jit().unwrap(), 0);
    }
}

#[test]
fn ordinary_pair_and_region_requests_share_capacity_and_service_priority() {
    let mut setup = Setup::new(2);
    setup.runtime.0.borrow_mut().config.max_queue_entries = 3;
    setup.runtime.observe_region(setup.key);
    let (pair_source, pair_key, normal_source, normal_id) = setup.lua.enter(|ctx| {
        let source = Closure::load(ctx, None, SOURCE).unwrap();
        let prototype = source.prototype();
        let key = {
            let registry = ctx.jit_registry().borrow();
            Key {
                caller: registry.identity(ctx, prototype).unwrap(),
                callee: registry.identity(ctx, prototype.prototypes[0]).unwrap(),
                pc: prototype
                    .opcodes
                    .iter()
                    .position(|op| matches!(op.decode(), Operation::Call { .. }))
                    .unwrap(),
            }
        };
        ctx.jit().observe_pair(key);
        let normal = Closure::load(ctx, None, b"return 42").unwrap();
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, normal.prototype())
            .unwrap();
        ctx.jit().0.borrow_mut().enqueue(id, true);
        (ctx.stash(source), key, ctx.stash(normal), id)
    });
    assert_eq!(setup.lua.jit_stats().queued_requests, 3);
    assert_eq!(setup.lua.service_jit().unwrap(), 1);
    assert!(setup.runtime.lookup(normal_id).is_some());
    assert!(setup.runtime.pair_lease(pair_key).is_none());
    assert!(setup.runtime.region_lease(setup.key).is_none());
    assert_eq!(setup.lua.jit_stats().queued_requests, 2);
    assert_eq!(setup.lua.service_jit().unwrap(), 1);
    assert!(setup.runtime.pair_lease(pair_key).is_some());
    assert!(setup.runtime.region_lease(setup.key).is_none());
    assert_eq!(setup.lua.jit_stats().queued_requests, 1);
    assert_eq!(setup.lua.service_jit().unwrap(), 1);
    assert!(setup.runtime.region_lease(setup.key).is_some());
    assert_eq!(setup.lua.jit_stats().queued_requests, 0);
    assert_eq!(setup.lua.service_jit().unwrap(), 0);
    drop((pair_source, normal_source));
}

#[test]
fn ordinary_eviction_cancels_pending_promotion_and_replacement_can_requeue() {
    let mut setup = Setup::new(2);
    setup.runtime.observe_region(setup.key);
    assert_eq!(setup.lua.jit_stats().queued_requests, 1);
    assert!(setup
        .runtime
        .0
        .borrow_mut()
        .evict_cached(crate::jit::CacheKey::Prototype(u64::MAX)));
    assert!(setup.runtime.lookup(setup.key.caller).is_none());
    assert_eq!(setup.lua.jit_stats().queued_requests, 0);
    setup.runtime.observe_region(setup.key);
    assert_eq!(setup.lua.service_jit().unwrap(), 0);
    setup.lua.enter(|ctx| {
        let source = ctx.fetch(&setup.source);
        let allocator = ctx.jit().0.borrow().snapshots.clone();
        let snapshot =
            crate::jit::ir::Snapshot::new_in(&source.prototype(), 4096, allocator).unwrap();
        ctx.jit().compile(setup.key.caller, snapshot).unwrap();
    });
    setup.runtime.observe_region(setup.key);
    assert_eq!(setup.lua.service_jit().unwrap(), 1);
    assert!(setup.runtime.region_lease(setup.key).is_some());
}

#[test]
fn real_mapping_retry_pins_dependencies_and_respects_attempts_and_peer_leases() {
    for mode in 0..3 {
        let mut setup = Setup::new(if mode == 2 { 1 } else { 3 });
        let (peer, other) = setup.peer();
        let request = setup
            .lua
            .enter(|ctx| ctx.jit().prepare_region(ctx, other).unwrap().unwrap());
        setup
            .runtime
            .install_region(request.compile(&setup.runtime).unwrap())
            .unwrap();
        let mut lease = (mode == 1).then(|| setup.runtime.region_lease(other).unwrap());
        let before = setup.lua.jit_stats();
        setup.runtime.0.borrow_mut().config.max_code_bytes = before.code_bytes;
        setup.runtime.observe_region(setup.key);
        let result = setup.lua.service_jit();
        if mode != 0 {
            assert!(matches!(
                result,
                Err(JitError::ResourceLimit("native mappings"))
            ));
            assert_eq!(setup.lua.jit_stats().code_bytes, before.code_bytes);
            assert_eq!(
                setup.lua.jit_stats().cache_evictions,
                before.cache_evictions
            );
            assert!(setup.runtime.region_lease(other).is_some());
            assert!(setup.runtime.lookup(setup.key.caller).is_some());
            assert!(setup.runtime.pair_lease(setup.key).is_some());
            drop(lease.take());
            setup.runtime.observe_region(setup.key);
            if mode == 2 {
                assert_eq!(setup.lua.service_jit().unwrap(), 0);
                continue;
            }
            assert_eq!(setup.lua.service_jit().unwrap(), 1);
        } else {
            assert_eq!(result.unwrap(), 1);
        }
        assert!(setup.runtime.region_lease(other).is_none());
        assert!(setup.runtime.region_lease(setup.key).is_some());
        assert_eq!(
            setup.lua.jit_stats().cache_evictions,
            before.cache_evictions + 1
        );
        assert!(setup.lua.jit_stats().code_bytes <= before.code_bytes);
        assert_eq!(setup.lua.jit_stats().snapshot_bytes, 0);
        assert_eq!(setup.lua.jit_stats().queued_requests, 0);
        drop(peer);
    }
}

#[test]
fn retired_sources_and_replaced_dependencies_cancel_promotion_requests() {
    for change in 0..7 {
        let mut setup = Setup::new(1);
        setup.runtime.observe_region(setup.key);
        assert_eq!(setup.lua.jit_stats().queued_requests, 1);
        match change {
            0 => {
                drop(setup.source);
                setup.lua.gc_collect();
                setup.lua.gc_collect();
            }
            1 => setup.lua.clear_jit_cache(),
            2 => {
                let mut config = setup.lua.jit_config();
                config.mode = JitMode::Off;
                setup.lua.set_jit_config(config).unwrap();
            }
            3..=6 => setup.lua.enter(|ctx| {
                if change >= 5 {
                    assert_eq!(ctx.jit().next_region_request(), Some(setup.key));
                }
                let source = ctx.fetch(&setup.source);
                let allocator = ctx.jit().0.borrow().snapshots.clone();
                let caller =
                    crate::jit::ir::Snapshot::new_in(&source.prototype(), 4096, allocator.clone())
                        .unwrap();
                if change % 2 == 1 {
                    ctx.jit().compile(setup.key.caller, caller).unwrap();
                } else {
                    let callee = crate::jit::ir::Snapshot::new_in(
                        &source.prototype().prototypes[0],
                        4096,
                        allocator,
                    )
                    .unwrap();
                    ctx.jit().compile_pair(setup.key, caller, callee).unwrap();
                }
                if change >= 5 {
                    ctx.jit().observe_region(setup.key);
                }
            }),
            _ => unreachable!(),
        }
        assert_eq!(setup.lua.jit_stats().queued_requests, 0);
        assert_eq!(setup.lua.service_jit().unwrap(), 0);
        assert_eq!(setup.lua.jit_stats().snapshot_bytes, 0);
        if change == 0 {
            assert_eq!(setup.lua.jit_stats().registered_prototypes, 0);
            assert_eq!(setup.lua.jit_stats().code_bytes, 0);
            assert_eq!(setup.lua.jit_stats().metadata_bytes, 0);
        }
    }
}

#[test]
fn promotion_metadata_refusal_is_optional_and_recovers_without_compiling_in_steps() {
    for allowance in [0, 1] {
        let mut setup = Setup::new(2);
        let metadata = setup.runtime.0.borrow().metadata.0.clone();
        metadata.fail_after(allowance);
        let before = setup.lua.jit_stats();
        setup.runtime.observe_region(setup.key);
        assert_eq!(setup.lua.jit_stats().queued_requests, 0);
        assert_eq!(
            setup.lua.jit_stats().compilation_requests,
            before.compilation_requests
        );
        assert_eq!(
            setup.lua.jit_stats().installed_regions,
            before.installed_regions
        );
        assert_eq!(setup.lua.jit_stats().code_bytes, before.code_bytes);
        metadata.fail_after(usize::MAX);
        setup.runtime.observe_region(setup.key);
        assert_eq!(setup.lua.service_jit().unwrap(), 1);
        assert!(setup.runtime.region_lease(setup.key).is_some());
    }
}
