use super::*;
use crate::jit::{owner::Shared, pairs::Key, Runtime};

struct Fixture {
    lua: Lua,
    runtime: Runtime,
    source: crate::StashedClosure,
    key: Key,
    start: usize,
}

impl Fixture {
    fn new() -> (Self, Request) {
        let mut lua = Lua::empty();
        lua.set_gc_pacing(false);
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        })
        .unwrap();
        let (runtime, source, key, start, request) = lua.enter(|ctx| {
            let source = Closure::load(
                ctx,
                None,
                b"local n=0 local function f(v) n=n+v end for i=1,20 do f(i) end return n",
            )
            .unwrap();
            let (pair, start) = build_dependencies(ctx, source);
            let key = pair.program.key();
            let caller = ctx
                .jit_registry()
                .borrow()
                .resolve(ctx, key.caller)
                .unwrap();
            let request = Request::prepare(ctx, caller, pair).unwrap();
            (ctx.jit().clone(), ctx.stash(source), key, start, request)
        });
        (
            Self {
                lua,
                runtime,
                source,
                key,
                start,
            },
            request,
        )
    }

    fn request(&mut self) -> Request {
        self.lua.enter(|ctx| {
            Request::prepare(
                ctx,
                ctx.jit_registry()
                    .borrow()
                    .resolve(ctx, self.key.caller)
                    .unwrap(),
                PreparedPair {
                    program: ctx.jit().pair_lease(self.key).unwrap(),
                },
            )
            .unwrap()
        })
    }
}

#[test]
fn request_compiles_outside_arena_and_cached_execution_matches_off() {
    let (mut fixture, request) = Fixture::new();
    fn gc_free<T: 'static>(_: &T) {}
    gc_free(&request);
    let snapshots = fixture.runtime.0.borrow().snapshots.0.clone();
    assert!(snapshots.current() > 0);
    fixture.lua.gc_collect();
    fixture.lua.gc_collect();
    let region = request.compile(&fixture.runtime).unwrap();
    assert_eq!(snapshots.current(), 0);
    fixture.runtime.install_region(region).unwrap();
    fixture.lua.enter(|ctx| {
        let closure = ctx.fetch(&fixture.source);
        let region = ctx.jit().region_lease(fixture.key).unwrap();
        let mut slices = 0;
        let mut run = |native| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, fixture.start as u32, 4).result.unwrap();
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
    });
    assert_eq!(snapshots.current(), 0);
}

#[test]
fn dropping_request_and_snapshot_refusal_leave_dependencies_intact() {
    let (mut fixture, request) = Fixture::new();
    let (snapshots, memory) = {
        let manager = fixture.runtime.0.borrow();
        (manager.snapshots.0.clone(), manager.memory.clone())
    };
    let requested = memory.requested();
    assert!(snapshots.current() > 0);
    drop(request);
    assert_eq!(snapshots.current(), 0);
    assert_eq!(memory.requested(), requested);
    snapshots.set_limit(0);
    fixture.lua.enter(|ctx| {
        assert!(matches!(
            Request::prepare(
                ctx,
                ctx.fetch(&fixture.source).prototype(),
                PreparedPair {
                    program: ctx.jit().pair_lease(fixture.key).unwrap()
                }
            ),
            Err(JitError::ResourceLimit("prototype snapshot"))
        ));
    });
    assert_eq!(snapshots.current(), 0);
    assert_eq!(memory.requested(), requested);
    assert!(fixture.runtime.lookup(fixture.key.caller).is_some());
    assert!(fixture.runtime.pair_lease(fixture.key).is_some());
}

#[test]
fn request_and_built_region_do_not_root_collected_sources() {
    for build in [false, true] {
        let (mut fixture, request) = Fixture::new();
        let snapshots = fixture.runtime.0.borrow().snapshots.0.clone();
        let memory = fixture.runtime.0.borrow().memory.clone();
        let (request, region) = if build {
            (None, Some(request.compile(&fixture.runtime).unwrap()))
        } else {
            (Some(request), None)
        };
        drop(fixture.source);
        fixture.lua.gc_collect();
        fixture.lua.gc_collect();
        fixture.lua.enter(|ctx| {
            for id in [fixture.key.caller, fixture.key.callee] {
                assert!(ctx.jit_registry().borrow().resolve(ctx, id).is_none());
            }
        });
        assert_eq!(fixture.runtime.0.borrow().stats.registered_prototypes, 0);
        assert!(memory.requested() > 0);
        let result = if let Some(request) = request {
            request.compile(&fixture.runtime).map(drop)
        } else {
            fixture.runtime.install_region(region.unwrap())
        };
        assert!(matches!(result, Err(JitError::Compilation(_))));
        assert_eq!(snapshots.current(), 0);
        assert_eq!(memory.requested(), 0);
        assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(fixture.lua.jit_stats().metadata_bytes, 0);
    }
}

#[test]
fn changed_configuration_or_dependencies_refuse_build_and_install() {
    for build in [false, true] {
        for change in 0..6 {
            let (mut fixture, request) = Fixture::new();
            let (peer_key, peer_source, peer) = fixture.lua.enter(|ctx| {
                let source = Closure::load(
                    ctx,
                    None,
                    b"local n=7 local function f(v) n=n+v end for i=1,10 do f(i) end return n",
                )
                .unwrap();
                let (region, _) = build_region(ctx, source);
                let key = region.pair.program.key();
                ctx.jit().install_region(region).unwrap();
                (key, ctx.stash(source), ctx.jit().region_lease(key).unwrap())
            });
            let (request, region) = if build {
                (None, Some(request.compile(&fixture.runtime).unwrap()))
            } else {
                (Some(request), None)
            };
            fixture.lua.enter(|ctx| match change {
                0 => {
                    let mut manager = ctx.jit().0.borrow_mut();
                    let mut config = manager.config.clone();
                    config.hot_threshold += 1;
                    manager.configure(config);
                }
                1 => ctx.jit().0.borrow_mut().retire(fixture.key.caller),
                2 => ctx.jit().0.borrow_mut().retire(fixture.key.callee),
                3 | 4 => {
                    let source = ctx.fetch(&fixture.source);
                    let allocator = ctx.jit().0.borrow().snapshots.clone();
                    let caller = crate::jit::ir::Snapshot::new_in(
                        &source.prototype(),
                        4096,
                        allocator.clone(),
                    )
                    .unwrap();
                    if change == 3 {
                        ctx.jit().compile(fixture.key.caller, caller).unwrap();
                    } else {
                        let callee = crate::jit::ir::Snapshot::new_in(
                            &source.prototype().prototypes[0],
                            4096,
                            allocator,
                        )
                        .unwrap();
                        ctx.jit().compile_pair(fixture.key, caller, callee).unwrap();
                    }
                }
                5 => ctx.jit().0.borrow_mut().clear(),
                _ => unreachable!(),
            });
            let clock = fixture.runtime.0.borrow().clock;
            let result = if let Some(request) = request {
                request.compile(&fixture.runtime).map(drop)
            } else {
                fixture.runtime.install_region(region.unwrap())
            };
            assert!(matches!(result, Err(JitError::Compilation(_))));
            assert_eq!(fixture.runtime.0.borrow().clock, clock);
            assert_eq!(fixture.runtime.0.borrow().snapshots.0.current(), 0);
            if change != 5 {
                assert!(Shared::ptr_eq(
                    &peer,
                    &fixture.runtime.region_lease(peer_key).unwrap()
                ));
            }
            drop(peer);
            drop(peer_source);
        }
    }
}

#[test]
fn foreign_runtime_refuses_prepared_request_before_native_allocation() {
    let (fixture, request) = Fixture::new();
    let (foreign, foreign_request) = Fixture::new();
    assert_eq!(fixture.key, foreign.key);
    let memory = fixture.runtime.0.borrow().memory.clone();
    let foreign_memory = foreign.runtime.0.borrow().memory.clone();
    let before = memory.requested();
    let foreign_before = foreign_memory.requested();
    assert!(matches!(
        request.compile(&foreign.runtime),
        Err(JitError::Compilation(_))
    ));
    assert_eq!(memory.requested(), before);
    assert_eq!(foreign_memory.requested(), foreign_before);
    assert_eq!(fixture.runtime.0.borrow().snapshots.0.current(), 0);
    drop(foreign_request);
}

#[test]
fn failed_compilation_releases_snapshot_and_preserves_cached_region() {
    for fault in 0..5 {
        let (mut fixture, request) = Fixture::new();
        let region = request.compile(&fixture.runtime).unwrap();
        fixture.runtime.install_region(region).unwrap();
        let original = fixture.runtime.region_lease(fixture.key).unwrap();
        if fault == 3 {
            fixture.runtime.0.borrow_mut().config.max_ir_blocks = 1;
        } else if fault == 4 {
            fixture.runtime.0.borrow_mut().config.max_relocations = original.caller.relocations();
        }
        let request = fixture.request();
        let (snapshots, metadata, memory, host) = {
            let manager = fixture.runtime.0.borrow();
            (
                manager.snapshots.0.clone(),
                manager.metadata.0.clone(),
                manager.memory.clone(),
                manager.host.clone(),
            )
        };
        let before = memory.requested();
        let metadata_before = metadata.current();
        match fault {
            0 => snapshots.fail_after(0),
            1 => metadata.fail_after(0),
            2 => host.set_limit(host.accounted()),
            3 | 4 => {}
            _ => unreachable!(),
        }
        let result = request.compile(&fixture.runtime);
        if fault == 4 {
            assert!(matches!(
                result,
                Err(JitError::ResourceLimit("native region relocations"))
            ));
        } else {
            assert!(matches!(result, Err(JitError::ResourceLimit(_))));
        }
        snapshots.fail_after(usize::MAX);
        metadata.fail_after(usize::MAX);
        assert_eq!(snapshots.current(), 0);
        assert_eq!(metadata.current(), metadata_before);
        assert_eq!(memory.requested(), before);
        assert!(Shared::ptr_eq(
            &original,
            &fixture.runtime.region_lease(fixture.key).unwrap()
        ));
        assert!(fixture.runtime.lookup(fixture.key.caller).is_some());
        assert!(fixture.runtime.pair_lease(fixture.key).is_some());
    }
}
