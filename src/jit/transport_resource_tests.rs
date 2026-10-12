use super::{backend, JitConfig, JitError, JitMode};
use crate::{Closure, Executor, Lua, StashedClosure};

#[path = "../../examples/jit_support/resource_workloads.rs"]
mod workloads;

use super::backend::EntryKind;

const TABLE: &[u8] = b"local t={} t.x=40 t.y=2 return t.x+t.y";
const SCALAR: &[u8] = b"local x=40 return x+2";
const SELECTIONS: [backend::Failure; 2] =
    [backend::Failure::None, backend::Failure::PreferBoundPayload];

fn state(selection: backend::Failure, relocations: usize, snapshots: usize) -> Lua {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        max_compile_attempts: 1,
        max_relocations: relocations,
        max_snapshot_bytes: snapshots,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = selection);
    lua
}

fn source(lua: &mut Lua, script: &[u8]) -> StashedClosure {
    lua.enter(|ctx| ctx.stash(Closure::load(ctx, Some("resources"), script).unwrap()))
}

fn execute(lua: &mut Lua, closure: &StashedClosure) {
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 42);
}

fn installed(lua: &mut Lua) -> (EntryKind, usize) {
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        assert_eq!(manager.code.len(), 1);
        let entry = manager.code.values().next().unwrap();
        (entry.code.entry_kind, entry.code.relocations)
    })
}

fn expected_entry(selection: backend::Failure, script: &[u8]) -> EntryKind {
    if selection == backend::Failure::PreferBoundPayload && [TABLE, SCALAR].contains(&script) {
        EntryKind::FrameBound
    } else {
        EntryKind::Ordinary
    }
}

fn recover(lua: &mut Lua, closure: &StashedClosure) {
    let refused = lua.jit_stats();
    assert_eq!(refused.snapshot_bytes, 0);
    assert_eq!(refused.code_bytes, 0);
    assert_eq!(refused.code_requested_bytes, 0);
    assert_eq!(refused.installed_regions, 0);
    assert_eq!(refused.compilation_failures, 1);
    assert_eq!(refused.cache_evictions, 0);
    lua.gc_collect();
    execute(lua, closure);
    assert_eq!(lua.jit_stats().native_entries, 0);
    assert_eq!(lua.jit_stats().compilation_failures, 1);
    let mut config = lua.jit_config();
    config.max_relocations = JitConfig::default().max_relocations;
    config.max_snapshot_bytes = JitConfig::default().max_snapshot_bytes;
    lua.set_jit_config(config).unwrap();
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    lua.clear_jit_cache();
    lua.gc_collect();
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    execute(lua, closure);
    assert!(lua.jit_stats().native_instructions > 0);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn relocation_admission_matches_emitted_count_for_both_transports() {
    let defaults = JitConfig::default();
    for selection in SELECTIONS {
        for script in [
            TABLE,
            SCALAR,
            workloads::RELOCATIONS,
            workloads::SCALAR_VERIFIER,
        ] {
            let mut probe = state(
                selection,
                defaults.max_relocations,
                defaults.max_snapshot_bytes,
            );
            let closure = source(&mut probe, script);
            assert_eq!(probe.prepare_jit().unwrap(), 1);
            let (kind, relocations) = installed(&mut probe);
            assert_eq!(kind, expected_entry(selection, script));
            if script == workloads::RELOCATIONS {
                assert!(relocations > 1);
            }
            execute(&mut probe, &closure);
            assert!(probe.jit_stats().native_instructions > 0);
            let mut limits = vec![1, relocations.max(1), relocations.saturating_sub(1).max(1)];
            limits.sort_unstable();
            limits.dedup();
            for limit in limits {
                let mut lua = state(selection, limit, defaults.max_snapshot_bytes);
                let closure = source(&mut lua, script);
                let prepared = lua.prepare_jit();
                if relocations > limit {
                    assert!(
                        matches!(prepared, Err(JitError::ResourceLimit("native relocations"))),
                        "{kind:?} relocations={relocations} limit={limit}: {prepared:?}"
                    );
                    recover(&mut lua, &closure);
                } else {
                    assert_eq!(prepared.unwrap(), 1);
                    assert_eq!(installed(&mut lua), (kind, relocations));
                    assert_eq!(lua.jit_stats().compilation_failures, 0);
                    execute(&mut lua, &closure);
                    assert!(lua.jit_stats().native_instructions > 0);
                    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
                }
            }
        }
    }
}

#[test]
fn workspace_refusal_preserves_source_and_retry_for_both_transports() {
    let defaults = JitConfig::default();
    for selection in SELECTIONS {
        for script in [
            TABLE,
            SCALAR,
            workloads::RELOCATIONS,
            workloads::SCALAR_VERIFIER,
        ] {
            let mut probe = state(
                selection,
                defaults.max_relocations,
                defaults.max_snapshot_bytes,
            );
            let closure = source(&mut probe, script);
            assert_eq!(probe.prepare_jit().unwrap(), 1);
            let (kind, _) = installed(&mut probe);
            assert_eq!(kind, expected_entry(selection, script));
            let peak = probe.jit_stats().snapshot_peak_bytes;
            assert!(peak > 1);
            execute(&mut probe, &closure);
            for limit in [peak - 1, peak] {
                let mut lua = state(selection, defaults.max_relocations, limit);
                let closure = source(&mut lua, script);
                let prepared = lua.prepare_jit();
                if limit < peak {
                    let expected = if kind == EntryKind::FrameBound {
                        "frontend dominance storage"
                    } else {
                        "scalar tag verification"
                    };
                    assert!(
                        matches!(prepared, Err(JitError::ResourceLimit(reason)) if reason == expected),
                        "{kind:?} limit={limit}: {prepared:?}"
                    );
                    recover(&mut lua, &closure);
                } else {
                    assert_eq!(prepared.unwrap(), 1);
                    assert_eq!(installed(&mut lua).0, kind);
                    execute(&mut lua, &closure);
                    assert!(lua.jit_stats().native_instructions > 0);
                    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
                    assert_eq!(lua.jit_stats().snapshot_peak_bytes, peak);
                }
            }
        }
    }
}

#[test]
fn relocation_refusal_preserves_installed_peer_for_both_transports() {
    let defaults = JitConfig::default();
    for selection in SELECTIONS {
        let mut lua = state(selection, 1, defaults.max_snapshot_bytes);
        let peer = source(&mut lua, b"return 42");
        assert_eq!(lua.prepare_jit().unwrap(), 1);
        let baseline = lua.jit_stats();
        let candidate = source(&mut lua, workloads::RELOCATIONS);
        assert!(matches!(
            lua.prepare_jit(),
            Err(JitError::ResourceLimit("native relocations"))
        ));
        let refused = lua.jit_stats();
        assert_eq!(refused.code_bytes, baseline.code_bytes);
        assert_eq!(refused.code_requested_bytes, baseline.code_requested_bytes);
        assert_eq!(refused.installed_regions, baseline.installed_regions);
        assert_eq!(refused.cache_evictions, baseline.cache_evictions);
        assert_eq!(refused.snapshot_bytes, 0);
        assert_eq!(refused.compilation_failures, 1);
        lua.gc_collect();
        execute(&mut lua, &candidate);
        assert_eq!(lua.jit_stats().native_entries, 0);
        execute(&mut lua, &peer);
        assert!(lua.jit_stats().native_instructions > 0);
        let mut config = lua.jit_config();
        config.max_relocations = defaults.max_relocations;
        lua.set_jit_config(config).unwrap();
        assert_eq!(lua.prepare_jit().unwrap(), 0);
        lua.clear_jit_cache();
        lua.gc_collect();
        assert_eq!(lua.prepare_jit().unwrap(), 2);
        let before = lua.jit_stats();
        execute(&mut lua, &candidate);
        let after = lua.jit_stats();
        assert!(after.native_table_reads > before.native_table_reads);
        assert!(after.native_table_writes > before.native_table_writes);
        assert_eq!(after.snapshot_bytes, 0);
    }
}
