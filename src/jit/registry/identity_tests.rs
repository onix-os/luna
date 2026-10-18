use crate::{Closure, Lua};

#[test]
fn cached_hashes_revalidate_entries_and_reset_with_the_seed() {
    use ottavino_gc_arena::Gc;
    Lua::empty().enter(|ctx| {
        let first = Closure::load(ctx, None, b"return 1").unwrap().prototype();
        let second = Closure::load(ctx, None, b"return 2").unwrap().prototype();
        let mut registry = ctx.jit_registry().borrow_mut(&ctx);
        let id = registry.identity(ctx, first).unwrap();
        let other_id = registry.identity(ctx, second).unwrap();
        let cached = registry.address_hashes.each_ref().map(|slot| slot.get());
        for _ in 0..32 {
            assert_eq!(registry.identity(ctx, first), Some(id));
            assert_eq!(registry.identity(ctx, second), Some(other_id));
            assert_eq!(
                registry.address_hashes.each_ref().map(|slot| slot.get()),
                cached
            );
        }
        registry.entries.try_reserve(128).unwrap();
        assert_eq!(registry.identity(ctx, first), Some(id));
        let address = Gc::as_ptr(first) as usize;
        registry.entries.get_mut(&address).unwrap().prototype = Gc::downgrade(second);
        assert_eq!(registry.identity(ctx, first), None);
        registry.entries.get_mut(&address).unwrap().prototype = Gc::downgrade(first);
        registry.entries.get_mut(&address).unwrap().id = u64::MAX;
        assert_eq!(registry.identity(ctx, first), Some(u64::MAX));
        registry.entries.get_mut(&address).unwrap().id = id;
        registry.reset(ctx);
        assert_eq!(
            registry.address_hashes.each_ref().map(|slot| slot.get()),
            [(0, 0); 2]
        );
        assert_eq!(registry.identity(ctx, first), None);
        assert_eq!(registry.address_hashes[0].get().0, address);
        assert_eq!(registry.identity(ctx, first), None);
        registry.register(ctx, first);
        assert_ne!(registry.identity(ctx, first), Some(id));
        assert!(registry.identity(ctx, first).is_some());
    });
}

#[test]
fn address_hashes_match_map_lookup_for_wide_keys_and_new_seeds() {
    use crate::jit::resources::{BudgetAllocator, Ledger};
    use std::hash::{BuildHasher, Hash, Hasher};

    let keys = [
        0,
        1,
        16,
        256,
        usize::MAX,
        usize::MAX - 1,
        1usize << (usize::BITS - 1),
    ];
    for _ in 0..32 {
        let ledger = Ledger::new(1024 * 1024);
        let mut map = crate::jit::metadata_map(BudgetAllocator(ledger.clone()));
        for (value, key) in keys.into_iter().enumerate() {
            map.insert(key, value);
        }
        for sparse in [false, true] {
            if sparse {
                map.try_reserve(128).unwrap();
            }
            let before = ledger.current();
            for key in keys.into_iter().chain([7, 31, usize::MAX / 2]) {
                let mut hasher = map.hasher().build_hasher();
                key.hash(&mut hasher);
                assert_eq!(
                    map.raw_entry()
                        .from_hash(hasher.finish(), |stored| *stored == key)
                        .map(|(_, value)| value),
                    map.get(&key),
                );
            }
            assert_eq!(ledger.current(), before);
        }
        drop(map);
        assert_eq!(ledger.current(), 0);
    }
}

#[test]
fn identity_matches_hash_lookup_at_dense_and_sparse_capacities() {
    Lua::empty().enter(|ctx| {
        let sources: Vec<_> = (0..64)
            .map(|_| Closure::load(ctx, None, b"return 7").unwrap().prototype())
            .collect();
        let mut registry = ctx.jit_registry().borrow_mut(&ctx);
        for count in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 16, 32, 64] {
            registry.reset(ctx);
            for &prototype in &sources[..count] {
                registry.register(ctx, prototype);
            }
            assert_eq!(registry.entries.len(), count);
            assert_eq!(registry.entries.raw_table().buckets() <= 8, count <= 7);
            for sparse in [false, true] {
                if sparse {
                    registry.entries.try_reserve(128).unwrap();
                    assert!(registry.entries.raw_table().buckets() > 8);
                }
                let before = ctx.jit().0.borrow().metadata.0.current();
                for &prototype in &sources {
                    let expected = registry
                        .entries
                        .get(&(ottavino_gc_arena::Gc::as_ptr(prototype) as usize))
                        .and_then(|entry| {
                            entry.prototype.upgrade(&ctx).and_then(|live| {
                                ottavino_gc_arena::Gc::ptr_eq(live, prototype).then_some(entry.id)
                            })
                        });
                    assert_eq!(registry.identity(ctx, prototype), expected);
                }
                assert_eq!(ctx.jit().0.borrow().metadata.0.current(), before);
            }
        }
    });
}

#[test]
fn known_prototypes_match_id_resolution_across_growth_and_reset() {
    Lua::empty().enter(|ctx| {
        let mut sources = Vec::new();
        let mut ids = vec![0, u64::MAX];
        for _ in 0..32 {
            let closure = Closure::load(ctx, None, b"return 7").unwrap();
            let prototype = closure.prototype();
            ids.push(
                ctx.jit_registry()
                    .borrow()
                    .identity(ctx, prototype)
                    .unwrap(),
            );
            sources.push(prototype);
        }
        for phase in 0..3 {
            let registry = ctx.jit_registry().borrow();
            let before = ctx.jit().0.borrow().metadata.0.current();
            for &id in &ids {
                for &prototype in &sources {
                    let resolved = registry
                        .resolve(ctx, id)
                        .is_some_and(|found| ottavino_gc_arena::Gc::ptr_eq(found, prototype));
                    assert_eq!(registry.identity(ctx, prototype) == Some(id), resolved);
                }
            }
            assert_eq!(ctx.jit().0.borrow().metadata.0.current(), before);
            drop(registry);
            if phase == 0 {
                ctx.jit_registry().borrow_mut(&ctx).reset(ctx);
            } else if phase == 1 {
                let mut registry = ctx.jit_registry().borrow_mut(&ctx);
                for &prototype in sources.iter().step_by(2) {
                    registry.register(ctx, prototype);
                    let id = registry.identity(ctx, prototype).unwrap();
                    assert!(!ids.contains(&id));
                    ids.push(id);
                }
            }
        }
    });
}

#[path = "identity_memo.rs"]
mod identity_memo;

#[test]
fn identities_survive_growth_collection_reset_and_reregistration() {
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    let mut roots = Vec::new();
    for round in 0..48 {
        let root = lua.enter(|ctx| {
            let closure =
                Closure::load(ctx, None, b"local function f() return 7 end return f()").unwrap();
            let registry = ctx.jit_registry().borrow();
            let id = registry.identity(ctx, closure.prototype()).unwrap();
            let child = closure.prototype().prototypes[0];
            let child_id = registry.identity(ctx, child).unwrap();
            assert_ne!(id, child_id);
            for &(ref root, expected, expected_child) in &roots {
                let old: Closure<'_> = ctx.fetch(root);
                assert_eq!(registry.identity(ctx, old.prototype()), Some(expected));
                assert_eq!(
                    registry.identity(ctx, old.prototype().prototypes[0]),
                    Some(expected_child)
                );
                assert!(ottavino_gc_arena::Gc::ptr_eq(
                    registry.resolve(ctx, expected).unwrap(),
                    old.prototype()
                ));
            }
            (ctx.stash(closure), id, child_id)
        });
        roots.push(root);
        if round % 8 == 0 {
            lua.gc_collect();
        }
    }
    assert_eq!(lua.jit_stats().registered_prototypes, 96);
    let retired: Vec<_> = roots.iter().skip(2).map(|r| (r.1, r.2)).collect();
    roots.truncate(2);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().registered_prototypes, 4);
    lua.enter(|ctx| {
        let mut registry = ctx.jit_registry().borrow_mut(&ctx);
        for &(id, child) in &retired {
            assert!(registry.resolve(ctx, id).is_none());
            assert!(registry.resolve(ctx, child).is_none());
        }
        for _ in 0..8 {
            for &(ref root, id, child) in &roots {
                let closure: Closure<'_> = ctx.fetch(root);
                assert_eq!(registry.identity(ctx, closure.prototype()), Some(id));
                assert_eq!(
                    registry.identity(ctx, closure.prototype().prototypes[0]),
                    Some(child)
                );
            }
        }
        registry.reset(ctx);
        for &(ref root, id, child) in &roots {
            let closure: Closure<'_> = ctx.fetch(root);
            assert!(registry.identity(ctx, closure.prototype()).is_none());
            assert!(registry.resolve(ctx, id).is_none());
            assert!(registry.resolve(ctx, child).is_none());
            registry.register(ctx, closure.prototype());
            let replacement = registry.identity(ctx, closure.prototype()).unwrap();
            assert!(replacement > id);
            assert_ne!(replacement, child);
            registry.register(ctx, closure.prototype());
            assert_eq!(
                registry.identity(ctx, closure.prototype()),
                Some(replacement)
            );
        }
    });
    assert_eq!(lua.jit_stats().registered_prototypes, 4);
    drop(roots);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().registered_prototypes, 0);
    assert_eq!(lua.jit_stats().metadata_bytes, 0);
}

#[test]
fn identity_misses_and_generation_refusal_preserve_live_entries() {
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let first = Closure::load(ctx, None, b"return 7").unwrap();
        let second = Closure::load(ctx, None, b"return 11").unwrap();
        let (first_id, second_id) = {
            let registry = ctx.jit_registry().borrow();
            (
                registry.identity(ctx, first.prototype()).unwrap(),
                registry.identity(ctx, second.prototype()).unwrap(),
            )
        };
        ctx.jit().0.borrow_mut().next_id = u64::MAX;
        let refused = Closure::load(ctx, None, b"return 13").unwrap();
        let mut registry = ctx.jit_registry().borrow_mut(&ctx);
        for _ in 0..16 {
            assert_eq!(registry.identity(ctx, first.prototype()), Some(first_id));
            assert!(registry.identity(ctx, refused.prototype()).is_none());
            assert_eq!(registry.identity(ctx, second.prototype()), Some(second_id));
            registry.register(ctx, refused.prototype());
            assert!(registry.identity(ctx, refused.prototype()).is_none());
        }
        assert_eq!(registry.entries.len(), 2);
        registry.reset(ctx);
        registry.register(ctx, first.prototype());
        assert!(registry.identity(ctx, first.prototype()).is_none());
        assert!(registry.identity(ctx, second.prototype()).is_none());
        assert!(registry.entries.is_empty());
    });
}
