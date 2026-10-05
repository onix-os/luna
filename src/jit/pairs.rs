use allocator_api2::vec::Vec;

use super::{
    canonical::Program,
    ir::Snapshot,
    metadata_map,
    owner::Shared,
    resources::{BudgetAllocator, Compactor},
    JitConfig, JitError, JitMode, MetadataMap, Runtime,
};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    pub caller: u64,
    pub callee: u64,
    pub pc: usize,
}

#[derive(Default)]
struct Entry {
    hotness: u32,
    attempts: u32,
    queued: bool,
    program: Option<Shared<Program>>,
    last_used: u64,
}

pub(super) struct State {
    executions: (usize, usize),
    entries: MetadataMap<Key, Entry>,
    queue: Vec<Key, BudgetAllocator>,
    entry_compactor: Compactor,
    queue_compactor: Compactor,
    allocator: BudgetAllocator,
}

impl State {
    fn new(allocator: BudgetAllocator) -> Self {
        Self {
            executions: (0, 0),
            entries: metadata_map(allocator.clone()),
            queue: Vec::new_in(allocator.clone()),
            entry_compactor: Compactor::default(),
            queue_compactor: Compactor::default(),
            allocator,
        }
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn clear(&mut self) {
        *self = Self::new(self.allocator.clone());
    }

    pub fn victim(&self, exclude: Option<Key>) -> Option<(u64, Key)> {
        self.entries
            .iter()
            .filter_map(|(key, entry)| {
                (Some(*key) != exclude
                    && entry
                        .program
                        .as_ref()
                        .is_some_and(|program| Shared::strong_count(program) == 1))
                .then_some((entry.last_used, *key))
            })
            .min()
    }

    pub fn evict(&mut self, key: Key) {
        let entry = self.entries.get_mut(&key).unwrap();
        entry.program = None;
        entry.hotness = 0;
    }

    pub fn configure(&mut self, config: &JitConfig, available: usize) {
        let entries = &mut self.entries;
        let mut retained = 0;
        self.queue.retain(|key| {
            let entry = entries.get_mut(key).unwrap();
            let keep = retained < available && entry.attempts < config.max_compile_attempts;
            retained += usize::from(keep);
            entry.queued = keep;
            keep
        });
        if self.queue.is_empty() {
            self.queue = Vec::new_in(self.allocator.clone());
        }
    }

    pub fn retire(&mut self, id: u64) {
        self.queue
            .retain(|key| key.caller != id && key.callee != id);
        self.entries
            .retain(|key, _| key.caller != id && key.callee != id);
        if self.queue.is_empty() {
            self.queue = Vec::new_in(self.allocator.clone());
        }
        if self.entries.is_empty() {
            self.entries = metadata_map(self.allocator.clone());
        }
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
        if available
            && entry.program.is_none()
            && !entry.queued
            && entry.attempts < config.max_compile_attempts
            && entry.hotness >= config.hot_threshold
        {
            if self.queue.try_reserve(1).is_err() {
                return false;
            }
            self.queue.push(key);
            entry.queued = true;
            return true;
        }
        false
    }

    fn next(&mut self) -> Option<Key> {
        if self.queue.is_empty() {
            return None;
        }
        let key = self.queue.remove(0);
        let entry = self.entries.get_mut(&key).unwrap();
        entry.queued = false;
        entry.attempts = entry.attempts.saturating_add(1);
        Some(key)
    }

    pub fn needs_compaction(&mut self) -> bool {
        self.entry_compactor
            .needed(self.entries.len(), self.entries.capacity())
            | self
                .queue_compactor
                .needed(self.queue.len(), self.queue.capacity())
    }

    pub fn compact(&mut self) -> [super::resources::Compaction; 2] {
        [
            self.entry_compactor.map(&mut self.entries),
            self.queue_compactor.vector(&mut self.queue),
        ]
    }
}

impl Runtime {
    pub(crate) fn record_pair_execution(&self, calls: usize, returns: usize) {
        if let Some(pairs) = &mut self.0.borrow_mut().pairs {
            pairs.executions.0 = pairs.executions.0.saturating_add(calls);
            pairs.executions.1 = pairs.executions.1.saturating_add(returns);
        }
    }

    pub(crate) fn test_pair_executions(&self) -> (usize, usize) {
        self.0
            .borrow()
            .pairs
            .as_ref()
            .map_or((0, 0), |pairs| pairs.executions)
    }

    pub(crate) fn prepare_call_at<'gc>(
        &self,
        ctx: crate::Context<'gc>,
        caller: crate::Closure<'gc>,
        registers: &crate::thread::LuaRegisters<'gc, '_>,
    ) -> Option<super::PreparedPair> {
        let prototype = caller.prototype();
        let crate::opcode::Operation::Call {
            func,
            args,
            returns,
        } = prototype.opcodes.get(*registers.pc)?.decode()
        else {
            return None;
        };
        if args.to_constant().is_none() || returns.to_constant() != Some(0) {
            return None;
        }
        let crate::Value::Function(crate::Function::Closure(callee)) =
            registers.stack_frame.get(usize::from(func.0)).copied()?
        else {
            return None;
        };
        let registry = ctx.jit_registry().borrow();
        let key = Key {
            caller: registry.identity(ctx, prototype)?,
            callee: registry.identity(ctx, callee.prototype())?,
            pc: *registers.pc,
        };
        self.pair_lease(key)
            .map(|program| super::PreparedPair { program })
    }

    pub(crate) fn call_pairs_enabled(&self) -> bool {
        let manager = self.0.borrow();
        manager.config.mode == JitMode::Auto && manager.pairs.is_some()
    }

    pub(crate) fn observe_call<'gc>(
        &self,
        ctx: crate::Context<'gc>,
        caller: crate::Closure<'gc>,
        registers: &crate::thread::LuaRegisters<'gc, '_>,
        function: crate::types::RegisterIndex,
        arguments: crate::types::VarCount,
        returns: crate::types::VarCount,
    ) {
        if arguments.to_constant().is_none() || returns.to_constant() != Some(0) {
            return;
        }
        let Some(crate::Value::Function(crate::Function::Closure(callee))) =
            registers.stack_frame.get(usize::from(function.0)).copied()
        else {
            return;
        };
        if callee.prototype().opcodes.len() != 4 {
            return;
        }
        let registry = ctx.jit_registry().borrow();
        let Some(caller) = registry.identity(ctx, caller.prototype()) else {
            return;
        };
        let Some(callee) = registry.identity(ctx, callee.prototype()) else {
            return;
        };
        let Some(pc) = registers.pc.checked_sub(1) else {
            return;
        };
        self.observe_pair(Key { caller, callee, pc });
    }

    pub(crate) fn test_call_pairs(&self, enabled: bool) {
        let mut manager = self.0.borrow_mut();
        manager.pairs = enabled.then(|| State::new(manager.metadata.clone()));
        manager.stats.queued_requests = manager.queued_count();
    }

    pub(crate) fn observe_pair(&self, key: Key) {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto
            || !manager.tracked.contains_key(&key.caller)
            || !manager.tracked.contains_key(&key.callee)
        {
            return;
        }
        let available = manager.queued_count() < manager.config.max_queue_entries;
        let config = manager.config.clone();
        if manager
            .pairs
            .as_mut()
            .is_some_and(|pairs| pairs.observe(key, &config, available))
        {
            manager.stats.compilation_requests =
                manager.stats.compilation_requests.saturating_add(1);
        }
        manager.stats.queued_requests = manager.queued_count();
    }

    pub(crate) fn next_pair_request(&self) -> Option<Key> {
        let mut manager = self.0.borrow_mut();
        let key = manager.pairs.as_mut()?.next();
        manager.stats.queued_requests = manager.queued_count();
        key
    }

    pub(super) fn pair_lease(&self, key: Key) -> Option<Shared<Program>> {
        let mut manager = self.0.borrow_mut();
        if manager.config.mode != JitMode::Auto {
            return None;
        }
        manager.stats.code_lookups = manager.stats.code_lookups.saturating_add(1);
        let last_used = manager.clock.saturating_add(1);
        let program = manager.pairs.as_mut()?.entries.get_mut(&key)?;
        let lease = program.program.as_ref()?.clone();
        program.last_used = last_used;
        manager.clock = last_used;
        manager.stats.code_leases = manager.stats.code_leases.saturating_add(1);
        Some(lease)
    }

    pub(crate) fn compile_pair(
        &self,
        key: Key,
        caller: Snapshot,
        callee: Snapshot,
    ) -> Result<(), JitError> {
        let hook = self.0.borrow_mut().before_compile.take();
        if let Some(hook) = hook {
            hook();
        }
        let (memory, limit, metadata, limits, failure) = {
            let manager = self.0.borrow();
            (
                manager.memory.clone(),
                manager.config.max_code_bytes,
                manager.metadata.clone(),
                super::work::Limits::from(&manager.config),
                manager.memory_failure,
            )
        };
        let compile = || {
            Program::new(
                key,
                &caller,
                &callee,
                memory.clone(),
                limit,
                metadata.clone(),
                limits,
                failure,
            )
        };
        let mut result = compile();
        if matches!(&result, Err(JitError::ResourceLimit("native mappings"))) {
            let mut manager = self.0.borrow_mut();
            let eligible = manager.config.mode == JitMode::Auto
                && manager.pairs.as_ref().is_some_and(|pairs| {
                    pairs
                        .entries
                        .get(&key)
                        .is_some_and(|entry| entry.attempts < manager.config.max_compile_attempts)
                });
            if eligible && manager.evict_cached(super::CacheKey::Pair(key)) {
                manager
                    .pairs
                    .as_mut()
                    .unwrap()
                    .entries
                    .get_mut(&key)
                    .unwrap()
                    .attempts += 1;
                manager.stats.compilation_failures =
                    manager.stats.compilation_failures.saturating_add(1);
                drop(manager);
                result = compile();
            }
        }
        drop(caller);
        drop(callee);
        let mut manager = self.0.borrow_mut();
        let result = result.and_then(|program| {
            if manager.config.mode != JitMode::Auto
                || !manager.tracked.contains_key(&key.caller)
                || !manager.tracked.contains_key(&key.callee)
                || !manager.pairs.as_ref().is_some_and(|pairs| {
                    pairs
                        .entries
                        .get(&key)
                        .is_some_and(|entry| entry.attempts <= manager.config.max_compile_attempts)
                })
            {
                return Ok(());
            }
            match failure {
                super::backend::Failure::RefuseOwnerStorage => {
                    manager.metadata.0.set_limit(manager.metadata.0.current())
                }
                super::backend::Failure::RefuseOwnerAllocation => manager.metadata.0.fail_after(0),
                _ => {}
            }
            let program = Shared::try_new(program, manager.metadata.clone())
                .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
            let last_used = manager.clock.saturating_add(1);
            let entry = manager
                .pairs
                .as_mut()
                .unwrap()
                .entries
                .get_mut(&key)
                .unwrap();
            entry.program = Some(program);
            entry.last_used = last_used;
            manager.clock = last_used;
            manager.stats.installed_regions = manager.stats.installed_regions.saturating_add(1);
            Ok(())
        });
        if result.is_err() {
            manager.stats.compilation_failures =
                manager.stats.compilation_failures.saturating_add(1);
        }
        result
    }
}

#[cfg(test)]
mod tests;
