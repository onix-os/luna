use allocator_api2::vec::Vec;

use crate::jit::{
    metadata_map,
    pairs::Key,
    resources::{BudgetAllocator, Compaction, Compactor},
    CacheKey, JitConfig, JitMode, MetadataMap, PreparedPair, Runtime,
};

#[derive(Default)]
struct Entry {
    hotness: u32,
    attempts: u32,
    queued: bool,
}

pub(in crate::jit) struct Schedule {
    entries: MetadataMap<Key, Entry>,
    queue: Vec<Key, BudgetAllocator>,
    allocator: BudgetAllocator,
    entries_compactor: Compactor,
    queue_compactor: Compactor,
}

impl Schedule {
    pub(in crate::jit) fn new(allocator: BudgetAllocator) -> Self {
        Self {
            entries: metadata_map(allocator.clone()),
            queue: Vec::new_in(allocator.clone()),
            allocator,
            entries_compactor: Compactor::default(),
            queue_compactor: Compactor::default(),
        }
    }

    pub(in crate::jit) fn queued(&self) -> usize {
        self.queue.len()
    }

    fn observe(&mut self, key: Key, config: &JitConfig, available: bool) -> bool {
        if !self.entries.contains_key(&key) {
            if self.entries.try_reserve(1).is_err() {
                return false;
            }
            self.entries.insert(key, Entry::default());
        }
        let entry = self.entries.get_mut(&key).unwrap();
        entry.hotness = entry.hotness.saturating_add(1);
        if !available
            || entry.queued
            || entry.attempts >= config.max_compile_attempts
            || entry.hotness < config.hot_threshold
            || self.queue.try_reserve(1).is_err()
        {
            return false;
        }
        self.queue.push(key);
        entry.queued = true;
        true
    }

    fn next(&mut self) -> Option<Key> {
        if self.queue.is_empty() {
            return None;
        }
        let key = self.queue.remove(0);
        let entry = self.entries.get_mut(&key).unwrap();
        entry.queued = false;
        entry.attempts = entry.attempts.saturating_add(1);
        self.release_empty();
        Some(key)
    }

    pub(in crate::jit) fn configure(&mut self, config: &JitConfig, available: usize) {
        let mut retained = 0;
        self.queue.retain(|key| {
            let entry = self.entries.get_mut(key).unwrap();
            let keep = retained < available && entry.attempts < config.max_compile_attempts;
            entry.queued = keep;
            retained += usize::from(keep);
            keep
        });
        self.release_empty();
    }

    pub(in crate::jit) fn cancel(&mut self, key: Key) {
        self.queue.retain(|request| *request != key);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.queued = false;
            entry.hotness = 0;
        }
        self.release_empty();
    }

    pub(in crate::jit) fn cancel_caller(&mut self, caller: u64) {
        self.queue.retain(|key| key.caller != caller);
        for (key, entry) in &mut self.entries {
            if key.caller == caller {
                entry.queued = false;
                entry.hotness = 0;
            }
        }
        self.release_empty();
    }

    pub(in crate::jit) fn retire(&mut self, id: u64) {
        self.queue
            .retain(|key| key.caller != id && key.callee != id);
        self.entries
            .retain(|key, _| key.caller != id && key.callee != id);
        self.release_empty();
    }

    fn release_empty(&mut self) {
        if self.queue.is_empty() {
            self.queue = Vec::new_in(self.allocator.clone());
            self.queue_compactor = Compactor::default();
        }
        if self.entries.is_empty() {
            self.entries = metadata_map(self.allocator.clone());
            self.entries_compactor = Compactor::default();
        }
    }

    pub(in crate::jit) fn needs_compaction(&mut self) -> bool {
        self.entries_compactor
            .needed(self.entries.len(), self.entries.capacity())
            | self
                .queue_compactor
                .needed(self.queue.len(), self.queue.capacity())
    }

    pub(in crate::jit) fn compact(&mut self) -> [Compaction; 2] {
        [
            self.entries_compactor.map(&mut self.entries),
            self.queue_compactor.vector(&mut self.queue),
        ]
    }
}

impl Runtime {
    pub(crate) fn test_region_promotions(&self, enabled: bool) {
        let mut manager = self.0.borrow_mut();
        let allocator = manager.metadata.clone();
        if let Some(pairs) = &mut manager.pairs {
            pairs.promotions = enabled.then(|| Schedule::new(allocator));
        }
        manager.stats.queued_requests = manager.queued_count();
    }

    pub(crate) fn observe_region(&self, key: Key) {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto || !manager.code.contains_key(&key.caller) {
            return;
        }
        let available = manager.queued_count() < manager.config.max_queue_entries;
        let config = manager.config.clone();
        let Some(pairs) = &mut manager.pairs else {
            return;
        };
        if !pairs.has_key(key) || pairs.regions.contains(key) {
            return;
        }
        if pairs
            .promotions
            .as_mut()
            .is_some_and(|queue| queue.observe(key, &config, available))
        {
            manager.stats.compilation_requests =
                manager.stats.compilation_requests.saturating_add(1);
        }
        manager.stats.queued_requests = manager.queued_count();
    }

    pub(crate) fn next_region_request(&self) -> Option<Key> {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto {
            return None;
        }
        let key = manager.pairs.as_mut()?.promotions.as_mut()?.next();
        manager.stats.queued_requests = manager.queued_count();
        key
    }

    pub(crate) fn prepare_region<'gc>(
        &self,
        ctx: crate::Context<'gc>,
        key: Key,
    ) -> Option<Result<super::Request, crate::jit::JitError>> {
        let caller = ctx.jit_registry().borrow().resolve(ctx, key.caller)?;
        ctx.jit_registry().borrow().resolve(ctx, key.callee)?;
        let pair = PreparedPair {
            program: self.pair_lease(key)?,
        };
        Some(super::Request::prepare(ctx, caller, pair))
    }

    pub(crate) fn retry_region_request(&self, key: Key) -> bool {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto
            || !manager
                .pairs
                .as_ref()
                .and_then(|pairs| pairs.promotions.as_ref())
                .and_then(|queue| queue.entries.get(&key))
                .is_some_and(|entry| entry.attempts < manager.config.max_compile_attempts)
        {
            return false;
        }
        if !manager.evict_cached(CacheKey::Region(key)) {
            return false;
        }
        manager
            .pairs
            .as_mut()
            .unwrap()
            .promotions
            .as_mut()
            .unwrap()
            .entries
            .get_mut(&key)
            .unwrap()
            .attempts += 1;
        manager.stats.compilation_failures = manager.stats.compilation_failures.saturating_add(1);
        true
    }
}
