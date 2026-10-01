//! Optional native execution configuration and per-state diagnostics.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use ahash::RandomState;
use allocator_api2::vec::Vec;
use hashbrown::HashMap;
use ottavino_gc_arena::Collect;
use thiserror::Error;

mod abi;
mod access;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod backend;
mod exits;
mod flow;
#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod fuzz;
mod helpers;
pub(crate) mod ir;
#[cfg(test)]
mod model;
pub(crate) mod registry;
pub(crate) mod resources;
mod work;
use resources::{BudgetAllocator, Compaction, Compactor, Ledger};

pub(crate) type MetadataMap<K, V> = HashMap<K, V, RandomState, BudgetAllocator>;

pub(crate) fn metadata_map<K, V>(allocator: BudgetAllocator) -> MetadataMap<K, V> {
    HashMap::with_hasher_in(RandomState::new(), allocator)
}

/// Selects interpreted execution or hot-code promotion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitMode {
    #[default]
    Off,
    Auto,
}

/// Limits compilation admission and executable code ownership per state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitConfig {
    pub mode: JitMode,
    pub hot_threshold: u32,
    pub max_prototype_instructions: usize,
    pub max_ir_instructions: usize,
    pub max_ir_blocks: usize,
    pub max_queue_entries: usize,
    pub max_code_bytes: usize,
    pub max_snapshot_bytes: usize,
    pub max_metadata_bytes: usize,
    pub max_compile_attempts: u32,
}

impl Default for JitConfig {
    fn default() -> Self {
        Self {
            mode: JitMode::Off,
            hot_threshold: 64,
            max_prototype_instructions: 4096,
            max_ir_instructions: 1024 * 1024,
            max_ir_blocks: 65536,
            max_queue_entries: 16,
            max_code_bytes: 8 * 1024 * 1024,
            max_snapshot_bytes: 2 * 1024 * 1024,
            max_metadata_bytes: 2 * 1024 * 1024,
            max_compile_attempts: 2,
        }
    }
}

impl JitConfig {
    pub(crate) fn validate(&self) -> Result<(), JitError> {
        if self.hot_threshold == 0 {
            return Err(JitError::InvalidConfiguration(
                "hot_threshold must be positive",
            ));
        }
        if self.max_prototype_instructions == 0
            || self.max_ir_instructions == 0
            || self.max_ir_blocks == 0
            || self.max_queue_entries == 0
            || self.max_code_bytes == 0
            || self.max_snapshot_bytes == 0
            || self.max_metadata_bytes == 0
            || self.max_compile_attempts == 0
        {
            return Err(JitError::InvalidConfiguration(
                "resource limits must be positive",
            ));
        }
        Ok(())
    }
}

/// Native backend availability for the current build target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitCapabilities {
    pub supported_target: bool,
    pub architecture: &'static str,
    pub unavailable_reason: Option<&'static str>,
}

impl JitCapabilities {
    pub fn current() -> Self {
        let supported = cfg!(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ));
        Self {
            supported_target: supported,
            architecture: std::env::consts::ARCH,
            unavailable_reason: (!supported).then_some("native backend unavailable on this target"),
        }
    }
}

/// Monotonic execution counters and current JIT resource usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitStats {
    pub interpreted_slices: u64,
    pub interpreted_instructions: u64,
    pub native_entries: u64,
    pub code_lookups: u64,
    pub code_leases: u64,
    pub native_instructions: u64,
    pub guard_exits: u64,
    pub helper_calls: u64,
    pub helper_instructions: u64,
    pub helper_declines: u64,
    pub native_table_reads: u64,
    pub native_table_writes: u64,
    pub native_upvalue_reads: u64,
    pub native_upvalue_writes: u64,
    pub native_allocations: u64,
    pub hook_exits: u64,
    pub compilation_requests: u64,
    pub compilation_failures: u64,
    pub cache_evictions: u64,
    pub cache_eviction_refusals: u64,
    pub registered_prototypes: usize,
    pub installed_regions: u64,
    pub code_bytes: usize,
    pub snapshot_bytes: usize,
    pub snapshot_peak_bytes: usize,
    pub metadata_bytes: usize,
    pub metadata_peak_bytes: usize,
    pub metadata_allocation_refusals: usize,
    pub metadata_compaction_attempts: u64,
    pub metadata_compactions: u64,
    pub metadata_compaction_refusals: u64,
    pub metadata_compaction_bytes: u64,
    pub registration_refusals: u64,
    pub queued_requests: usize,
}

/// Configuration, admission, or native compilation failure.
#[derive(Debug, Error)]
pub enum JitError {
    #[error("invalid JIT configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("JIT unavailable: {0}")]
    Unavailable(&'static str),
    #[error("JIT resource limit: {0}")]
    ResourceLimit(&'static str),
    #[error("native compilation failed: {0}")]
    Compilation(String),
}

pub(crate) struct Manager {
    pub(crate) config: JitConfig,
    pub(crate) stats: JitStats,
    pub(crate) next_id: u64,
    pub(crate) tracked: MetadataMap<u64, Tracking>,
    queue: Vec<u64, BudgetAllocator>,
    tracked_compactor: Compactor,
    queue_compactor: Compactor,
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    code_compactor: Compactor,
    pub(crate) metadata: BudgetAllocator,
    pub(crate) snapshots: BudgetAllocator,
    pub(crate) memory: Arc<AtomicUsize>,
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    clock: u64,
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    code: MetadataMap<u64, CachedCode>,
    #[cfg(all(
        test,
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    memory_failure: backend::Failure,
}

impl Default for Manager {
    fn default() -> Self {
        let config = JitConfig::default();
        let metadata = BudgetAllocator(Ledger::new(config.max_metadata_bytes));
        let snapshots = BudgetAllocator(Ledger::new(config.max_snapshot_bytes));
        Self {
            config,
            stats: JitStats::default(),
            next_id: 0,
            tracked: metadata_map(metadata.clone()),
            queue: Vec::new_in(metadata.clone()),
            tracked_compactor: Compactor::default(),
            queue_compactor: Compactor::default(),
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            code_compactor: Compactor::default(),
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            code: metadata_map(metadata.clone()),
            #[cfg(all(
                test,
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            memory_failure: backend::Failure::None,
            metadata,
            snapshots,
            memory: Arc::new(AtomicUsize::new(0)),
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            clock: 0,
        }
    }
}

#[derive(Default)]
pub(crate) struct Tracking {
    hotness: u32,
    attempts: u32,
    queued: bool,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
struct CachedCode {
    code: Rc<backend::Code>,
    last_used: u64,
}

impl Manager {
    pub(crate) fn needs_compaction(&mut self) -> bool {
        let needed = self
            .tracked_compactor
            .needed(self.tracked.len(), self.tracked.capacity())
            | self
                .queue_compactor
                .needed(self.queue.len(), self.queue.capacity());
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            needed
                | self
                    .code_compactor
                    .needed(self.code.len(), self.code.capacity())
        }
        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            needed
        }
    }

    pub(crate) fn record_compaction(&mut self, result: Compaction) {
        match result {
            Compaction::Unchanged => return,
            Compaction::Compacted(bytes) => {
                self.stats.metadata_compactions = self.stats.metadata_compactions.saturating_add(1);
                self.stats.metadata_compaction_bytes = self
                    .stats
                    .metadata_compaction_bytes
                    .saturating_add(bytes as u64);
            }
            Compaction::Refused => {
                self.stats.metadata_compaction_refusals =
                    self.stats.metadata_compaction_refusals.saturating_add(1);
            }
        }
        self.stats.metadata_compaction_attempts =
            self.stats.metadata_compaction_attempts.saturating_add(1);
    }

    pub(crate) fn compact_metadata(&mut self) {
        let result = self.tracked_compactor.map(&mut self.tracked);
        self.record_compaction(result);
        let result = self.queue_compactor.vector(&mut self.queue);
        self.record_compaction(result);
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            let result = self.code_compactor.map(&mut self.code);
            self.record_compaction(result);
        }
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn evict_for_retry(&mut self, id: u64) -> bool {
        if self.config.mode != JitMode::Auto
            || !self
                .tracked
                .get(&id)
                .is_some_and(|tracking| tracking.attempts < self.config.max_compile_attempts)
        {
            return false;
        }
        let victim = self
            .code
            .iter()
            .filter(|(candidate, entry)| **candidate != id && Rc::strong_count(&entry.code) == 1)
            .min_by_key(|(candidate, entry)| (entry.last_used, **candidate))
            .map(|(candidate, _)| *candidate);
        let Some(victim) = victim else {
            self.stats.cache_eviction_refusals =
                self.stats.cache_eviction_refusals.saturating_add(1);
            return false;
        };
        self.code.remove(&victim);
        if let Some(tracking) = self.tracked.get_mut(&victim) {
            tracking.hotness = 0;
        }
        self.tracked.get_mut(&id).unwrap().attempts += 1;
        self.stats.cache_evictions = self.stats.cache_evictions.saturating_add(1);
        true
    }

    pub(crate) fn configure(&mut self, config: JitConfig) {
        if config.max_metadata_bytes < self.config.max_metadata_bytes {
            self.clear_registrations();
        }
        if config.mode == JitMode::Off
            || config.max_code_bytes < self.config.max_code_bytes
            || config.max_snapshot_bytes < self.config.max_snapshot_bytes
            || config.max_prototype_instructions < self.config.max_prototype_instructions
            || config.max_ir_instructions < self.config.max_ir_instructions
            || config.max_ir_blocks < self.config.max_ir_blocks
        {
            self.clear();
        } else {
            let tracked = &mut self.tracked;
            let mut retained = 0;
            self.queue.retain(|id| {
                let Some(tracking) = tracked.get_mut(id) else {
                    return false;
                };
                let keep = retained < config.max_queue_entries
                    && tracking.attempts < config.max_compile_attempts;
                tracking.queued = keep;
                retained += usize::from(keep);
                keep
            });
            self.stats.queued_requests = self.queue.len();
        }
        self.metadata.0.set_limit(config.max_metadata_bytes);
        self.snapshots.0.set_limit(config.max_snapshot_bytes);
        self.config = config;
    }

    pub(crate) fn retire(&mut self, id: u64) {
        self.tracked.remove(&id);
        self.stats.registered_prototypes = self.tracked.len();
        self.queue.retain(|request| *request != id);
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        self.code.remove(&id);
        self.stats.queued_requests = self.queue.len();
        if self.tracked.is_empty() {
            self.tracked = metadata_map(self.metadata.clone());
        }
        if self.queue.is_empty() {
            self.queue = Vec::new_in(self.metadata.clone());
        }
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if self.code.is_empty() {
            self.code = metadata_map(self.metadata.clone());
        }
    }

    pub(crate) fn enqueue(&mut self, id: u64, force: bool) {
        if self.config.mode == JitMode::Off {
            return;
        }
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if self.code.contains_key(&id) {
            return;
        }
        let Some(tracking) = self.tracked.get_mut(&id) else {
            return;
        };
        tracking.hotness = tracking.hotness.saturating_add(1);
        if (force || tracking.hotness >= self.config.hot_threshold)
            && !tracking.queued
            && tracking.attempts < self.config.max_compile_attempts
            && self.queue.len() < self.config.max_queue_entries
        {
            if self.queue.try_reserve(1).is_err() {
                return;
            }
            self.queue.push(id);
            tracking.queued = true;
            self.stats.compilation_requests = self.stats.compilation_requests.saturating_add(1);
            self.stats.queued_requests = self.queue.len();
        }
    }

    pub(crate) fn next_request(&mut self) -> Option<u64> {
        if self.queue.is_empty() {
            return None;
        }
        let id = self.queue.remove(0);
        if let Some(tracking) = self.tracked.get_mut(&id) {
            tracking.queued = false;
            tracking.attempts = tracking.attempts.saturating_add(1);
        }
        self.stats.queued_requests = self.queue.len();
        Some(id)
    }

    pub(crate) fn clear(&mut self) {
        self.queue_compactor = Compactor::default();
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            self.code_compactor = Compactor::default();
            self.code = metadata_map(self.metadata.clone());
        }
        self.queue = Vec::new_in(self.metadata.clone());
        for tracking in self.tracked.values_mut() {
            *tracking = Tracking::default();
        }
        self.stats.queued_requests = 0;
    }

    pub(crate) fn clear_registrations(&mut self) {
        self.clear();
        self.tracked = metadata_map(self.metadata.clone());
        self.tracked_compactor = Compactor::default();
        self.stats.registered_prototypes = 0;
    }
}

#[derive(Clone, Collect)]
#[collect(require_static)]
pub(crate) struct Runtime(pub(crate) Rc<RefCell<Manager>>);

pub(crate) struct Prepared {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    code: Rc<backend::Code>,
}

impl Runtime {
    pub(crate) fn new() -> Self {
        Self(Rc::new(RefCell::new(Manager::default())))
    }

    pub(crate) fn record_interpreter(&self, instructions: u32, executed: bool) {
        let mut manager = self.0.borrow_mut();
        manager.stats.interpreted_slices = manager
            .stats
            .interpreted_slices
            .saturating_add(u64::from(executed));
        manager.stats.interpreted_instructions = manager
            .stats
            .interpreted_instructions
            .saturating_add(u64::from(instructions));
    }

    pub(crate) fn active(&self) -> bool {
        self.0.borrow().config.mode == JitMode::Auto
    }

    pub(crate) fn usage(&self) -> usize {
        self.0.borrow().memory.load(Ordering::Relaxed)
    }

    pub(crate) fn compile(&self, id: u64, snapshot: ir::Snapshot) -> Result<(), JitError> {
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            let (memory, limit, metadata, work) = {
                let manager = self.0.borrow();
                (
                    manager.memory.clone(),
                    manager.config.max_code_bytes,
                    manager.metadata.clone(),
                    work::Limits::from(&manager.config),
                )
            };
            #[cfg(test)]
            let failure = self.0.borrow().memory_failure;
            let compile = || {
                backend::compile_in(
                    &snapshot,
                    memory.clone(),
                    limit,
                    metadata.clone(),
                    work,
                    #[cfg(test)]
                    failure,
                )
            };
            let mut result = compile();
            if matches!(&result, Err(JitError::ResourceLimit("native mappings"))) {
                let retry = {
                    let mut manager = self.0.borrow_mut();
                    manager.evict_for_retry(id)
                };
                if retry {
                    let mut manager = self.0.borrow_mut();
                    manager.stats.compilation_failures =
                        manager.stats.compilation_failures.saturating_add(1);
                    drop(manager);
                    result = compile();
                }
            }
            let mut manager = self.0.borrow_mut();
            match result {
                Ok(code) => {
                    if manager.tracked.contains_key(&id) && manager.config.mode == JitMode::Auto {
                        if manager.code.try_reserve(1).is_err() {
                            manager.stats.compilation_failures =
                                manager.stats.compilation_failures.saturating_add(1);
                            return Err(JitError::ResourceLimit("JIT metadata"));
                        }
                        manager.clock = manager.clock.saturating_add(1);
                        let last_used = manager.clock;
                        manager.code.insert(
                            id,
                            CachedCode {
                                code: Rc::new(code),
                                last_used,
                            },
                        );
                        manager.stats.installed_regions =
                            manager.stats.installed_regions.saturating_add(1);
                    }
                    Ok(())
                }
                Err(error) => {
                    manager.stats.compilation_failures =
                        manager.stats.compilation_failures.saturating_add(1);
                    Err(error)
                }
            }
        }
        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = (id, snapshot);
            Err(JitError::Unavailable(
                "native backend unavailable on this target",
            ))
        }
    }

    pub(crate) fn lookup(&self, id: u64) -> Option<Prepared> {
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            let mut manager = self.0.borrow_mut();
            if manager.config.mode == JitMode::Off {
                return None;
            }
            manager.stats.code_lookups = manager.stats.code_lookups.saturating_add(1);
            let last_used = manager.clock.saturating_add(1);
            let entry = manager.code.get_mut(&id)?;
            entry.last_used = last_used;
            let code = entry.code.clone();
            manager.clock = last_used;
            manager.stats.code_leases = manager.stats.code_leases.saturating_add(1);
            Some(Prepared { code })
        }
        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = id;
            None
        }
    }

    pub(crate) fn observe(&self, id: u64) {
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        self.0.borrow_mut().enqueue(id, false);
        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        let _ = id;
    }

    pub(crate) fn run<'gc>(
        &self,
        prepared: &Prepared,
        ctx: crate::Context<'gc>,
        closure: crate::Closure<'gc>,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        budget: u32,
    ) -> u32 {
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            let code = &prepared.code;
            if !code.entries.get(*registers.pc).copied().unwrap_or(false) {
                return 0;
            }
            if registers.stack_frame.len() < code.registers {
                return 0;
            }
            if code.registers > 256 {
                return 0;
            }
            match code.registers {
                0..=8 => self.invoke::<8>(code, ctx, closure, registers, budget),
                9..=16 => self.invoke::<16>(code, ctx, closure, registers, budget),
                17..=32 => self.invoke::<32>(code, ctx, closure, registers, budget),
                33..=64 => self.invoke::<64>(code, ctx, closure, registers, budget),
                65..=128 => self.invoke::<128>(code, ctx, closure, registers, budget),
                129..=256 => self.invoke::<256>(code, ctx, closure, registers, budget),
                _ => 0,
            }
        }
        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = (prepared, ctx, closure, registers, budget);
            0
        }
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[inline(never)]
    fn invoke<'gc, const CAPACITY: usize>(
        &self,
        code: &backend::Code,
        ctx: crate::Context<'gc>,
        closure: crate::Closure<'gc>,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        budget: u32,
    ) -> u32 {
        let mut scratch = [std::mem::MaybeUninit::<abi::Slot>::uninit(); CAPACITY];
        for (slot, value) in scratch[..code.registers]
            .iter_mut()
            .zip(registers.stack_frame.iter().copied())
        {
            slot.write(abi::Slot::from_value(value));
        }
        // Borrow the initialized prefix of scalar scratch storage.
        let slots = unsafe {
            std::slice::from_raw_parts_mut(scratch.as_mut_ptr().cast::<abi::Slot>(), code.registers)
        };
        let mut frame = helpers::Frame {
            ctx,
            closure,
            registers,
            count: helpers::Counts::default(),
            slot_count: code.registers,
            panic: None,
        };
        let mut host = abi::Host {
            data: (&mut frame as *mut helpers::Frame<'_, '_, '_>).cast(),
        };
        let exit = unsafe { code.invoke_host(slots, *frame.registers.pc, budget, &mut host) };
        for (slot, dest) in slots
            .iter()
            .copied()
            .zip(frame.registers.stack_frame.iter_mut())
        {
            slot.write_back(dest);
        }
        if frame.panic.is_none() {
            *frame.registers.pc = usize::try_from(exit.pc).expect("native PC exceeds host range");
        }
        let mut manager = self.0.borrow_mut();
        let counts = frame.count;
        manager.stats.helper_calls = manager.stats.helper_calls.saturating_add(counts.calls);
        manager.stats.helper_instructions = manager
            .stats
            .helper_instructions
            .saturating_add(counts.completed);
        manager.stats.helper_declines = manager
            .stats
            .helper_declines
            .saturating_add(counts.declined);
        manager.stats.native_table_reads = manager
            .stats
            .native_table_reads
            .saturating_add(counts.table_reads);
        manager.stats.native_table_writes = manager
            .stats
            .native_table_writes
            .saturating_add(counts.table_writes);
        manager.stats.native_upvalue_reads = manager
            .stats
            .native_upvalue_reads
            .saturating_add(counts.upvalue_reads);
        manager.stats.native_upvalue_writes = manager
            .stats
            .native_upvalue_writes
            .saturating_add(counts.upvalue_writes);
        manager.stats.native_allocations = manager
            .stats
            .native_allocations
            .saturating_add(counts.allocations);
        manager.stats.native_entries = manager.stats.native_entries.saturating_add(1);
        manager.stats.native_instructions = manager
            .stats
            .native_instructions
            .saturating_add(u64::from(exit.instructions));
        manager.stats.guard_exits = manager
            .stats
            .guard_exits
            .saturating_add(u64::from(exit.reason == exits::Kind::Guard as u32));
        if let Some(payload) = frame.panic.take() {
            drop(manager);
            std::panic::resume_unwind(payload);
        }
        exit.instructions
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn queued_manager() -> Manager {
        let mut manager = Manager::default();
        manager.configure(JitConfig {
            mode: JitMode::Auto,
            ..JitConfig::default()
        });
        for id in 1..=10 {
            manager.tracked.insert(id, Tracking::default());
            manager.enqueue(id, true);
        }
        manager
    }

    #[test]
    fn compaction_diagnostics_saturate_without_counting_deferred_passes() {
        let mut manager = Manager::default();
        manager.stats.metadata_compaction_attempts = u64::MAX;
        manager.stats.metadata_compactions = u64::MAX;
        manager.stats.metadata_compaction_bytes = u64::MAX;
        manager.stats.metadata_compaction_refusals = u64::MAX;
        manager.record_compaction(Compaction::Compacted(usize::MAX));
        manager.record_compaction(Compaction::Refused);
        manager.record_compaction(Compaction::Unchanged);
        assert_eq!(manager.stats.metadata_compaction_attempts, u64::MAX);
        assert_eq!(manager.stats.metadata_compactions, u64::MAX);
        assert_eq!(manager.stats.metadata_compaction_bytes, u64::MAX);
        assert_eq!(manager.stats.metadata_compaction_refusals, u64::MAX);
    }

    #[test]
    fn shrinking_queue_preserves_oldest_requests_and_releases_dropped_flags() {
        let mut manager = queued_manager();
        manager.configure(JitConfig {
            max_queue_entries: 4,
            ..manager.config.clone()
        });
        assert_eq!(
            manager.queue.iter().copied().collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(manager.stats.queued_requests, 4);
        for id in 1..=10 {
            assert_eq!(manager.tracked[&id].queued, id <= 4);
        }
        assert_eq!(manager.next_request(), Some(1));
        manager.enqueue(5, true);
        assert_eq!(
            manager.queue.iter().copied().collect::<Vec<_>>(),
            [2, 3, 4, 5]
        );
        assert!(manager.tracked[&5].queued);
    }

    #[test]
    fn lowering_attempt_limit_discards_now_exhausted_requests() {
        let mut manager = queued_manager();
        manager.tracked.get_mut(&1).unwrap().attempts = 1;
        manager.configure(JitConfig {
            max_compile_attempts: 1,
            ..manager.config.clone()
        });
        assert_eq!(manager.queue.first(), Some(&2));
        assert_eq!(manager.stats.queued_requests, 9);
        assert!(!manager.tracked[&1].queued);
        manager.enqueue(1, true);
        assert_eq!(manager.queue.len(), 9);
    }

    #[test]
    fn disabling_cancels_pending_work_and_resets_admission() {
        let mut manager = queued_manager();
        manager.tracked.get_mut(&1).unwrap().attempts = 1;
        manager.configure(JitConfig::default());
        assert!(manager.queue.is_empty());
        assert_eq!(manager.stats.queued_requests, 0);
        assert!(manager.tracked.values().all(|tracking| {
            !tracking.queued && tracking.attempts == 0 && tracking.hotness == 0
        }));
        manager.enqueue(1, true);
        assert!(manager.queue.is_empty());
    }
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod eviction_tests {
    use super::*;

    #[test]
    fn collected_source_retirement_preserves_code_until_active_lease_drops() {
        let mut lua = crate::Lua::empty();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        })
        .unwrap();
        let (closure, identity, runtime) = lua.enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
            let identity = ctx
                .jit_registry()
                .borrow()
                .identity(ctx, closure.prototype())
                .unwrap();
            (ctx.stash(closure), identity, ctx.jit().clone())
        });
        assert_eq!(lua.prepare_jit().unwrap(), 1);
        let lease = runtime.lookup(identity).unwrap();
        assert_executable(&lease);
        let bytes = lua.jit_stats().code_bytes;
        drop(closure);
        lua.gc_collect();
        lua.gc_collect();
        assert_eq!(lua.jit_stats().registered_prototypes, 0);
        assert!(runtime.lookup(identity).is_none());
        assert_eq!(lua.jit_stats().code_bytes, bytes);
        assert!(lua.jit_stats().metadata_bytes > 0);
        assert_executable(&lease);
        drop(lease);
        assert_eq!(lua.jit_stats().code_bytes, 0);
        assert_eq!(lua.jit_stats().metadata_bytes, 0);
    }

    fn snapshot() -> ir::Snapshot {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "eviction", b"return 42").unwrap();
            ir::Snapshot::new(&prototype, 4096, 65536).unwrap()
        })
    }

    fn request(runtime: &Runtime, id: u64) -> Result<(), JitError> {
        {
            let mut manager = runtime.0.borrow_mut();
            manager.tracked.entry(id).or_default();
            manager.enqueue(id, true);
            assert_eq!(manager.next_request(), Some(id));
        }
        runtime.compile(id, snapshot())
    }

    fn two_module_cache() -> (Runtime, usize) {
        let runtime = Runtime::new();
        runtime.0.borrow_mut().configure(JitConfig {
            mode: JitMode::Auto,
            ..JitConfig::default()
        });
        request(&runtime, 1).unwrap();
        let bytes = runtime.usage();
        assert!(bytes > 0);
        runtime.0.borrow_mut().config.max_code_bytes = 2 * bytes;
        request(&runtime, 2).unwrap();
        assert_eq!(runtime.usage(), 2 * bytes);
        (runtime, bytes)
    }

    fn assert_executable(prepared: &Prepared) {
        let mut slots = vec![
            abi::Slot {
                tag: abi::NIL,
                bits: 0
            };
            prepared.code.registers
        ];
        let exit = prepared.code.invoke(&mut slots, 0, 64);
        assert_eq!((exit.pc, exit.instructions, exit.reason), (1, 1, 0));
        assert_eq!((slots[0].tag, slots[0].bits), (abi::INTEGER, 42));
    }

    #[test]
    fn compaction_refusal_preserves_cache_leases_queue_and_retry_state() {
        let (runtime, _) = two_module_cache();
        let lease = runtime.lookup(1).unwrap();
        let before = {
            let mut manager = runtime.0.borrow_mut();
            manager.tracked.try_reserve(128).unwrap();
            manager.code.try_reserve(128).unwrap();
            manager.queue.try_reserve_exact(128).unwrap();
            manager.tracked.insert(
                3,
                Tracking {
                    hotness: 7,
                    attempts: 1,
                    queued: true,
                },
            );
            manager.queue.push(3);
            manager.stats.queued_requests = 1;
            let before = manager.metadata.0.current();
            manager.metadata.0.set_limit(before);
            before
        };
        {
            let mut manager = runtime.0.borrow_mut();
            let capacity = (
                manager.tracked.capacity(),
                manager.code.capacity(),
                manager.queue.capacity(),
            );
            let owners = Rc::strong_count(&lease.code);
            let recency = manager.code[&1].last_used;
            manager.compact_metadata();
            assert_eq!(manager.stats.metadata_compaction_attempts, 3);
            assert_eq!(manager.stats.metadata_compaction_refusals, 3);
            assert_eq!(manager.stats.metadata_compactions, 0);
            assert_eq!(manager.stats.metadata_compaction_bytes, 0);
            assert_eq!(manager.metadata.0.current(), before);
            assert_eq!(
                (
                    manager.tracked.capacity(),
                    manager.code.capacity(),
                    manager.queue.capacity()
                ),
                capacity
            );
            assert_eq!(Rc::strong_count(&lease.code), owners);
            assert_eq!(manager.code[&1].last_used, recency);
            let pending = &manager.tracked[&3];
            assert_eq!(
                (pending.hotness, pending.attempts, pending.queued),
                (7, 1, true)
            );
            assert_eq!(manager.queue.as_slice(), &[3]);
            manager
                .metadata
                .0
                .set_limit(JitConfig::default().max_metadata_bytes);
            for _ in 0..8 {
                manager.compact_metadata();
            }
            assert_eq!(manager.stats.metadata_compaction_attempts, 3);
            manager.compact_metadata();
            assert_eq!(manager.stats.metadata_compaction_attempts, 6);
            assert_eq!(manager.stats.metadata_compactions, 3);
            assert_eq!(manager.stats.metadata_compaction_refusals, 3);
            assert!(manager.metadata.0.current() < before);
        }
        assert_executable(&lease);
        assert_executable(&runtime.lookup(2).unwrap());
        let mut manager = runtime.0.borrow_mut();
        assert_eq!(manager.next_request(), Some(3));
        assert_eq!(manager.tracked[&3].attempts, 2);
        assert!(!manager.tracked[&3].queued);
    }

    #[test]
    fn compaction_preserves_pinned_code_recency_and_admission_state() {
        let (runtime, bytes) = two_module_cache();
        let lease = runtime.lookup(1).unwrap();
        let (before, owner_count, recency) = {
            let mut manager = runtime.0.borrow_mut();
            manager.tracked.try_reserve(128).unwrap();
            manager.code.try_reserve(128).unwrap();
            manager.queue.try_reserve_exact(128).unwrap();
            manager.tracked.insert(
                3,
                Tracking {
                    hotness: 41,
                    attempts: 1,
                    queued: true,
                },
            );
            manager.queue.push(3);
            manager.stats.queued_requests = 1;
            (
                manager.metadata.0.current(),
                Rc::strong_count(&lease.code),
                manager.code[&1].last_used,
            )
        };
        {
            let mut manager = runtime.0.borrow_mut();
            let clock = manager.clock;
            assert!(manager.needs_compaction());
            manager.compact_metadata();
            assert!(manager.metadata.0.current() < before);
            assert_eq!(manager.stats.metadata_compactions, 3);
            assert_eq!(manager.stats.metadata_compaction_attempts, 3);
            assert_eq!(manager.stats.metadata_compaction_refusals, 0);
            assert_eq!(
                manager.stats.metadata_compaction_bytes,
                (before - manager.metadata.0.current()) as u64
            );
            assert_eq!(manager.code[&1].last_used, recency);
            assert_eq!(manager.clock, clock);
            assert!(Rc::ptr_eq(&manager.code[&1].code, &lease.code));
            assert_eq!(Rc::strong_count(&lease.code), owner_count);
            let pending = &manager.tracked[&3];
            assert_eq!(
                (pending.hotness, pending.attempts, pending.queued),
                (41, 1, true)
            );
            assert_eq!(manager.queue.as_slice(), &[3]);
            assert_eq!(manager.stats.queued_requests, 1);
            assert!(!manager.needs_compaction());
        }
        assert_eq!(runtime.usage(), 2 * bytes);
        assert_executable(&lease);
        assert_executable(&runtime.lookup(2).unwrap());
        let metadata = runtime.0.borrow().metadata.0.clone();
        runtime.0.borrow_mut().clear_registrations();
        assert_eq!(runtime.usage(), bytes);
        assert_executable(&lease);
        drop(lease);
        assert_eq!(runtime.usage(), 0);
        assert_eq!(metadata.current(), 0);
    }

    #[test]
    fn pressure_evicts_lru_and_charges_retry_without_resetting_victim_attempts() {
        let (runtime, bytes) = two_module_cache();
        drop(runtime.lookup(1).unwrap());
        request(&runtime, 3).unwrap();
        assert_eq!(runtime.usage(), 2 * bytes);
        {
            let manager = runtime.0.borrow();
            assert!(manager.code.contains_key(&1));
            assert!(!manager.code.contains_key(&2));
            assert!(manager.code.contains_key(&3));
            assert_eq!(manager.tracked[&2].attempts, 1);
            assert_eq!(manager.tracked[&2].hotness, 0);
            assert_eq!(manager.tracked[&3].attempts, 2);
            assert_eq!(manager.stats.cache_evictions, 1);
            assert_eq!(manager.stats.compilation_failures, 1);
        }
        assert_executable(&runtime.lookup(3).unwrap());
        assert!(matches!(
            request(&runtime, 2),
            Err(JitError::ResourceLimit("native mappings"))
        ));
        let mut manager = runtime.0.borrow_mut();
        assert_eq!(manager.tracked[&2].attempts, 2);
        assert_eq!(manager.stats.cache_evictions, 1);
        assert_eq!(manager.stats.compilation_failures, 2);
        manager.enqueue(2, true);
        assert!(manager.queue.is_empty());
        manager.clear();
        assert!(manager
            .tracked
            .values()
            .all(|tracking| tracking.attempts == 0));
        drop(manager);
        assert_eq!(runtime.usage(), 0);
    }

    #[test]
    fn leases_refuse_eviction_and_survive_explicit_retirement_until_final_drop() {
        let (runtime, bytes) = two_module_cache();
        let first = runtime.lookup(1).unwrap();
        let second = runtime.lookup(2).unwrap();
        assert!(matches!(
            request(&runtime, 3),
            Err(JitError::ResourceLimit("native mappings"))
        ));
        {
            let manager = runtime.0.borrow();
            assert_eq!(manager.code.len(), 2);
            assert_eq!(manager.stats.cache_evictions, 0);
            assert_eq!(manager.stats.cache_eviction_refusals, 1);
            assert_eq!(manager.tracked[&3].attempts, 1);
        }
        assert_executable(&first);
        assert_executable(&second);
        drop(second);
        request(&runtime, 4).unwrap();
        assert_executable(&first);
        assert_executable(&runtime.lookup(4).unwrap());
        let metadata = runtime.0.borrow().metadata.0.clone();
        runtime.0.borrow_mut().clear_registrations();
        assert_eq!(runtime.usage(), bytes);
        assert!(metadata.current() > 0);
        assert_executable(&first);
        drop(first);
        assert_eq!(runtime.usage(), 0);
        assert_eq!(metadata.current(), 0);
    }

    #[test]
    fn saturated_recency_clock_uses_generation_tiebreak() {
        let (runtime, _) = two_module_cache();
        runtime.0.borrow_mut().clock = u64::MAX;
        drop(runtime.lookup(2).unwrap());
        drop(runtime.lookup(1).unwrap());
        request(&runtime, 3).unwrap();
        let manager = runtime.0.borrow();
        assert_eq!(manager.clock, u64::MAX);
        assert!(!manager.code.contains_key(&1));
        assert!(manager.code.contains_key(&2));
        assert!(manager.code.contains_key(&3));
    }

    #[test]
    fn exhausted_budget_preserves_existing_cache_without_eviction() {
        let (runtime, bytes) = two_module_cache();
        runtime.0.borrow_mut().config.max_compile_attempts = 1;
        assert!(matches!(
            request(&runtime, 3),
            Err(JitError::ResourceLimit("native mappings"))
        ));
        assert_eq!(runtime.usage(), 2 * bytes);
        let manager = runtime.0.borrow();
        assert_eq!(manager.stats.cache_evictions, 0);
        assert_eq!(manager.tracked[&3].attempts, 1);
        assert_eq!(manager.code.len(), 2);
    }

    #[test]
    fn oversized_retry_stops_after_one_victim_and_two_failed_compilations() {
        let (runtime, bytes) = two_module_cache();
        {
            let mut manager = runtime.0.borrow_mut();
            manager.tracked.insert(3, Tracking::default());
            manager.enqueue(3, true);
            assert_eq!(manager.next_request(), Some(3));
        }
        let source = format!("local sum=0 {} return sum", "sum=sum+1 ".repeat(1000));
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "oversized", source.as_bytes()).unwrap();
            ir::Snapshot::new(&prototype, 4096, JitConfig::default().max_snapshot_bytes).unwrap()
        });
        assert!(matches!(
            runtime.compile(3, snapshot),
            Err(JitError::ResourceLimit("native mappings"))
        ));
        assert_eq!(runtime.usage(), bytes);
        let manager = runtime.0.borrow();
        assert_eq!(manager.stats.cache_evictions, 1);
        assert_eq!(manager.stats.compilation_failures, 2);
        assert_eq!(manager.tracked[&3].attempts, 2);
        assert!(!manager.code.contains_key(&1));
        assert!(manager.code.contains_key(&2));
        assert!(!manager.code.contains_key(&3));
    }

    #[test]
    fn metadata_refusal_does_not_evict_or_consume_a_retry() {
        let (runtime, bytes) = two_module_cache();
        {
            let mut manager = runtime.0.borrow_mut();
            manager.tracked.insert(3, Tracking::default());
            manager.enqueue(3, true);
            assert_eq!(manager.next_request(), Some(3));
            manager.metadata.0.set_limit(1);
        }
        assert!(matches!(
            runtime.compile(3, snapshot()),
            Err(JitError::ResourceLimit("JIT metadata"))
        ));
        assert_eq!(runtime.usage(), 2 * bytes);
        let manager = runtime.0.borrow();
        assert_eq!(manager.code.len(), 2);
        assert_eq!(manager.stats.cache_evictions, 0);
        assert_eq!(manager.stats.cache_eviction_refusals, 0);
        assert_eq!(manager.stats.compilation_failures, 1);
        assert_eq!(manager.tracked[&3].attempts, 1);
    }

    #[test]
    fn lookup_misses_preserve_clock_and_successful_hits_update_entry_only() {
        let (runtime, _) = two_module_cache();
        let (clock, stats) = {
            let manager = runtime.0.borrow();
            (manager.clock, manager.stats)
        };
        assert!(runtime.lookup(999).is_none());
        {
            let manager = runtime.0.borrow();
            assert_eq!(manager.clock, clock);
            assert_eq!(manager.stats.code_lookups, stats.code_lookups + 1);
            assert_eq!(manager.stats.code_leases, stats.code_leases);
        }
        drop(runtime.lookup(1).unwrap());
        let manager = runtime.0.borrow();
        assert_eq!(manager.clock, clock + 1);
        assert_eq!(manager.code[&1].last_used, clock + 1);
        assert_eq!(manager.stats.code_lookups, stats.code_lookups + 2);
        assert_eq!(manager.stats.code_leases, stats.code_leases + 1);
        assert_eq!(manager.tracked[&1].attempts, 1);
        assert!(!manager.tracked[&1].queued);
    }

    #[test]
    fn cache_entry_recency_is_charged_by_the_metadata_allocator() {
        let ledger = Ledger::new(65536);
        let allocator = BudgetAllocator(ledger.clone());
        let mut bare: MetadataMap<u64, Rc<backend::Code>> = metadata_map(allocator.clone());
        bare.try_reserve(1).unwrap();
        let bare_bytes = ledger.current();
        drop(bare);
        assert_eq!(ledger.current(), 0);
        ledger.set_limit(bare_bytes);
        let mut entries: MetadataMap<u64, CachedCode> = metadata_map(allocator);
        assert!(entries.try_reserve(1).is_err());
        assert_eq!(ledger.current(), 0);
        assert_eq!(ledger.refusals(), 1);
        ledger.set_limit(65536);
        entries.try_reserve(1).unwrap();
        assert!(ledger.current() > bare_bytes);
        drop(entries);
        assert_eq!(ledger.current(), 0);
    }
}
