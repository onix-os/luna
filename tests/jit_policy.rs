#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{Closure, Executor, ExternError, Fuel, JitConfig, JitError, JitMode, Lua};

#[test]
fn cache_pressure_preserves_live_closures_native_results_and_interpreter_fallback(
) -> Result<(), ExternError> {
    let load = |lua: &mut Lua| {
        lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, Some("cache-pressure"), b"return 42")?;
            Ok(ctx.stash(closure))
        })
    };
    let mut probe = state();
    let probe_closure = load(&mut probe)?;
    assert_eq!(probe.prepare_jit().unwrap(), 1);
    let module_bytes = probe.jit_stats().code_bytes;
    assert!(module_bytes > 0);
    drop((probe_closure, probe));

    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        max_code_bytes: 2 * module_bytes,
        ..JitConfig::default()
    })
    .unwrap();
    let first = load(&mut lua)?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let second = load(&mut lua)?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let execute = |lua: &mut Lua, closure: &luna::StashedClosure| -> Result<i64, ExternError> {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
        lua.execute(&executor)
    };
    assert_eq!(execute(&mut lua, &first)?, 42);
    let third = load(&mut lua)?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let stats = lua.jit_stats();
    assert_eq!(stats.cache_evictions, 1);
    assert_eq!(stats.compilation_failures, 1);
    assert_eq!(stats.code_bytes, 2 * module_bytes);
    lua.gc_collect();
    for closure in [&first, &third] {
        let before = lua.jit_stats().native_instructions;
        assert_eq!(execute(&mut lua, closure)?, 42);
        assert!(lua.jit_stats().native_instructions > before);
        lua.gc_collect();
    }
    let before = lua.jit_stats().native_instructions;
    assert_eq!(execute(&mut lua, &second)?, 42);
    assert_eq!(lua.jit_stats().native_instructions, before);
    assert_eq!(lua.jit_stats().cache_evictions, 1);
    drop((first, second, third));
    lua.gc_collect();
    lua.gc_collect();
    lua.service_jit().unwrap();
    let reclaimed = lua.jit_stats();
    assert_eq!(
        (
            reclaimed.code_bytes,
            reclaimed.metadata_bytes,
            reclaimed.snapshot_bytes
        ),
        (0, 0, 0)
    );
    Ok(())
}

fn state() -> Lua {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    })
    .unwrap();
    lua
}

#[test]
fn disabling_retires_code_without_destroying_suspended_lua_state() -> Result<(), ExternError> {
    let mut lua = state();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local s=0 for i=1,1000 do s=s+i end return s")?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    lua.enter(|ctx| {
        ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap();
    });
    let instructions = lua.jit_stats().native_instructions;
    assert!(instructions >= 64);
    assert!(lua.jit_stats().code_bytes > 0);
    lua.set_jit_config(JitConfig::default()).unwrap();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().queued_requests, 0);
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    lua.gc_collect();
    assert_eq!(lua.execute::<i64>(&executor)?, 500500);
    assert_eq!(lua.jit_stats().native_instructions, instructions);
    Ok(())
}

#[test]
fn lowering_native_quota_retires_code_and_refuses_recompilation_safely() -> Result<(), ExternError>
{
    let mut lua = state();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local t={} for i=1,100 do t[i]=i end return t[100]",
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    assert!(lua.jit_stats().code_bytes > 0);
    let mut config = lua.jit_config();
    config.max_code_bytes = 1;
    lua.set_jit_config(config).unwrap();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert!(matches!(
        lua.prepare_jit(),
        Err(JitError::ResourceLimit("native mappings"))
    ));
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 100);
    assert_eq!(lua.jit_stats().native_instructions, 0);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    Ok(())
}

#[test]
fn failed_compilation_is_bounded_until_explicit_admission_reset() -> Result<(), ExternError> {
    let mut lua = state();
    let mut config = lua.jit_config();
    config.max_code_bytes = 1;
    config.max_compile_attempts = 2;
    lua.set_jit_config(config).unwrap();
    let closure = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 40+2")?;
        Ok(ctx.stash(closure))
    })?;
    for attempt in 1..=2 {
        assert!(matches!(
            lua.prepare_jit(),
            Err(JitError::ResourceLimit("native mappings"))
        ));
        assert_eq!(lua.jit_stats().compilation_requests, attempt);
        assert_eq!(lua.jit_stats().compilation_failures, attempt);
        assert_eq!(lua.jit_stats().queued_requests, 0);
        assert_eq!(lua.jit_stats().code_bytes, 0);
    }
    for _ in 0..5 {
        assert_eq!(lua.prepare_jit().unwrap(), 0);
    }
    let mut config = lua.jit_config();
    config.max_code_bytes = JitConfig::default().max_code_bytes;
    lua.set_jit_config(config).unwrap();
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    lua.clear_jit_cache();
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert!(lua.jit_stats().native_instructions > 0);
    Ok(())
}
