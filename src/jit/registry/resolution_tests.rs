use super::*;
use crate::{Closure, Lua};

fn check_scan<'gc>(ctx: Context<'gc>, registry: &Registrations<'gc>, id: u64) {
    let expected = registry
        .entries
        .values()
        .find(|entry| entry.id == id)
        .and_then(|entry| entry.prototype.upgrade(&ctx))
        .map(Gc::as_ptr);
    assert_eq!(registry.resolve(ctx, id).map(Gc::as_ptr), expected);
}

#[test]
fn repeated_resolution_matches_scan_across_growth_and_sparse_storage() {
    Lua::empty().enter(|ctx| {
        let mut ids = vec![0, u64::MAX];
        for round in 0..96 {
            let closure = Closure::load(ctx, None, b"return 7").unwrap();
            let mut registry = ctx.jit_registry().borrow_mut(&ctx);
            ids.push(registry.identity(ctx, closure.prototype()).unwrap());
            if round % 11 == 0 {
                registry.entries.try_reserve(128).unwrap();
            }
            let before = ctx.jit().0.borrow().metadata.0.current();
            for _ in 0..3 {
                for &id in ids.iter().rev() {
                    check_scan(ctx, &registry, id);
                    check_scan(ctx, &registry, *ids.last().unwrap());
                }
            }
            assert_eq!(ctx.jit().0.borrow().metadata.0.current(), before);
        }
    });
}

#[test]
fn resolution_rejects_retired_generations_and_recovers_after_reset() {
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    let mut roots = Vec::new();
    for _ in 0..32 {
        roots.push(lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return 7").unwrap();
            let registry = ctx.jit_registry().borrow();
            let id = registry.identity(ctx, closure.prototype()).unwrap();
            check_scan(ctx, &registry, id);
            (ctx.stash(closure), id)
        }));
    }
    let old: Vec<_> = roots.iter().map(|(_, id)| *id).collect();
    roots.truncate(2);
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        let mut registry = ctx.jit_registry().borrow_mut(&ctx);
        for &id in &old {
            check_scan(ctx, &registry, id);
        }
        for &id in &old[2..] {
            assert!(registry.resolve(ctx, id).is_none());
        }
        registry.reset(ctx);
        for (root, _) in &roots {
            let closure: Closure<'_> = ctx.fetch(root);
            registry.register(ctx, closure.prototype());
            let fresh = registry.identity(ctx, closure.prototype()).unwrap();
            assert!(!old.contains(&fresh));
            for _ in 0..4 {
                check_scan(ctx, &registry, fresh);
                for &id in &old {
                    assert!(registry.resolve(ctx, id).is_none());
                }
            }
        }
    });
    drop(roots);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().registered_prototypes, 0);
    assert_eq!(lua.jit_stats().metadata_bytes, 0);
}

#[test]
fn resolution_observes_authoritative_entry_removal_and_replacement() {
    Lua::empty().enter(|ctx| {
        let first = Closure::load(ctx, None, b"return 7").unwrap();
        let second = Closure::load(ctx, None, b"return 11").unwrap();
        let mut registry = ctx.jit_registry().borrow_mut(&ctx);
        let address = Gc::as_ptr(first.prototype()) as usize;
        let id = registry.identity(ctx, first.prototype()).unwrap();
        check_scan(ctx, &registry, id);
        let original = registry.entries.remove(&address).unwrap();
        assert!(registry.resolve(ctx, id).is_none());
        registry.entries.insert(
            address,
            Registration {
                id: u64::MAX,
                prototype: Gc::downgrade(second.prototype()),
            },
        );
        assert!(registry.resolve(ctx, id).is_none());
        check_scan(ctx, &registry, u64::MAX);
        registry.entries.insert(address, original);
        check_scan(ctx, &registry, id);
        assert!(registry.resolve(ctx, u64::MAX).is_none());
    });
}
