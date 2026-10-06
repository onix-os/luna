use super::*;
use crate::jit::{
    ir::Snapshot,
    native_region::tests::{fixture_source, trace},
    PreparedPair,
};
use crate::{thread::activation::with_test_thread, Closure, Context, Fuel, JitConfig, Lua};

const SOURCE: &[u8] =
    b"local n=0 local function f(v) n=n+v end for i=1,10 do f(i) f(i) f(i) end return n";
const DISTINCT: &[u8] = b"local n=0 local function f(v) n=n+v end local function g(v) n=n+v end local function h(v) n=n+v end f(1) g(2) h(3) return n";

fn regions<'gc>(ctx: Context<'gc>, caller: Closure<'gc>, first: Region) -> Vec<Region> {
    let prototype = caller.prototype();
    let first_key = first.pair.program.key();
    let allocator = ctx.jit().0.borrow().snapshots.clone();
    let mut regions = vec![first];
    for (index, (pc, _)) in prototype
        .opcodes
        .iter()
        .enumerate()
        .filter(|(_, op)| matches!(op.decode(), crate::opcode::Operation::Call { .. }))
        .enumerate()
        .skip(1)
    {
        let callee = prototype.prototypes[if prototype.prototypes.len() == 1 {
            0
        } else {
            index
        }];
        let key = Key {
            caller: first_key.caller,
            callee: ctx.jit_registry().borrow().identity(ctx, callee).unwrap(),
            pc,
        };
        ctx.jit().observe_pair(key);
        ctx.jit()
            .compile_pair(
                key,
                Snapshot::new_in(&prototype, 4096, allocator.clone()).unwrap(),
                Snapshot::new_in(&callee, 4096, allocator.clone()).unwrap(),
            )
            .unwrap();
        regions.push(
            Region::new(
                ctx,
                caller,
                PreparedPair {
                    program: ctx.jit().pair_lease(key).unwrap(),
                },
            )
            .unwrap(),
        );
    }
    assert_eq!(regions.len(), 3);
    regions
}

fn verify(cache: &Cache) {
    let mut seen = std::collections::HashSet::new();
    for (&caller, &head) in &cache.callers {
        let mut current = Some(head);
        let mut previous = None;
        while let Some(key) = current {
            assert!(seen.insert(key), "cyclic or multiply indexed region");
            assert_eq!(key.caller, caller);
            let entry = &cache.entries[&key];
            assert_eq!(entry.region.pair.program.key(), key);
            assert_eq!(entry.previous, previous);
            previous = Some(key);
            current = entry.next;
        }
    }
    assert_eq!(seen.len(), cache.entries.len());
    if seen.is_empty() {
        assert_eq!(cache.entries.capacity(), 0);
        assert_eq!(cache.callers.capacity(), 0);
    }
}

#[test]
fn live_caller_selection_executes_all_sites_with_one_lookup_and_lease() {
    fixture_source(SOURCE, |ctx, caller, first, start| {
        let regions = regions(ctx, caller, first);
        let selected = regions.last().unwrap().pair.program.key();
        for region in regions {
            ctx.jit().install_region(region).unwrap();
        }
        let before = ctx.jit().0.borrow().stats;
        let clock = ctx.jit().0.borrow().clock;
        let lease = ctx.jit().region_for(ctx, caller).unwrap();
        let after = ctx.jit().0.borrow().stats;
        assert_eq!(lease.pair.program.key(), selected);
        assert_eq!(after.code_lookups, before.code_lookups + 1);
        assert_eq!(after.code_leases, before.code_leases + 1);
        assert_eq!(ctx.jit().0.borrow().clock, clock + 1);
        verify(&ctx.jit().0.borrow().pairs.as_ref().unwrap().regions);
        let mut slices = 0;
        let mut run = |native| {
            with_test_thread(ctx, caller, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                let stats = ctx.jit().0.borrow().stats;
                if native {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let outcome = lease.run(ctx, host, 64, 64).unwrap();
                    outcome.result.unwrap();
                    assert_eq!(outcome.pairs, 30);
                    slices = outcome.slices;
                } else {
                    host.run(ctx, slices, 64, 4).result.unwrap();
                }
                let result = trace(
                    ctx,
                    host,
                    (
                        stats.total_dispatches,
                        stats.native_instructions + stats.interpreted_instructions,
                    ),
                );
                assert_eq!(result.1, vec![(crate::jit::abi::INTEGER, 165)]);
                result
            })
        };
        assert_eq!(run(true), run(false));
    });
}

#[test]
fn index_replacement_and_each_removal_order_preserve_sibling_links() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        fixture_source(SOURCE, |ctx, caller, first, _| {
            let regions = regions(ctx, caller, first);
            let keys: Vec<_> = regions.iter().map(|r| r.pair.program.key()).collect();
            for region in regions {
                ctx.jit().install_region(region).unwrap();
            }
            let old = ctx.jit().region_lease(keys[0]).unwrap();
            let replacement = Region::new(
                ctx,
                caller,
                PreparedPair {
                    program: ctx.jit().pair_lease(keys[0]).unwrap(),
                },
            )
            .unwrap();
            ctx.jit().install_region(replacement).unwrap();
            assert!(!Shared::ptr_eq(
                &old,
                &ctx.jit().region_lease(keys[0]).unwrap()
            ));
            assert_eq!(Shared::strong_count(&old), 1);
            let mut expected = keys.clone();
            for index in order {
                assert_eq!(
                    ctx.jit()
                        .region_for(ctx, caller)
                        .unwrap()
                        .pair
                        .program
                        .key(),
                    *expected.last().unwrap()
                );
                {
                    let mut manager = ctx.jit().0.borrow_mut();
                    let cache = &mut manager.pairs.as_mut().unwrap().regions;
                    cache.remove(keys[index]);
                    cache.remove(keys[index]);
                    verify(cache);
                }
                assert!(ctx.jit().region_lease(keys[index]).is_none());
                expected.retain(|key| *key != keys[index]);
            }
            assert!(ctx.jit().region_for(ctx, caller).is_none());
        });
    }
}

#[test]
fn retirement_unlinks_callee_sites_without_reordering_survivors() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        fixture_source(DISTINCT, |ctx, caller, first, _| {
            let regions = regions(ctx, caller, first);
            let keys: Vec<_> = regions.iter().map(|r| r.pair.program.key()).collect();
            for region in regions {
                ctx.jit().install_region(region).unwrap();
            }
            let mut expected = keys.clone();
            for index in order {
                assert_eq!(
                    ctx.jit()
                        .region_for(ctx, caller)
                        .unwrap()
                        .pair
                        .program
                        .key(),
                    *expected.last().unwrap()
                );
                {
                    let mut manager = ctx.jit().0.borrow_mut();
                    manager.retire(keys[index].callee);
                    verify(&manager.pairs.as_ref().unwrap().regions);
                }
                expected.retain(|key| *key != keys[index]);
            }
            assert!(ctx.jit().region_for(ctx, caller).is_none());
        });
    }
}

#[test]
fn disabled_hooked_and_foreign_live_selection_is_effect_free() {
    fixture_source(SOURCE, |ctx, caller, region, _| {
        ctx.jit().install_region(region).unwrap();
        for condition in 0..3 {
            let before = ctx.jit().0.borrow().stats;
            let clock = ctx.jit().0.borrow().clock;
            match condition {
                0 => ctx.jit().0.borrow_mut().config.mode = JitMode::Off,
                1 => {
                    let callback =
                        crate::Callback::from_fn(&ctx, |_, _, _| panic!("selection ran hook"));
                    ctx.set_debug_hook(callback.into(), false, 1);
                }
                _ => {}
            }
            if condition == 2 {
                let mut foreign = Lua::empty();
                foreign
                    .set_jit_config(JitConfig {
                        mode: JitMode::Auto,
                        ..Default::default()
                    })
                    .unwrap();
                foreign.enter(|other| {
                    let closure = Closure::load(other, None, SOURCE).unwrap();
                    assert_eq!(
                        ctx.jit_registry()
                            .borrow()
                            .identity(ctx, caller.prototype()),
                        other
                            .jit_registry()
                            .borrow()
                            .identity(other, closure.prototype())
                    );
                    assert!(ctx.jit().region_for(other, closure).is_none());
                });
            } else {
                assert!(ctx.jit().region_for(ctx, caller).is_none());
            }
            assert_eq!(ctx.jit().0.borrow().stats, before);
            assert_eq!(ctx.jit().0.borrow().clock, clock);
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
            ctx.set_debug_hook(crate::Value::Nil, false, 0);
        }
        assert!(ctx.jit().region_for(ctx, caller).is_some());
    });
}

#[test]
fn stale_selected_dependency_is_unlinked_once_without_scanning_siblings() {
    fixture_source(SOURCE, |ctx, caller, first, _| {
        let mut sites = regions(ctx, caller, first);
        let stale = sites.pop().unwrap();
        let stale_key = stale.pair.program.key();
        let remaining = sites.last().unwrap().pair.program.key();
        for region in sites {
            ctx.jit().install_region(region).unwrap();
        }
        let allocator = ctx.jit().0.borrow().snapshots.clone();
        ctx.jit()
            .compile_pair(
                stale_key,
                Snapshot::new_in(&caller.prototype(), 4096, allocator.clone()).unwrap(),
                Snapshot::new_in(&caller.prototype().prototypes[0], 4096, allocator).unwrap(),
            )
            .unwrap();
        {
            let mut manager = ctx.jit().0.borrow_mut();
            let clock = manager.clock;
            manager
                .pairs
                .as_mut()
                .unwrap()
                .regions
                .insert(stale, clock)
                .unwrap();
            verify(&manager.pairs.as_ref().unwrap().regions);
        }
        let before = ctx.jit().0.borrow().stats;
        let clock = ctx.jit().0.borrow().clock;
        assert!(ctx.jit().region_for(ctx, caller).is_none());
        let after = ctx.jit().0.borrow().stats;
        assert_eq!(after.code_lookups, before.code_lookups + 1);
        assert_eq!(after.code_leases, before.code_leases);
        assert_eq!(ctx.jit().0.borrow().clock, clock);
        {
            let manager = ctx.jit().0.borrow();
            let cache = &manager.pairs.as_ref().unwrap().regions;
            verify(cache);
            assert!(!cache.contains(stale_key));
            assert_eq!(cache.caller(stale_key.caller), Some(remaining));
        }
        assert_eq!(
            ctx.jit()
                .region_for(ctx, caller)
                .unwrap()
                .pair
                .program
                .key(),
            remaining
        );
    });
}

#[test]
fn caller_index_growth_refusal_preserves_existing_heads_and_recovers() {
    fixture_source(SOURCE, |ctx, caller, first, _| {
        let mut callers = vec![caller];
        ctx.jit().install_region(first).unwrap();
        for _ in 0..2 {
            let peer = Closure::load(ctx, None, SOURCE).unwrap();
            let region = crate::jit::native_region::tests::build_region(ctx, peer).0;
            ctx.jit().install_region(region).unwrap();
            callers.push(peer);
        }
        let peer = Closure::load(ctx, None, SOURCE).unwrap();
        let region = crate::jit::native_region::tests::build_region(ctx, peer).0;
        let key = region.pair.program.key();
        let originals: Vec<_> = callers
            .iter()
            .map(|&caller| ctx.jit().region_for(ctx, caller).unwrap())
            .collect();
        let metadata = ctx.jit().0.borrow().metadata.0.clone();
        let before = ctx.jit().0.borrow().stats;
        let clock = ctx.jit().0.borrow().clock;
        {
            let manager = ctx.jit().0.borrow();
            let cache = &manager.pairs.as_ref().unwrap().regions;
            assert_eq!(cache.entries.len(), cache.entries.capacity());
            assert_eq!(cache.callers.len(), cache.callers.capacity());
        }
        metadata.fail_after(2);
        assert!(matches!(
            ctx.jit().install_region(region),
            Err(JitError::ResourceLimit("region caller index metadata"))
        ));
        metadata.fail_after(usize::MAX);
        assert_eq!(ctx.jit().0.borrow().clock, clock);
        assert_eq!(ctx.jit().0.borrow().stats, before);
        assert!(ctx.jit().region_for(ctx, peer).is_none());
        verify(&ctx.jit().0.borrow().pairs.as_ref().unwrap().regions);
        for (&caller, original) in callers.iter().zip(&originals) {
            assert!(Shared::ptr_eq(
                original,
                &ctx.jit().region_for(ctx, caller).unwrap()
            ));
        }
        assert!(ctx.jit().lookup(key.caller).is_some());
        let replacement = Region::new(
            ctx,
            peer,
            PreparedPair {
                program: ctx.jit().pair_lease(key).unwrap(),
            },
        )
        .unwrap();
        ctx.jit().install_region(replacement).unwrap();
        assert_eq!(
            ctx.jit().region_for(ctx, peer).unwrap().pair.program.key(),
            key
        );
        verify(&ctx.jit().0.borrow().pairs.as_ref().unwrap().regions);
        ctx.jit().0.borrow_mut().retire(key.caller);
        assert!(ctx.jit().region_for(ctx, peer).is_none());
        for (&caller, original) in callers.iter().zip(&originals) {
            assert!(Shared::ptr_eq(
                original,
                &ctx.jit().region_for(ctx, caller).unwrap()
            ));
        }
    });
}

#[test]
fn sparse_index_compaction_refusal_preserves_links_leases_and_accounting() {
    fixture_source(SOURCE, |ctx, caller, first, _| {
        for region in regions(ctx, caller, first) {
            ctx.jit().install_region(region).unwrap();
        }
        let lease = ctx.jit().region_for(ctx, caller).unwrap();
        let key = lease.pair.program.key();
        let metadata = ctx.jit().0.borrow().metadata.0.clone();
        let mut manager = ctx.jit().0.borrow_mut();
        let cache = &mut manager.pairs.as_mut().unwrap().regions;
        cache.entries.try_reserve(128).unwrap();
        cache.callers.try_reserve(128).unwrap();
        let before = metadata.current();
        assert!(cache.needs_compaction());
        metadata.fail_after(0);
        assert!(cache
            .compact()
            .iter()
            .all(|result| matches!(result, Compaction::Refused)));
        assert_eq!(metadata.current(), before);
        verify(cache);
        metadata.fail_after(usize::MAX);
        for _ in 0..8 {
            assert!(cache
                .compact()
                .iter()
                .all(|result| matches!(result, Compaction::Unchanged)));
        }
        let released: usize = cache
            .compact()
            .into_iter()
            .map(|result| match result {
                Compaction::Compacted(bytes) => bytes,
                _ => panic!("sparse index did not compact"),
            })
            .sum();
        assert!(released > 0);
        assert_eq!(metadata.current(), before - released);
        assert!(Shared::ptr_eq(cache.get(key).unwrap(), &lease));
        verify(cache);
        cache.retire_caller(key.caller);
        verify(cache);
        assert_eq!(Shared::strong_count(&lease), 1);
    });
}
