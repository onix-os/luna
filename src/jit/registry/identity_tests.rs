use crate::{Closure, Lua};

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
