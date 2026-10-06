use super::Region;
use crate::jit::{
    metadata_map,
    owner::Shared,
    pairs::Key,
    resources::{BudgetAllocator, Compaction, Compactor},
    JitError, JitMode, MetadataMap, Runtime,
};

struct Entry {
    region: Shared<Region>,
    last_used: u64,
    previous: Option<Key>,
    next: Option<Key>,
}

pub(in crate::jit) struct Cache {
    entries: MetadataMap<Key, Entry>,
    callers: MetadataMap<u64, Key>,
    allocator: BudgetAllocator,
    compactor: Compactor,
    caller_compactor: Compactor,
}

impl Cache {
    pub(in crate::jit) fn new(allocator: BudgetAllocator) -> Self {
        Self {
            entries: metadata_map(allocator.clone()),
            callers: metadata_map(allocator.clone()),
            allocator,
            compactor: Compactor::default(),
            caller_compactor: Compactor::default(),
        }
    }

    fn insert(&mut self, region: Region, last_used: u64) -> Result<(), JitError> {
        let key = region.pair.program.key();
        let region = Shared::try_new(region, self.allocator.clone())
            .map_err(|_| JitError::ResourceLimit("region owner metadata"))?;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.region = region;
            entry.last_used = last_used;
            return Ok(());
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| JitError::ResourceLimit("region cache metadata"))?;
        if !self.callers.contains_key(&key.caller) && self.callers.try_reserve(1).is_err() {
            self.release_empty();
            return Err(JitError::ResourceLimit("region caller index metadata"));
        }
        let next = self.callers.insert(key.caller, key);
        if let Some(next) = next {
            self.entries.get_mut(&next).unwrap().previous = Some(key);
        }
        self.entries.insert(
            key,
            Entry {
                region,
                last_used,
                previous: None,
                next,
            },
        );
        Ok(())
    }

    fn caller(&self, caller: u64) -> Option<Key> {
        self.callers.get(&caller).copied()
    }

    fn get(&self, key: Key) -> Option<&Shared<Region>> {
        self.entries.get(&key).map(|entry| &entry.region)
    }

    pub(in crate::jit) fn contains(&self, key: Key) -> bool {
        self.entries.contains_key(&key)
    }

    fn lease(&mut self, key: Key, last_used: u64) -> Option<Shared<Region>> {
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = last_used;
        Some(entry.region.clone())
    }

    pub(in crate::jit) fn victim(&self, exclude: Option<Key>) -> Option<(u64, Key)> {
        self.entries
            .iter()
            .filter_map(|(key, entry)| {
                (Some(*key) != exclude && Shared::strong_count(&entry.region) == 1)
                    .then_some((entry.last_used, *key))
            })
            .min()
    }

    pub(in crate::jit) fn remove(&mut self, key: Key) {
        let Some(entry) = self.entries.remove(&key) else {
            return;
        };
        if let Some(previous) = entry.previous {
            self.entries.get_mut(&previous).unwrap().next = entry.next;
        } else if let Some(next) = entry.next {
            *self.callers.get_mut(&key.caller).unwrap() = next;
        } else {
            self.callers.remove(&key.caller);
        }
        if let Some(next) = entry.next {
            self.entries.get_mut(&next).unwrap().previous = entry.previous;
        }
        self.release_empty();
    }

    pub(in crate::jit) fn retire(&mut self, id: u64) {
        let entries = &mut self.entries;
        self.callers.retain(|caller, head| {
            let mut current = Some(*head);
            let mut first = None;
            let mut previous = None;
            while let Some(key) = current {
                let next = entries[&key].next;
                if *caller == id || key.callee == id {
                    entries.remove(&key);
                    if let Some(previous) = previous {
                        entries.get_mut(&previous).unwrap().next = next;
                    }
                    if let Some(next) = next {
                        entries.get_mut(&next).unwrap().previous = previous;
                    }
                } else {
                    first.get_or_insert(key);
                    previous = Some(key);
                }
                current = next;
            }
            if let Some(first) = first {
                *head = first;
                true
            } else {
                false
            }
        });
        self.release_empty();
    }

    pub(in crate::jit) fn retire_caller(&mut self, id: u64) {
        while let Some(key) = self.caller(id) {
            self.remove(key);
        }
    }

    fn release_empty(&mut self) {
        if self.entries.is_empty() {
            self.entries = metadata_map(self.allocator.clone());
            self.callers = metadata_map(self.allocator.clone());
            self.compactor = Compactor::default();
            self.caller_compactor = Compactor::default();
        }
    }

    pub(in crate::jit) fn needs_compaction(&mut self) -> bool {
        self.compactor
            .needed(self.entries.len(), self.entries.capacity())
            | self
                .caller_compactor
                .needed(self.callers.len(), self.callers.capacity())
    }

    pub(in crate::jit) fn compact(&mut self) -> [Compaction; 2] {
        [
            self.compactor.map(&mut self.entries),
            self.caller_compactor.map(&mut self.callers),
        ]
    }
}

impl Runtime {
    pub(crate) fn install_region(&self, region: Region) -> Result<(), JitError> {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto
            || manager.config != region.config
            || !region.installed(&manager)
        {
            return Err(JitError::Compilation("region dependency admission".into()));
        }
        let last_used = manager.clock.saturating_add(1);
        let key = region.pair.program.key();
        manager
            .pairs
            .as_mut()
            .unwrap()
            .regions
            .insert(region, last_used)?;
        manager.clock = last_used;
        manager.stats.installed_regions = manager.stats.installed_regions.saturating_add(1);
        if let Some(queue) = &mut manager.pairs.as_mut().unwrap().promotions {
            queue.cancel(key);
        }
        manager.stats.queued_requests = manager.queued_count();
        Ok(())
    }

    pub(in crate::jit) fn region_lease(&self, key: Key) -> Option<Shared<Region>> {
        self.select_region(|_| Some(key))
    }

    pub(in crate::jit) fn region_for<'gc>(
        &self,
        ctx: crate::Context<'gc>,
        caller: crate::Closure<'gc>,
    ) -> Option<Shared<Region>> {
        if !crate::jit::RuntimeOwner::ptr_eq(&self.0, &ctx.jit().0) || ctx.hook_enabled() {
            return None;
        }
        let source = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, caller.prototype())?;
        self.select_region(|cache| cache.caller(source))
    }

    fn select_region(&self, select: impl FnOnce(&Cache) -> Option<Key>) -> Option<Shared<Region>> {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto {
            return None;
        }
        manager.stats.code_lookups = manager.stats.code_lookups.saturating_add(1);
        let key = select(&manager.pairs.as_ref()?.regions)?;
        let region = manager.pairs.as_ref()?.regions.get(key)?;
        if !region.installed(&manager) {
            manager.pairs.as_mut().unwrap().regions.remove(key);
            return None;
        }
        let last_used = manager.clock.saturating_add(1);
        let region = manager
            .pairs
            .as_mut()
            .unwrap()
            .regions
            .lease(key, last_used)?;
        manager.clock = last_used;
        manager.stats.code_leases = manager.stats.code_leases.saturating_add(1);
        Some(region)
    }
}

#[cfg(test)]
#[path = "tests/index.rs"]
mod tests;
