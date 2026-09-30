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
    for lua in [Lua::empty(), Lua::core(), Lua::full()] {
        assert_eq!(lua.jit_config().mode, JitMode::Off);
        assert_eq!(lua.jit_stats().native_entries, 0);
    }
}

#[test]
fn configuration_is_validated_transactionally() {
    let mut lua = Lua::core();
    let before = lua.jit_config();
    let mut invalid = before.clone();
    invalid.mode = JitMode::Auto;
    invalid.max_queue_entries = 0;
    assert!(matches!(
        lua.set_jit_config(invalid),
        Err(JitError::InvalidConfiguration(_))
    ));
    assert_eq!(lua.jit_config(), before);
    let mut invalid = before.clone();
    invalid.max_metadata_bytes = 0;
    assert!(matches!(
        lua.set_jit_config(invalid),
        Err(JitError::InvalidConfiguration(_))
    ));
    assert_eq!(lua.jit_config(), before);
    let mut invalid = before.clone();
    invalid.hot_threshold = 0;
    assert!(lua.set_jit_config(invalid).is_err());
    assert_eq!(lua.jit_config(), before);
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
