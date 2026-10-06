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
}

pub(in crate::jit) struct Cache {
    entries: MetadataMap<Key, Entry>,
    allocator: BudgetAllocator,
    compactor: Compactor,
}

impl Cache {
    pub(in crate::jit) fn new(allocator: BudgetAllocator) -> Self {
        Self {
            entries: metadata_map(allocator.clone()),
            allocator,
            compactor: Compactor::default(),
        }
    }

    fn insert(&mut self, region: Region, last_used: u64) -> Result<(), JitError> {
        let key = region.pair.program.key();
        let region = Shared::try_new(region, self.allocator.clone())
            .map_err(|_| JitError::ResourceLimit("region owner metadata"))?;
        self.entries
            .try_reserve(1)
            .map_err(|_| JitError::ResourceLimit("region cache metadata"))?;
        self.entries.insert(key, Entry { region, last_used });
        Ok(())
    }

    fn get(&self, key: Key) -> Option<&Shared<Region>> {
        self.entries.get(&key).map(|entry| &entry.region)
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
        self.entries.remove(&key);
        self.release_empty();
    }

    pub(in crate::jit) fn retire(&mut self, id: u64) {
        self.entries
            .retain(|key, _| key.caller != id && key.callee != id);
        self.release_empty();
    }

    pub(in crate::jit) fn retire_caller(&mut self, id: u64) {
        self.entries.retain(|key, _| key.caller != id);
        self.release_empty();
    }

    fn release_empty(&mut self) {
        if self.entries.is_empty() {
            self.entries = metadata_map(self.allocator.clone());
            self.compactor = Compactor::default();
        }
    }

    pub(in crate::jit) fn needs_compaction(&mut self) -> bool {
        self.compactor
            .needed(self.entries.len(), self.entries.capacity())
    }

    pub(in crate::jit) fn compact(&mut self) -> Compaction {
        self.compactor.map(&mut self.entries)
    }
}

impl Runtime {
    pub(in crate::jit) fn install_region(&self, region: Region) -> Result<(), JitError> {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto
            || manager.config != region.config
            || !region.installed(&manager)
        {
            return Err(JitError::Compilation("region dependency admission".into()));
        }
        let last_used = manager.clock.saturating_add(1);
        manager
            .pairs
            .as_mut()
            .unwrap()
            .regions
            .insert(region, last_used)?;
        manager.clock = last_used;
        Ok(())
    }

    pub(in crate::jit) fn region_lease(&self, key: Key) -> Option<Shared<Region>> {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto {
            return None;
        }
        manager.stats.code_lookups = manager.stats.code_lookups.saturating_add(1);
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
