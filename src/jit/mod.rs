//! Optional native execution configuration and per-state diagnostics.

use std::{
    cell::RefCell,
    collections::hash_map::RandomState,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use allocator_api2::vec::Vec;
use hashbrown::HashMap;
use ottavino_gc_arena::Collect;
use thiserror::Error;

mod abi;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod backend;
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
use resources::{BudgetAllocator, Ledger};

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
    pub registered_prototypes: usize,
    pub installed_regions: u64,
    pub code_bytes: usize,
    pub snapshot_bytes: usize,
    pub snapshot_peak_bytes: usize,
    pub metadata_bytes: usize,
    pub metadata_peak_bytes: usize,
    pub metadata_allocation_refusals: usize,
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
    pub(crate) metadata: BudgetAllocator,
    pub(crate) snapshots: BudgetAllocator,
    pub(crate) memory: Arc<AtomicUsize>,
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    code: MetadataMap<u64, Rc<backend::Code>>,
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
        }
    }
}

#[derive(Default)]
pub(crate) struct Tracking {
    hotness: u32,
    attempts: u32,
    queued: bool,
}

impl Manager {
    pub(crate) fn configure(&mut self, config: JitConfig) {
        if config.max_metadata_bytes < self.config.max_metadata_bytes {
            self.clear_registrations();
        }
        if config.mode == JitMode::Off
            || config.max_code_bytes < self.config.max_code_bytes
            || config.max_snapshot_bytes < self.config.max_snapshot_bytes
            || config.max_prototype_instructions < self.config.max_prototype_instructions
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
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
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
            let (memory, limit, metadata) = {
                let manager = self.0.borrow();
                (
                    manager.memory.clone(),
                    manager.config.max_code_bytes,
                    manager.metadata.clone(),
                )
            };
            #[cfg(test)]
            let failure = self.0.borrow().memory_failure;
            let result = backend::compile_in(
                &snapshot,
                memory,
                limit,
                metadata,
                #[cfg(test)]
                failure,
            );
            let mut manager = self.0.borrow_mut();
            match result {
                Ok(code) => {
                    if manager.tracked.contains_key(&id) && manager.config.mode == JitMode::Auto {
                        if manager.code.try_reserve(1).is_err() {
                            manager.stats.compilation_failures =
                                manager.stats.compilation_failures.saturating_add(1);
                            return Err(JitError::ResourceLimit("JIT metadata"));
                        }
                        manager.code.insert(id, Rc::new(code));
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
            let code = manager.code.get(&id).cloned()?;
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
            .saturating_add(u64::from(exit.reason == 1));
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
