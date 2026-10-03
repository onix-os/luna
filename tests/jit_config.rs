#![cfg(feature = "jit")]

use luna::{Closure, Executor, JitConfig, JitError, JitMode, Lua};

#[test]
fn completed_gc_cycles_retire_dead_sources_while_jit_is_off() {
    for collection in ["full", "forced", "debt"] {
        let mut lua = Lua::empty();
        lua.set_gc_pacing(false);
        let live = lua
            .try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, b"return 42")?)))
            .unwrap();
        let dead = lua
            .try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, b"return 99")?)))
            .unwrap();
        assert_eq!(lua.jit_stats().registered_prototypes, 2);
        drop(dead);
        let collect = |lua: &mut Lua| {
            for _ in 0..128 {
                match collection {
                    "full" => lua.gc_collect(),
                    "forced" => lua.gc_step(None),
                    "debt" => {
                        lua.enter(|ctx| {
                            for _ in 0..128 {
                                luna::Table::new(&ctx);
                            }
                        });
                        lua.gc_step(Some(0.0));
                    }
                    _ => unreachable!(),
                }
                if lua.jit_stats().registered_prototypes <= 1 {
                    break;
                }
            }
        };
        collect(&mut lua);
        assert_eq!(lua.jit_stats().registered_prototypes, 1, "{collection}");
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&live).into(), ())));
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 42);
        drop(executor);
        drop(live);
        for _ in 0..128 {
            match collection {
                "full" => lua.gc_collect(),
                "forced" => lua.gc_step(None),
                "debt" => {
                    lua.enter(|ctx| {
                        for _ in 0..128 {
                            luna::Table::new(&ctx);
                        }
                    });
                    lua.gc_step(Some(0.0));
                }
                _ => unreachable!(),
            }
            if lua.jit_stats().registered_prototypes == 0 {
                break;
            }
        }
        let stats = lua.jit_stats();
        assert_eq!(stats.registered_prototypes, 0, "{collection}");
        assert_eq!(stats.metadata_bytes, 0, "{collection}");
        assert_eq!(stats.snapshot_bytes, 0);
        assert_eq!(stats.code_bytes, 0);
        assert_eq!(stats.compilation_requests, 0);
        assert_eq!(stats.native_instructions, 0);
        assert_eq!(lua.jit_config().mode, JitMode::Off);
    }
}

#[test]
fn constructors_default_to_interpretation() {
    for lua in [Lua::empty(), Lua::core(), Lua::full(), Lua::default()] {
        assert_eq!(lua.jit_config().mode, JitMode::Off);
        assert_eq!(lua.jit_stats().native_entries, 0);
    }
}

fn invalid_configurations(
    base: &JitConfig,
) -> impl Iterator<Item = (&'static str, JitConfig)> + '_ {
    type Invalidate = fn(&mut JitConfig);
    let fields: [(&str, Invalidate); 10] = [
        ("hot_threshold", |c| c.hot_threshold = 0),
        ("max_prototype_instructions", |c| {
            c.max_prototype_instructions = 0
        }),
        ("max_ir_instructions", |c| c.max_ir_instructions = 0),
        ("max_ir_blocks", |c| c.max_ir_blocks = 0),
        ("max_relocations", |c| c.max_relocations = 0),
        ("max_queue_entries", |c| c.max_queue_entries = 0),
        ("max_code_bytes", |c| c.max_code_bytes = 0),
        ("max_snapshot_bytes", |c| c.max_snapshot_bytes = 0),
        ("max_metadata_bytes", |c| c.max_metadata_bytes = 0),
        ("max_compile_attempts", |c| c.max_compile_attempts = 0),
    ];
    fields.into_iter().map(|(name, invalidate)| {
        let mut invalid = base.clone();
        invalid.mode = match base.mode {
            JitMode::Off => JitMode::Auto,
            JitMode::Auto => JitMode::Off,
        };
        invalidate(&mut invalid);
        (name, invalid)
    })
}

#[test]
fn configuration_is_validated_transactionally() {
    let mut lua = Lua::core();
    let before = lua.jit_config();
    let stats = lua.jit_stats();
    for (name, invalid) in invalid_configurations(&before) {
        assert!(
            matches!(
                lua.set_jit_config(invalid),
                Err(JitError::InvalidConfiguration(_))
            ),
            "{name}"
        );
        assert_eq!(lua.jit_config(), before, "{name}");
        assert_eq!(lua.jit_stats(), stats, "{name}");
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn invalid_configuration_preserves_native_peer_and_queued_work() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    })
    .unwrap();
    let peer = lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        ctx.stash(closure)
    });
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let pending = lua.enter(|ctx| {
        let closure =
            Closure::load(ctx, None, b"local s=0 for i=1,100 do s=s+i end return s").unwrap();
        let executor = Executor::start(ctx, closure.into(), ());
        assert!(!executor.step(ctx, &mut luna::Fuel::empty()).unwrap());
        ctx.stash(executor)
    });
    let config = lua.jit_config();
    let stats = lua.jit_stats();
    assert_eq!(stats.installed_regions, 1);
    assert_eq!(stats.queued_requests, 1);
    assert!(stats.code_bytes > 0);
    for (name, invalid) in invalid_configurations(&config) {
        assert!(
            matches!(
                lua.set_jit_config(invalid),
                Err(JitError::InvalidConfiguration(_))
            ),
            "{name}"
        );
        assert_eq!(lua.jit_config(), config, "{name}");
        assert_eq!(lua.jit_stats(), stats, "{name}");
    }
    assert_eq!(lua.service_jit().unwrap(), 1);
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&peer).into(), ())));
    let before = lua.jit_stats();
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 42);
    assert!(lua.jit_stats().native_instructions > before.native_instructions);
    let before = lua.jit_stats();
    assert_eq!(lua.execute::<i64>(&pending).unwrap(), 5050);
    assert!(lua.jit_stats().native_instructions > before.native_instructions);
    assert_eq!(
        lua.jit_stats().compilation_failures,
        stats.compilation_failures
    );
}

#[test]
fn lua_remains_non_send_and_non_sync() {
    trait AmbiguousSend<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousSend<u8> for T {}
    trait AmbiguousSync<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousSync<u8> for T {}
    let _ = <Lua as AmbiguousSend<_>>::check;
    let _ = <Lua as AmbiguousSync<_>>::check;
}

#[test]
fn states_have_independent_configuration_and_counters() {
    let mut first = Lua::core();
    let second = Lua::core();
    first
        .set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..JitConfig::default()
        })
        .unwrap();
    assert_eq!(second.jit_config().mode, JitMode::Off);
    let executor = first
        .try_enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return 42")?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    assert_eq!(first.execute::<i64>(&executor).unwrap(), 42);
    assert!(first.jit_stats().interpreted_slices > 0);
    assert_eq!(second.jit_stats().interpreted_slices, 0);
    first.set_jit_config(JitConfig::default()).unwrap();
    assert_eq!(first.jit_config().mode, JitMode::Off);
}

#[test]
fn capability_reports_the_build_target() {
    let lua = Lua::empty();
    let capabilities = lua.jit_capabilities();
    assert_eq!(capabilities.architecture, std::env::consts::ARCH);
    assert_eq!(
        capabilities.supported_target,
        cfg!(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))
    );
    assert_eq!(
        capabilities.unavailable_reason.is_some(),
        !capabilities.supported_target
    );
}
