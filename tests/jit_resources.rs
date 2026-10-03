#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{Closure, Executor, ExternError, JitConfig, JitError, JitMode, Lua, StashedExecutor};

#[path = "../examples/jit_support/churn.rs"]
mod churn;

#[test]
fn public_cache_pressure_bounds_retries_and_preserves_reusable_sources() {
    for mode in [churn::Mode::Off, churn::Mode::Auto, churn::Mode::Prepared] {
        for fuel in [1, 64] {
            let report = churn::run(mode, fuel).unwrap();
            assert!(report.calibration_ns > 0);
            assert!(report.module_bytes > 0);
            assert_eq!(report.quota_bytes, 2 * report.module_bytes);
            assert_eq!(report.prepare_ns > 0, mode == churn::Mode::Prepared);
            assert_eq!(report.passes.len(), 4);
            assert_eq!(
                report
                    .passes
                    .iter()
                    .map(|pass| pass.name)
                    .collect::<Vec<_>>(),
                ["warm", "revisit", "steady", "reset"]
            );
            assert_eq!(report.final_stats.code_bytes, 0);
            assert_eq!(report.final_stats.metadata_bytes, 0);
        }
    }
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
fn relocation_refusal_preserves_peer_and_interpretation_until_explicit_retry(
) -> Result<(), ExternError> {
    let mut lua = state();
    let mut config = lua.jit_config();
    config.max_relocations = 1;
    config.max_compile_attempts = 1;
    lua.set_jit_config(config).unwrap();
    let peer = lua.try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, b"return 42")?)))?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let baseline = lua.jit_stats();
    let candidate = lua.try_enter(|ctx| {
        Ok(ctx.stash(Closure::load(
            ctx,
            None,
            b"local t={} t.x=40 t.y=2 return t.x+t.y",
        )?))
    })?;
    assert!(matches!(
        lua.prepare_jit(),
        Err(JitError::ResourceLimit("native relocations"))
    ));
    let refused = lua.jit_stats();
    assert_eq!(refused.code_bytes, baseline.code_bytes);
    assert_eq!(refused.snapshot_bytes, 0);
    assert_eq!(refused.installed_regions, baseline.installed_regions);
    assert_eq!(
        refused.compilation_failures,
        baseline.compilation_failures + 1
    );
    assert_eq!(refused.cache_evictions, baseline.cache_evictions);
    let executor =
        lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&candidate).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert_eq!(lua.jit_stats().native_entries, refused.native_entries);
    assert_eq!(
        lua.jit_stats().compilation_failures,
        refused.compilation_failures
    );
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&peer).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert!(lua.jit_stats().native_instructions > refused.native_instructions);

    let mut config = lua.jit_config();
    config.max_relocations = JitConfig::default().max_relocations;
    lua.set_jit_config(config).unwrap();
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    lua.clear_jit_cache();
    assert_eq!(lua.prepare_jit().unwrap(), 2);
    let before = lua.jit_stats();
    let executor =
        lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&candidate).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    let after = lua.jit_stats();
    assert!(after.native_table_writes > before.native_table_writes);
    assert!(after.native_table_reads > before.native_table_reads);
    Ok(())
}

#[test]
fn lowering_relocation_limit_retires_code_and_queued_requests() -> Result<(), ExternError> {
    let mut lua = state();
    let candidate = lua.try_enter(|ctx| {
        Ok(ctx.stash(Closure::load(
            ctx,
            None,
            b"local t={} t.x=40 t.y=2 return t.x+t.y",
        )?))
    })?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    assert!(lua.jit_stats().code_bytes > 0);
    let pending = source(&mut lua, b"local s=0 for i=1,100 do s=s+i end return s")?;
    lua.enter(|ctx| {
        assert!(!ctx
            .fetch(&pending)
            .step(ctx, &mut luna::Fuel::empty())
            .unwrap());
    });
    assert!(lua.jit_stats().queued_requests > 0);
    let mut config = lua.jit_config();
    config.max_relocations = 1;
    lua.set_jit_config(config).unwrap();
    let stats = lua.jit_stats();
    assert_eq!(stats.code_bytes, 0);
    assert_eq!(stats.snapshot_bytes, 0);
    assert_eq!(stats.queued_requests, 0);
    assert_eq!(stats.registered_prototypes, 2);
    let executor =
        lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&candidate).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert_eq!(lua.jit_stats().native_entries, stats.native_entries);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    Ok(())
}

#[test]
fn ir_shape_refusal_preserves_interpretation_and_recovers_after_reset() -> Result<(), ExternError> {
    for instructions in [true, false] {
        let mut lua = state();
        let mut config = lua.jit_config();
        if instructions {
            config.max_ir_instructions = 1;
        } else {
            config.max_ir_blocks = 1;
        }
        lua.set_jit_config(config).unwrap();
        let closure = lua
            .try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, b"local x=40 return x+2")?)))?;
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        let expected = if instructions {
            "IR instructions"
        } else {
            "IR blocks"
        };
        assert!(
            matches!(lua.prepare_jit(), Err(JitError::ResourceLimit(reason)) if reason == expected)
        );
        let stats = lua.jit_stats();
        assert_eq!(stats.code_bytes, 0);
        assert_eq!(stats.snapshot_bytes, 0);
        assert_eq!(stats.native_entries, 0);
        assert_eq!(stats.compilation_failures, 1);
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 42);
        assert_eq!(lua.jit_stats().native_entries, 0);
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..JitConfig::default()
        })
        .unwrap();
        lua.clear_jit_cache();
        assert!(lua.prepare_jit().unwrap() > 0);
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        assert_eq!(lua.execute::<i64>(&executor)?, 42);
        assert!(lua.jit_stats().native_entries > 0);
        assert!(lua.jit_stats().native_instructions > 0);
    }
    Ok(())
}

#[test]
fn lowering_ir_caps_retires_native_code_and_preserves_live_source() -> Result<(), ExternError> {
    for instructions in [true, false] {
        let mut lua = state();
        let executor = source(&mut lua, b"local x=40 return x+2")?;
        assert_eq!(lua.prepare_jit().unwrap(), 1);
        assert!(lua.jit_stats().code_bytes > 0);
        let mut config = lua.jit_config();
        if instructions {
            config.max_ir_instructions = 1;
        } else {
            config.max_ir_blocks = 1;
        }
        lua.set_jit_config(config).unwrap();
        let stats = lua.jit_stats();
        assert_eq!(
            (
                stats.code_bytes,
                stats.snapshot_bytes,
                stats.queued_requests
            ),
            (0, 0, 0)
        );
        assert_eq!(stats.registered_prototypes, 1);
        assert_eq!(lua.execute::<i64>(&executor)?, 42);
        assert_eq!(lua.jit_stats().native_entries, 0);
        assert_eq!(lua.jit_stats().code_bytes, 0);
    }
    Ok(())
}

fn source(lua: &mut Lua, text: &[u8]) -> Result<StashedExecutor, ExternError> {
    lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("resources"), text)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })
}

#[test]
fn scalar_verifier_workspace_refusal_preserves_source_and_recovers() -> Result<(), ExternError> {
    let script = b"local x=40 return x+2";
    let mut probe = state();
    let _probe = source(&mut probe, script)?;
    assert_eq!(probe.prepare_jit().unwrap(), 1);
    let peak = probe.jit_stats().snapshot_peak_bytes;
    let mut lua = state();
    let mut config = lua.jit_config();
    config.max_snapshot_bytes = peak - 1;
    lua.set_jit_config(config).unwrap();
    let closure =
        lua.try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, Some("resources"), script)?)))?;
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
    assert!(matches!(
        lua.prepare_jit(),
        Err(JitError::ResourceLimit("scalar tag verification"))
    ));
    let stats = lua.jit_stats();
    assert_eq!(
        (
            stats.snapshot_bytes,
            stats.code_bytes,
            stats.native_entries,
            stats.cache_evictions
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert_eq!(lua.jit_stats().native_entries, 0);
    let mut config = lua.jit_config();
    config.max_snapshot_bytes = JitConfig::default().max_snapshot_bytes;
    lua.set_jit_config(config).unwrap();
    lua.clear_jit_cache();
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert!(lua.jit_stats().native_instructions > 0);
    Ok(())
}

#[test]
fn warm_source_identity_survives_code_clear_but_not_registration_reset() -> Result<(), ExternError>
{
    fn execute(lua: &mut Lua, closure: &luna::StashedClosure) -> Result<i64, ExternError> {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
        lua.execute(&executor)
    }
    let mut lua = state();
    let closure = lua.try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, b"return 42")?)))?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    assert_eq!(execute(&mut lua, &closure)?, 42);
    assert!(lua.jit_stats().native_instructions > 0);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().registered_prototypes, 1);
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let before = lua.jit_stats().native_instructions;
    assert_eq!(execute(&mut lua, &closure)?, 42);
    assert!(lua.jit_stats().native_instructions > before);
    lua.set_jit_config(JitConfig::default()).unwrap();
    let before = lua.jit_stats().native_instructions;
    assert_eq!(execute(&mut lua, &closure)?, 42);
    assert_eq!(lua.jit_stats().native_instructions, before);
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    })
    .unwrap();
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    assert_eq!(execute(&mut lua, &closure)?, 42);
    assert!(lua.jit_stats().native_instructions > before);
    let mut config = lua.jit_config();
    config.max_metadata_bytes = 1;
    lua.set_jit_config(config).unwrap();
    let before = lua.jit_stats();
    assert_eq!(execute(&mut lua, &closure)?, 42);
    assert_eq!(
        lua.jit_stats().native_instructions,
        before.native_instructions
    );
    assert_eq!(lua.jit_stats().code_lookups, before.code_lookups);
    let mut config = lua.jit_config();
    config.max_metadata_bytes = JitConfig::default().max_metadata_bytes;
    lua.set_jit_config(config).unwrap();
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    let replacement =
        lua.try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, b"return 144")?)))?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    lua.gc_collect();
    let before = lua.jit_stats();
    assert_eq!(execute(&mut lua, &closure)?, 42);
    assert_eq!(
        lua.jit_stats().native_instructions,
        before.native_instructions
    );
    assert_eq!(lua.jit_stats().code_lookups, before.code_lookups);
    assert_eq!(execute(&mut lua, &replacement)?, 144);
    assert!(lua.jit_stats().native_instructions > before.native_instructions);
    Ok(())
}

#[test]
fn sweeping_sparse_sources_compacts_metadata_and_preserves_live_native_identity(
) -> Result<(), ExternError> {
    let mut lua = state();
    let mut closures = Vec::new();
    for value in 0..128 {
        let closure = lua.try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                Some("sparse-source"),
                format!("local n={value} return n+1").as_bytes(),
            )?;
            Ok(ctx.stash(closure))
        })?;
        closures.push((value, closure));
    }
    let mut installed = 0;
    for _ in 0..8 {
        installed += lua.prepare_jit().unwrap();
    }
    assert_eq!(installed, 128);
    let before = lua.jit_stats();
    assert_eq!(before.registered_prototypes, 128);
    closures.retain(|(value, _)| [0, 61, 127].contains(value));
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.service_jit().unwrap(), 0);
    let compacted = lua.jit_stats();
    assert_eq!(compacted.registered_prototypes, 3);
    assert!(compacted.metadata_bytes < before.metadata_bytes / 4);
    assert!(compacted.code_bytes < before.code_bytes / 4);
    assert!(compacted.metadata_compactions >= before.metadata_compactions + 3);
    assert!(compacted.metadata_compaction_bytes > before.metadata_compaction_bytes);
    assert_eq!(compacted.metadata_compaction_refusals, 0);
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    for (value, closure) in &closures {
        let native = lua.jit_stats().native_instructions;
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
        assert_eq!(lua.execute::<i64>(&executor)?, value + 1);
        assert!(lua.jit_stats().native_instructions > native);
    }
    drop(closures);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.service_jit().unwrap(), 0);
    let final_stats = lua.jit_stats();
    assert_eq!(
        (
            final_stats.metadata_bytes,
            final_stats.code_bytes,
            final_stats.registered_prototypes
        ),
        (0, 0, 0)
    );
    Ok(())
}

#[test]
fn metadata_refusal_keeps_source_loading_and_interpretation_usable() -> Result<(), ExternError> {
    let mut lua = state();
    let mut config = lua.jit_config();
    config.max_metadata_bytes = 1;
    lua.set_jit_config(config).unwrap();
    let executor = source(&mut lua, b"return 42")?;
    let stats = lua.jit_stats();
    assert_eq!(stats.metadata_bytes, 0);
    assert_eq!(stats.registered_prototypes, 0);
    assert!(stats.registration_refusals > 0);
    assert!(stats.metadata_allocation_refusals > 0);
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert_eq!(lua.jit_stats().native_entries, 0);
    Ok(())
}

#[test]
fn snapshot_budget_is_independent_of_weak_registration_storage() -> Result<(), ExternError> {
    let mut lua = state();
    let mut config = lua.jit_config();
    config.max_snapshot_bytes = 1;
    lua.set_jit_config(config).unwrap();
    let (_closure, executor) = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("snapshot-budget"), &b"return 42"[..])?;
        Ok((
            ctx.stash(closure),
            ctx.stash(Executor::start(ctx, closure.into(), ())),
        ))
    })?;
    assert_eq!(lua.jit_stats().registered_prototypes, 1);
    assert!(lua.jit_stats().metadata_bytes > 0);
    assert!(matches!(
        lua.prepare_jit(),
        Err(JitError::ResourceLimit("prototype snapshot"))
    ));
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    let mut config = lua.jit_config();
    config.max_snapshot_bytes = JitConfig::default().max_snapshot_bytes;
    lua.set_jit_config(config).unwrap();
    lua.clear_jit_cache();
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    assert!(lua.jit_stats().snapshot_peak_bytes > 0);
    Ok(())
}

#[test]
fn lowering_metadata_limit_retires_registrations_without_destroying_live_closures(
) -> Result<(), ExternError> {
    let mut lua = state();
    let mut executors = Vec::new();
    for value in 0..20 {
        executors.push(source(&mut lua, format!("return {value}").as_bytes())?);
    }
    assert_eq!(lua.jit_stats().registered_prototypes, 20);
    assert!(lua.prepare_jit().unwrap() > 0);
    assert!(lua.jit_stats().code_bytes > 0);
    assert!(lua.jit_stats().metadata_bytes > 0);
    let peak = lua.jit_stats().metadata_peak_bytes;
    let mut config = lua.jit_config();
    config.max_metadata_bytes = 1;
    lua.set_jit_config(config).unwrap();
    let stats = lua.jit_stats();
    assert_eq!(
        (
            stats.registered_prototypes,
            stats.metadata_bytes,
            stats.code_bytes,
            stats.queued_requests
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(stats.metadata_peak_bytes, peak);
    lua.gc_collect();
    for (value, executor) in executors.iter().enumerate() {
        assert_eq!(lua.execute::<i64>(executor)?, value as i64);
    }
    let mut config = lua.jit_config();
    config.max_metadata_bytes = JitConfig::default().max_metadata_bytes;
    lua.set_jit_config(config).unwrap();
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    let new = source(&mut lua, b"return 1001")?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    assert_eq!(lua.execute::<i64>(&new)?, 1001);
    assert!(lua.jit_stats().native_instructions > 0);
    Ok(())
}

#[test]
fn sweeping_the_last_source_reclaims_container_capacity_and_native_mappings(
) -> Result<(), ExternError> {
    let mut lua = state();
    let executor = source(&mut lua, b"return 42")?;
    lua.prepare_jit().unwrap();
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert!(lua.jit_stats().metadata_bytes > 0);
    drop(executor);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.service_jit().unwrap(), 0);
    let stats = lua.jit_stats();
    assert_eq!(
        (
            stats.metadata_bytes,
            stats.code_bytes,
            stats.snapshot_bytes,
            stats.registered_prototypes
        ),
        (0, 0, 0, 0)
    );
    assert!(stats.metadata_peak_bytes > 0);
    assert!(stats.snapshot_peak_bytes > 0);
    Ok(())
}

#[test]
fn preparation_buffer_refusal_is_typed_and_leaves_registrations_usable() -> Result<(), ExternError>
{
    let mut probe = state();
    let _probe = source(&mut probe, b"return 42")?;
    let exact_storage = probe.jit_stats().metadata_bytes;
    let mut lua = state();
    let mut config = lua.jit_config();
    config.max_metadata_bytes = exact_storage;
    lua.set_jit_config(config).unwrap();
    let executor = source(&mut lua, b"return 42")?;
    assert_eq!(lua.jit_stats().registered_prototypes, 1);
    assert_eq!(lua.jit_stats().metadata_bytes, exact_storage);
    assert!(matches!(
        lua.prepare_jit(),
        Err(JitError::ResourceLimit("JIT metadata"))
    ));
    assert_eq!(lua.jit_stats().queued_requests, 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert_eq!(lua.jit_stats().native_entries, 0);
    assert!(lua.jit_stats().metadata_allocation_refusals > 0);
    Ok(())
}
