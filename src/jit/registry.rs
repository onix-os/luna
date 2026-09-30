use allocator_api2::vec::Vec;
use ottavino_gc_arena::{lock::RefLock, Collect, Gc, GcWeak};

use super::{
    metadata_map,
    resources::{BudgetAllocator, Compactor},
    JitError, MetadataMap,
};
use crate::{Context, FunctionPrototype};

#[derive(Collect)]
#[collect(no_drop)]
struct Registration<'gc> {
    id: u64,
    prototype: GcWeak<'gc, FunctionPrototype<'gc>>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn completed_collection_retires_disabled_sources_without_compilation() {
        let mut lua = crate::Lua::empty();
        lua.set_gc_pacing(false);
        let live = lua.enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
            ctx.stash(closure)
        });
        lua.enter(|ctx| {
            crate::Closure::load(ctx, None, b"return 99").unwrap();
        });
        assert_eq!(lua.jit_stats().registered_prototypes, 2);
        lua.gc_collect();
        lua.gc_collect();
        assert_eq!(lua.jit_stats().registered_prototypes, 1);
        lua.enter(|ctx| {
            let prototype = ctx.fetch(&live).prototype();
            assert!(ctx
                .jit_registry()
                .borrow()
                .identity(ctx, prototype)
                .is_some());
        });
        drop(live);
        lua.gc_collect();
        lua.gc_collect();
        let stats = lua.jit_stats();
        assert_eq!(stats.registered_prototypes, 0);
        assert_eq!(stats.metadata_bytes, 0);
        assert_eq!(stats.compilation_requests, 0);
        assert_eq!(stats.native_entries, 0);
    }

    #[test]
    fn unchanged_dense_sweep_does_not_reborrow_manager() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let closure = crate::Closure::load(ctx, Some("dense-sweep"), b"return 42").unwrap();
            let mut registry = ctx.jit_registry().borrow_mut(&ctx);
            let identity = registry.identity(ctx, closure.prototype()).unwrap();
            let manager = ctx.jit().0.borrow_mut();
            registry.sweep(ctx, false);
            drop(manager);
            assert_eq!(registry.identity(ctx, closure.prototype()), Some(identity));
            assert_eq!(ctx.jit().0.borrow().stats.metadata_compaction_attempts, 0);
        });
    }
}

#[derive(Collect)]
#[collect(no_drop)]
pub(crate) struct Registrations<'gc> {
    entries: MetadataMap<usize, Registration<'gc>>,
    compactor: Compactor,
}

pub(crate) type Registry<'gc> = Gc<'gc, RefLock<Registrations<'gc>>>;

impl<'gc> Registrations<'gc> {
    pub fn new(allocator: BudgetAllocator) -> Self {
        Self {
            entries: metadata_map(allocator),
            compactor: Compactor::default(),
        }
    }

    pub fn reset(&mut self, ctx: Context<'gc>) {
        let allocator = ctx.jit().0.borrow().metadata.clone();
        self.entries = metadata_map(allocator);
        self.compactor = Compactor::default();
        ctx.jit().0.borrow_mut().clear_registrations();
    }

    pub fn register(&mut self, ctx: Context<'gc>, prototype: Gc<'gc, FunctionPrototype<'gc>>) {
        let address = Gc::as_ptr(prototype) as usize;
        if self.identity(ctx, prototype).is_some() {
            return;
        }
        if let Some(old) = self.entries.remove(&address) {
            ctx.jit().0.borrow_mut().retire(old.id);
        }
        let id = {
            let mut manager = ctx.jit().0.borrow_mut();
            if self.entries.try_reserve(1).is_err() || manager.tracked.try_reserve(1).is_err() {
                manager.stats.registration_refusals =
                    manager.stats.registration_refusals.saturating_add(1);
                return;
            }
            let Some(id) = manager.next_id.checked_add(1) else {
                return;
            };
            manager.next_id = id;
            manager.tracked.insert(id, super::Tracking::default());
            manager.stats.registered_prototypes = manager.tracked.len();
            id
        };
        self.entries.insert(
            address,
            Registration {
                id,
                prototype: Gc::downgrade(prototype),
            },
        );
        for child in prototype.prototypes.iter().copied() {
            self.register(ctx, child);
        }
    }

    pub fn identity(
        &self,
        ctx: Context<'gc>,
        prototype: Gc<'gc, FunctionPrototype<'gc>>,
    ) -> Option<u64> {
        let entry = self.entries.get(&(Gc::as_ptr(prototype) as usize))?;
        let live = entry.prototype.upgrade(&ctx)?;
        Gc::ptr_eq(live, prototype).then_some(entry.id)
    }

    pub fn resolve(&self, ctx: Context<'gc>, id: u64) -> Option<Gc<'gc, FunctionPrototype<'gc>>> {
        self.entries
            .values()
            .find(|entry| entry.id == id)?
            .prototype
            .upgrade(&ctx)
    }

    pub fn sweep(&mut self, ctx: Context<'gc>, maintain: bool) {
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            if entry.prototype.upgrade(&ctx).is_some() {
                true
            } else {
                ctx.jit().0.borrow_mut().retire(entry.id);
                false
            }
        });
        let retired = before != self.entries.len();
        let result = self.compactor.map(&mut self.entries);
        if !maintain && !retired && matches!(result, super::resources::Compaction::Unchanged) {
            return;
        }
        let mut manager = ctx.jit().0.borrow_mut();
        manager.record_compaction(result);
        if maintain || retired {
            manager.compact_metadata();
        }
    }

    pub fn ids(&self, ctx: Context<'gc>) -> Result<Vec<u64, BudgetAllocator>, JitError> {
        let mut ids = Vec::new_in(ctx.jit().0.borrow().metadata.clone());
        ids.try_reserve_exact(self.entries.len())
            .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
        ids.extend(self.entries.values().map(|entry| entry.id));
        Ok(ids)
    }
}
