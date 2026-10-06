//! Optional native execution configuration and per-state diagnostics.

use std::{cell::RefCell, sync::atomic::Ordering};

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
mod arrays;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod atomic_owner;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod backend;
#[cfg(any(
    test,
    all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod calls;
#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod canonical;
#[cfg(all(
    test,
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod chains;
#[cfg(test)]
mod continuations;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod dominance;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod entry_flow;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod exit_flow;
mod exits;
mod flow;
#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod fuzz;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod global_box;
mod global_owner;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod handoff;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod helper_flow;
mod helpers;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(test, not(miri))
))]
mod integer;
pub(crate) mod ir;
#[cfg(any(
    test,
    all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod leaf;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod memory_status;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod model;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod owner;
#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod pairs;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod preds;
pub(crate) mod registry;
pub(crate) mod resources;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod segments;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod shape;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod tags;
mod work;
use resources::{BudgetAllocator, Compaction, Compactor, Ledger, LedgerRef, MappingCounter};

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
    pub max_relocations: usize,
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
            max_relocations: 65536,
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
            || self.max_relocations == 0
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
    /// Completed native bytecodes plus interpreted opcode dispatches, including errors and transitions.
    pub total_dispatches: u64,
    pub interpreted_slices: u64,
    pub interpreted_instructions: u64,
    pub native_entries: u64,
    pub native_pair_calls: u64,
    pub native_pair_returns: u64,
    pub native_pair_cache_hits: u64,
    pub code_lookups: u64,
    pub code_leases: u64,
    pub native_instructions: u64,
    pub guard_exits: u64,
    pub native_interpreter_exits: u64,
    pub native_budget_exits: u64,
    pub native_panic_exits: u64,
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
    /// Page-rounded bytes of live native mappings, including retired leased code.
    pub code_bytes: usize,
    /// Live provider-requested payload bytes, excluding page and alignment padding.
    pub code_requested_bytes: usize,
    /// Live requested/reserved snapshot and known compiler-workspace bytes.
    pub snapshot_bytes: usize,
    pub snapshot_peak_bytes: usize,
    /// Live charged metadata payloads and owners, excluding bootstrap storage.
    pub metadata_bytes: usize,
    pub metadata_peak_bytes: usize,
    pub metadata_allocation_refusals: usize,
    /// Charged JIT storage, including bootstrap owners and compiler reservations.
    pub accounted_jit_bytes: usize,
    /// Charged runtime and ledger-owner allocation layouts.
    pub bootstrap_bytes: usize,
    pub accounted_jit_peak_bytes: usize,
    pub host_allocation_refusals: usize,
    pub metadata_compaction_attempts: u64,
    pub metadata_compactions: u64,
    pub metadata_compaction_refusals: u64,
    pub metadata_compaction_bytes: u64,
    pub registration_refusals: u64,
    pub queued_requests: usize,
}

impl JitStats {
    #[cfg(any(
        test,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    fn record_native_exit(&mut self, exit: &abi::Exit) {
        self.total_dispatches = self
            .total_dispatches
            .saturating_add(u64::from(exit.instructions));
        self.native_entries = self.native_entries.saturating_add(1);
        self.native_instructions = self
            .native_instructions
            .saturating_add(u64::from(exit.instructions));
        let counter = match exit.reason {
            reason if reason == exits::Kind::Interpreter as u32 => {
                &mut self.native_interpreter_exits
            }
            reason if reason == exits::Kind::Guard as u32 => &mut self.guard_exits,
            reason if reason == exits::Kind::Budget as u32 => &mut self.native_budget_exits,
            reason if reason == exits::Kind::Panic as u32 => &mut self.native_panic_exits,
            _ => panic!("invalid native exit reason"),
        };
        *counter = counter.saturating_add(1);
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn zero_work_exits_still_count_native_entries() {
        let mut stats = JitStats::default();
        for reason in [
            exits::Kind::Interpreter,
            exits::Kind::Guard,
            exits::Kind::Budget,
            exits::Kind::Panic,
        ] {
            stats.record_native_exit(&abi::Exit {
                pc: 0,
                instructions: 0,
                reason: reason as u32,
            });
        }
        assert_eq!(stats.native_entries, 4);
        assert_eq!(stats.total_dispatches, 0);
        assert_eq!(stats.native_instructions, 0);
        assert_eq!(
            (
                stats.native_interpreter_exits,
                stats.guard_exits,
                stats.native_budget_exits,
                stats.native_panic_exits
            ),
            (1, 1, 1, 1)
        );
    }

    #[test]
    fn native_exit_counts_partition_entries_and_saturate_independently() {
        for reason in [
            exits::Kind::Interpreter,
            exits::Kind::Guard,
            exits::Kind::Budget,
            exits::Kind::Panic,
        ] {
            for initial in [0, u64::MAX - 1, u64::MAX] {
                let mut stats = JitStats {
                    total_dispatches: initial,
                    native_entries: initial,
                    native_instructions: initial,
                    guard_exits: initial,
                    native_interpreter_exits: initial,
                    native_budget_exits: initial,
                    native_panic_exits: initial,
                    ..Default::default()
                };
                let before = stats;
                stats.record_native_exit(&abi::Exit {
                    pc: 17,
                    instructions: 3,
                    reason: reason as u32,
                });
                assert_eq!(stats.native_entries, initial.saturating_add(1));
                assert_eq!(stats.total_dispatches, initial.saturating_add(3));
                assert_eq!(stats.native_instructions, initial.saturating_add(3));
                for (kind, count) in [
                    (exits::Kind::Interpreter, stats.native_interpreter_exits),
                    (exits::Kind::Guard, stats.guard_exits),
                    (exits::Kind::Budget, stats.native_budget_exits),
                    (exits::Kind::Panic, stats.native_panic_exits),
                ] {
                    assert_eq!(count, initial.saturating_add(u64::from(kind == reason)));
                }
                assert_eq!(stats.interpreted_slices, before.interpreted_slices);
                assert_eq!(stats.hook_exits, before.hook_exits);
                assert_eq!(stats.helper_declines, before.helper_declines);
            }
        }
    }

    #[test]
    fn interpreter_dispatches_survive_errors_and_unwinding() {
        let runtime = Runtime::new();
        {
            let mut slice = runtime.interpreter_stats();
            slice.dispatches = 3;
            slice.reported_instructions = Some(2);
        }
        {
            let mut slice = runtime.interpreter_stats();
            slice.dispatches = 4;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut slice = runtime.interpreter_stats();
            slice.dispatches = 5;
            panic!("dispatch unwind sentinel");
        }));
        assert!(result.is_err());
        let stats = runtime.0.borrow().stats;
        assert_eq!(stats.total_dispatches, 12);
        assert_eq!(stats.interpreted_slices, 1);
        assert_eq!(stats.interpreted_instructions, 2);
        assert_eq!(stats.native_entries, 0);
    }

    #[test]
    fn mixed_dispatches_preserve_native_totals_and_successful_reports() {
        for initial in [0, u64::MAX - 1, u64::MAX] {
            for native in [0, 1, 3, 64] {
                for interpreted in [0u32, 1, 3, 64] {
                    for report in [None, Some(interpreted.saturating_sub(1)), Some(interpreted)] {
                        let runtime = Runtime::new();
                        runtime.0.borrow_mut().stats.total_dispatches = initial;
                        if native != 0 {
                            runtime.0.borrow_mut().stats.record_native_exit(&abi::Exit {
                                pc: 3,
                                instructions: native,
                                reason: exits::Kind::Budget as u32,
                            });
                        }
                        {
                            let mut slice = runtime.interpreter_stats();
                            slice.dispatches = interpreted;
                            slice.reported_instructions = report;
                        }
                        let stats = runtime.0.borrow().stats;
                        assert_eq!(
                            stats.total_dispatches,
                            initial.saturating_add(u64::from(native + interpreted))
                        );
                        assert_eq!(stats.native_instructions, u64::from(native));
                        assert_eq!(stats.native_entries, u64::from(native != 0));
                        assert_eq!(
                            stats.interpreted_slices,
                            u64::from(report.is_some() && interpreted != 0)
                        );
                        assert_eq!(
                            stats.interpreted_instructions,
                            u64::from(report.unwrap_or(0))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mixed_unwind_preserves_unreported_interpreter_dispatches() {
        let runtime = Runtime::new();
        runtime.0.borrow_mut().stats.record_native_exit(&abi::Exit {
            pc: 5,
            instructions: 5,
            reason: exits::Kind::Interpreter as u32,
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut slice = runtime.interpreter_stats();
            slice.dispatches = 3;
            panic!("mixed dispatch unwind sentinel");
        }));
        assert!(result.is_err());
        let stats = runtime.0.borrow().stats;
        assert_eq!(stats.total_dispatches, 8);
        assert_eq!(stats.native_instructions, 5);
        assert_eq!(stats.native_entries, 1);
        assert_eq!(stats.interpreted_instructions, 0);
        assert_eq!(stats.interpreted_slices, 0);
    }

    #[test]
    fn interpreter_dispatches_saturate_without_counting_empty_slices() {
        let runtime = Runtime::new();
        for initial in [0, u64::MAX - 1, u64::MAX] {
            runtime.0.borrow_mut().stats = JitStats {
                total_dispatches: initial,
                ..Default::default()
            };
            {
                let mut slice = runtime.interpreter_stats();
                slice.reported_instructions = Some(0);
            }
            assert_eq!(runtime.0.borrow().stats.total_dispatches, initial);
            assert_eq!(runtime.0.borrow().stats.interpreted_slices, 0);
            {
                let mut slice = runtime.interpreter_stats();
                slice.dispatches = 3;
                slice.reported_instructions = Some(2);
            }
            let stats = runtime.0.borrow().stats;
            assert_eq!(stats.total_dispatches, initial.saturating_add(3));
            assert_eq!(stats.interpreted_slices, 1);
            assert_eq!(stats.interpreted_instructions, 2);
        }
    }
    #[test]
    fn interpreter_counts_are_published_before_reentrant_callbacks() {
        use crate::{Callback, CallbackReturn, Closure, Executor, Fuel, Lua};
        use std::{cell::RefCell, rc::Rc};

        let run = |mode| {
            let mut lua = Lua::empty();
            lua.set_gc_pacing(false);
            lua.set_jit_config(JitConfig {
                mode,
                hot_threshold: u32::MAX,
                ..Default::default()
            })
            .unwrap();
            let observed = Rc::new(RefCell::new(Vec::new()));
            let snapshots = observed.clone();
            let executor = lua.enter(|ctx| {
                let child =
                    Closure::load(ctx, None, b"local n=0 for i=1,7 do n=n+i end return n").unwrap();
                let callback =
                    Callback::from_fn_with(&ctx, child, move |child, ctx, _, mut stack| {
                        let before = ctx.jit().0.borrow().stats.total_dispatches;
                        assert!(before > 0);
                        let nested = Executor::start(ctx, (*child).into(), ());
                        let mut fuel = Fuel::with(65536);
                        while !nested.step(ctx, &mut fuel).unwrap() {}
                        assert_eq!(nested.take_result::<i64>(ctx).unwrap().unwrap(), 28);
                        let after = ctx.jit().0.borrow().stats.total_dispatches;
                        assert!(after > before);
                        snapshots.borrow_mut().push((before, after));
                        stack.clear();
                        Ok(CallbackReturn::Return)
                    });
                ctx.globals().set(ctx, "inspect", callback).unwrap();
                let parent = Closure::load(
                    ctx,
                    None,
                    b"local n=0 for i=1,5 do n=n+i end inspect() return n",
                )
                .unwrap();
                ctx.stash(Executor::start(ctx, parent.into(), ()))
            });
            if !cfg!(miri) && mode == JitMode::Auto && lua.jit_capabilities().supported_target {
                assert_eq!(lua.prepare_jit().unwrap(), 2);
            }
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), 15);
            let stats = lua.jit_stats();
            let snapshots = observed.borrow().clone();
            assert_eq!(snapshots.len(), 1);
            assert!(stats.total_dispatches > snapshots[0].1);
            (snapshots, stats.total_dispatches)
        };
        assert_eq!(run(JitMode::Off), run(JitMode::Auto));
    }
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
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pairs: Option<pairs::State>,
    #[cfg(test)]
    mock: Option<mock::Mock>,
    #[cfg(test)]
    before_compile: Option<Box<dyn FnOnce()>>,
    #[cfg(test)]
    activation_limit: usize,
    #[cfg(test)]
    activation_counts: (usize, usize, usize),
    #[cfg(test)]
    scalar_leaves: bool,
    #[cfg(all(test, not(miri)))]
    scalar_kernels: bool,
    #[cfg(all(test, not(miri)))]
    integer_activations: bool,
    #[cfg(test)]
    scalar_native_counts: (u64, u64),
    #[cfg(all(test, not(miri)))]
    scalar_activation_entries: u64,
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
    pub(crate) host: LedgerRef,
    pub(crate) memory: MappingCounter,
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
        let host = Ledger::host(usize::MAX);
        let metadata = BudgetAllocator(Ledger::child(config.max_metadata_bytes, host.clone()));
        let snapshots = BudgetAllocator(Ledger::child(config.max_snapshot_bytes, host.clone()));
        Self {
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            pairs: None,
            #[cfg(test)]
            mock: None,
            #[cfg(test)]
            before_compile: None,
            #[cfg(test)]
            activation_limit: 0,
            #[cfg(test)]
            activation_counts: (0, 0, 0),
            #[cfg(test)]
            scalar_leaves: false,
            #[cfg(all(test, not(miri)))]
            scalar_kernels: false,
            #[cfg(all(test, not(miri)))]
            integer_activations: false,
            #[cfg(test)]
            scalar_native_counts: (0, 0),
            #[cfg(all(test, not(miri)))]
            scalar_activation_entries: 0,
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
            memory: MappingCounter::new(host.clone()),
            host,
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
    code: owner::Shared<backend::Code>,
    last_used: u64,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CacheKey {
    Prototype(u64),
    #[cfg(not(miri))]
    Pair(pairs::Key),
}

impl Manager {
    fn queued_count(&self) -> usize {
        let count = self.queue.len();
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        let count = count + self.pairs.as_ref().map_or(0, pairs::State::queued);
        count
    }

    pub(crate) fn needs_compaction(&mut self) -> bool {
        let needed = self
            .tracked_compactor
            .needed(self.tracked.len(), self.tracked.capacity())
            | self
                .queue_compactor
                .needed(self.queue.len(), self.queue.capacity());
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        let needed = needed
            | self
                .pairs
                .as_mut()
                .is_some_and(pairs::State::needs_compaction);
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
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if let Some(pairs) = &mut self.pairs {
            let results = pairs.compact();
            for result in results {
                self.record_compaction(result);
            }
        }
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
        if !self.evict_cached(CacheKey::Prototype(id)) {
            return false;
        }
        self.tracked.get_mut(&id).unwrap().attempts += 1;
        true
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn evict_cached(&mut self, exclude: CacheKey) -> bool {
        let victim = self
            .code
            .iter()
            .filter(|(candidate, entry)| {
                CacheKey::Prototype(**candidate) != exclude
                    && owner::Shared::strong_count(&entry.code) == 1
            })
            .min_by_key(|(candidate, entry)| (entry.last_used, **candidate))
            .map(|(candidate, entry)| (entry.last_used, CacheKey::Prototype(*candidate)));
        #[cfg(not(miri))]
        let victim = victim
            .into_iter()
            .chain(self.pairs.as_ref().and_then(|pairs| {
                let excluded = match exclude {
                    CacheKey::Pair(key) => Some(key),
                    CacheKey::Prototype(_) => None,
                };
                pairs
                    .victim(excluded)
                    .map(|(clock, key)| (clock, CacheKey::Pair(key)))
            }))
            .min();
        let Some((_, victim)) = victim else {
            self.stats.cache_eviction_refusals =
                self.stats.cache_eviction_refusals.saturating_add(1);
            return false;
        };
        match victim {
            CacheKey::Prototype(id) => {
                self.code.remove(&id);
                if let Some(tracking) = self.tracked.get_mut(&id) {
                    tracking.hotness = 0;
                }
            }
            #[cfg(not(miri))]
            CacheKey::Pair(key) => self.pairs.as_mut().unwrap().evict(key),
        }
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
            || config.max_relocations < self.config.max_relocations
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
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            if let Some(pairs) = &mut self.pairs {
                pairs.configure(
                    &config,
                    config.max_queue_entries.saturating_sub(self.queue.len()),
                );
            }
            self.stats.queued_requests = self.queued_count();
        }
        self.metadata.0.set_limit(config.max_metadata_bytes);
        self.snapshots.0.set_limit(config.max_snapshot_bytes);
        self.config = config;
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if self.config.mode == JitMode::Auto && self.pairs.is_none() {
            self.pairs = Some(pairs::State::new(self.metadata.clone()));
        }
    }

    pub(crate) fn retire(&mut self, id: u64) {
        self.tracked.remove(&id);
        self.stats.registered_prototypes = self.tracked.len();
        self.queue.retain(|request| *request != id);
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if let Some(pairs) = &mut self.pairs {
            pairs.retire(id);
        }
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        self.code.remove(&id);
        self.stats.queued_requests = self.queued_count();
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
        let queue_space = self.queued_count() < self.config.max_queue_entries;
        let Some(tracking) = self.tracked.get_mut(&id) else {
            return;
        };
        tracking.hotness = tracking.hotness.saturating_add(1);
        if (force || tracking.hotness >= self.config.hot_threshold)
            && !tracking.queued
            && tracking.attempts < self.config.max_compile_attempts
            && queue_space
        {
            if self.queue.try_reserve(1).is_err() {
                return;
            }
            self.queue.push(id);
            tracking.queued = true;
            self.stats.compilation_requests = self.stats.compilation_requests.saturating_add(1);
            self.stats.queued_requests = self.queued_count();
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
        self.stats.queued_requests = self.queued_count();
        Some(id)
    }

    pub(crate) fn clear(&mut self) {
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if let Some(pairs) = &mut self.pairs {
            pairs.clear();
        }
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
pub(crate) struct Runtime(pub(crate) RuntimeOwner);

pub(crate) type RuntimeOwner =
    global_owner::GlobalShared<RefCell<Manager>, resources::HeaderCharge>;

pub(crate) struct Prepared {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    code: owner::Shared<backend::Code>,
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) struct PreparedPair {
    program: owner::Shared<canonical::Program>,
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[derive(Default)]
pub(crate) struct PairScope {
    pub handoff: Option<PreparedPair>,
    pub cache: Option<PreparedPair>,
    caller: usize,
    callee: usize,
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) struct PairOutcome {
    pub calls: usize,
    pub returns: usize,
    pub result: Result<(), crate::thread::VMError>,
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl PreparedPair {
    pub(crate) fn invoke<'gc>(
        &self,
        ctx: crate::Context<'gc>,
        host: &mut crate::thread::activation::ActivationHost<'gc, '_>,
        budget: u32,
        prefix: u32,
    ) -> Option<PairOutcome> {
        let outcome = self.program.invoke_result(ctx, host, budget, prefix);
        if let Some(outcome) = &outcome {
            ctx.jit().record_pair_execution(
                outcome.calls,
                if outcome.result.is_ok() {
                    outcome.returns
                } else {
                    0
                },
            );
        }
        outcome
    }
}

pub(crate) struct InterpreterStats<'a> {
    runtime: &'a Runtime,
    pub dispatches: u32,
    pub reported_instructions: Option<u32>,
}

impl Drop for InterpreterStats<'_> {
    fn drop(&mut self) {
        let mut manager = self.runtime.0.borrow_mut();
        let stats = &mut manager.stats;
        stats.total_dispatches = stats
            .total_dispatches
            .saturating_add(u64::from(self.dispatches));
        if let Some(instructions) = self.reported_instructions {
            stats.interpreted_slices = stats
                .interpreted_slices
                .saturating_add(u64::from(self.dispatches != 0));
            stats.interpreted_instructions = stats
                .interpreted_instructions
                .saturating_add(u64::from(instructions));
        }
    }
}

impl Runtime {
    #[cfg(test)]
    pub(crate) fn test_activation_host(&self, limit: usize) {
        self.0.borrow_mut().activation_limit = limit;
    }

    #[cfg(test)]
    pub(crate) fn activation_limit(&self) -> usize {
        self.0.borrow().activation_limit
    }

    #[cfg(test)]
    pub(crate) fn record_activations(&self, count: usize, growths: usize) {
        let mut manager = self.0.borrow_mut();
        manager.activation_counts.0 += 1;
        manager.activation_counts.1 += count;
        manager.activation_counts.2 += growths;
    }

    #[cfg(test)]
    pub(crate) fn test_activation_counts(&self) -> (usize, usize, usize) {
        self.0.borrow().activation_counts
    }

    #[cfg(test)]
    pub(crate) fn call_transition(
        &self,
        prepared: &Prepared,
        pc: usize,
    ) -> Option<crate::opcode::CallTransition> {
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            prepared
                .code
                .continuations
                .as_ref()?
                .at(pc)
                .map(|request| request.transition)
        }
        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = (prepared, pc);
            None
        }
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) fn test_scalar_kernels(&self, enabled: bool) {
        self.0.borrow_mut().scalar_kernels = enabled;
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) fn test_scalar_activation_entries(&self) -> u64 {
        self.0.borrow().scalar_activation_entries
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) fn test_integer_activations(&self, enabled: bool) {
        let mut manager = self.0.borrow_mut();
        manager.scalar_kernels = enabled;
        manager.integer_activations = enabled;
    }

    pub(crate) fn new() -> Self {
        Self::try_new(Manager::default())
            .unwrap_or_else(|_| std::alloc::handle_alloc_error(RuntimeOwner::allocation_layout()))
    }

    fn try_new(manager: Manager) -> Result<Self, allocator_api2::alloc::AllocError> {
        let charge = resources::HeaderCharge::try_new(
            manager.host.clone(),
            RuntimeOwner::allocation_bytes(),
        )?;
        RuntimeOwner::try_new(RefCell::new(manager), charge).map(Self)
    }

    pub(crate) fn interpreter_stats(&self) -> InterpreterStats<'_> {
        InterpreterStats {
            runtime: self,
            dispatches: 0,
            reported_instructions: None,
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.0.borrow().config.mode == JitMode::Auto
    }

    #[cfg(any(
        test,
        all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    pub(crate) fn scoped_activation_limit(&self) -> usize {
        let manager = self.0.borrow();
        #[cfg(test)]
        if manager.activation_limit > 0 {
            return manager.activation_limit;
        }
        #[cfg(all(test, not(miri)))]
        if manager.scalar_kernels {
            return 0;
        }
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if manager.config.mode == JitMode::Auto && manager.pairs.is_some() {
            return 64;
        }
        0
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    pub(crate) fn try_scalar_activation<'gc>(
        &self,
        ctx: crate::Context<'gc>,
        closure: crate::Closure<'gc>,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        budget: u32,
    ) -> Option<u32> {
        if *registers.pc != 0 || budget <= 3 {
            return None;
        }
        {
            let manager = self.0.borrow();
            if !manager.scalar_kernels || manager.config.mode != JitMode::Auto {
                return None;
            }
        }
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, closure.prototype())?;
        let code = self.lookup(id)?;
        let kernel = code.code.scalar_kernel.as_ref()?;
        let pattern = kernel.scalar_leaf?;
        let count = kernel.registers;
        if count > 8 || registers.stack_frame.len() < count {
            return None;
        }
        let mut scratch = [abi::Slot {
            tag: abi::NIL,
            bits: 0,
        }; 8];
        let slots = &mut scratch[..count];
        for (slot, value) in slots.iter_mut().zip(registers.stack_frame.iter().copied()) {
            *slot = abi::Slot::from_value(value);
        }
        let cell = closure.upvalues().get(usize::from(pattern.upvalue))?.get();
        let binding = leaf::Binding::from_origin(registers.projection_origin(cell)?, slots)?;
        let right = match pattern.right {
            leaf::Operand::Register(index) => *slots.get(usize::from(index.0))?,
            leaf::Operand::Constant(index) => {
                abi::Slot::from_constant(*closure.prototype().constants.get(usize::from(index))?)
            }
        };
        if !binding.integer_activation(pattern, right) {
            return None;
        }
        let completed = self.invoke_scalar_kernel(kernel, registers, slots, binding, budget);
        assert_eq!(completed, 3);
        assert_eq!(*registers.pc, 3);
        let mut manager = self.0.borrow_mut();
        manager.scalar_activation_entries = manager.scalar_activation_entries.saturating_add(1);
        Some(completed)
    }

    pub(crate) fn usage(&self) -> usize {
        self.0.borrow().memory.load(Ordering::Relaxed)
    }

    pub(crate) fn compile(&self, id: u64, snapshot: ir::Snapshot) -> Result<(), JitError> {
        #[cfg(test)]
        {
            let hook = self.0.borrow_mut().before_compile.take();
            if let Some(hook) = hook {
                hook();
            }
        }
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
                #[cfg(all(test, not(miri)))]
                if self.activation_limit() > 0 {
                    return backend::compile_continuations_in(
                        &snapshot,
                        memory.clone(),
                        limit,
                        metadata.clone(),
                        work,
                        failure,
                    );
                }
                #[cfg(all(test, not(miri)))]
                if self.0.borrow().integer_activations
                    && leaf::Pattern::recognize(&snapshot).is_some()
                {
                    return backend::compile_integer_leaf_pair_in(
                        &snapshot,
                        memory.clone(),
                        limit,
                        metadata.clone(),
                        work,
                        failure,
                    );
                }
                #[cfg(all(test, not(miri)))]
                if self.0.borrow().scalar_kernels && leaf::Pattern::recognize(&snapshot).is_some() {
                    return backend::compile_leaf_pair_in(
                        &snapshot,
                        memory.clone(),
                        limit,
                        metadata.clone(),
                        work,
                        failure,
                    );
                }
                #[cfg(test)]
                if self.0.borrow().scalar_leaves && leaf::Pattern::recognize(&snapshot).is_some() {
                    return backend::compile_leaf_in(
                        &snapshot,
                        memory.clone(),
                        limit,
                        metadata.clone(),
                        work,
                        failure,
                    );
                }
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
            drop(snapshot);
            let mut manager = self.0.borrow_mut();
            match result {
                Ok(code) => {
                    #[cfg(test)]
                    if failure == backend::Failure::RequireReleasedSnapshot {
                        assert_eq!(manager.snapshots.0.current(), 0);
                    }
                    if manager.tracked.contains_key(&id) && manager.config.mode == JitMode::Auto {
                        #[cfg(all(test, not(miri)))]
                        if (failure == backend::Failure::RefuseScalarCacheStorage
                            && code.scalar_kernel.is_some())
                            || (failure == backend::Failure::RefuseContinuationCacheStorage
                                && code.continuations.is_some())
                        {
                            manager.metadata.0.set_limit(manager.metadata.0.current());
                        }
                        let reserved = manager.code.try_reserve(1);
                        #[cfg(all(test, not(miri)))]
                        let (code, reserved) = if reserved.is_err() {
                            let mut code = code;
                            if code.discard_optional_entries() {
                                (code, manager.code.try_reserve(1))
                            } else {
                                (code, reserved)
                            }
                        } else {
                            (code, reserved)
                        };
                        if reserved.is_err() {
                            manager.stats.compilation_failures =
                                manager.stats.compilation_failures.saturating_add(1);
                            return Err(JitError::ResourceLimit("JIT metadata"));
                        }
                        #[cfg(test)]
                        match failure {
                            backend::Failure::RefuseOwnerStorage => {
                                manager.metadata.0.set_limit(manager.metadata.0.current())
                            }
                            backend::Failure::RefuseOwnerAllocation => {
                                manager.metadata.0.fail_after(0)
                            }
                            _ => {}
                        }
                        #[cfg(all(test, not(miri)))]
                        let owner = code.into_shared(manager.metadata.clone());
                        #[cfg(not(all(test, not(miri))))]
                        let owner = owner::Shared::try_new(code, manager.metadata.clone());
                        let code = match owner {
                            Ok(code) => code,
                            Err(_) => {
                                manager.stats.compilation_failures =
                                    manager.stats.compilation_failures.saturating_add(1);
                                return Err(JitError::ResourceLimit("JIT metadata"));
                            }
                        };
                        manager.clock = manager.clock.saturating_add(1);
                        let last_used = manager.clock;
                        manager.code.insert(id, CachedCode { code, last_used });
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

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(crate) fn resume_lease(&self, id: u64, prepared: Prepared) -> Option<Prepared> {
        let manager = self.0.borrow();
        (manager.config.mode == JitMode::Auto
            && manager
                .code
                .get(&id)
                .is_some_and(|entry| owner::Shared::ptr_eq(&entry.code, &prepared.code)))
        .then_some(prepared)
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
                0 => self.invoke::<8, false>(code, ctx, closure, registers, budget),
                1 => self.invoke::<1, true>(code, ctx, closure, registers, budget),
                2 => self.invoke::<2, true>(code, ctx, closure, registers, budget),
                3 => self.invoke::<3, true>(code, ctx, closure, registers, budget),
                4 => self.invoke::<4, true>(code, ctx, closure, registers, budget),
                5 => self.invoke::<5, true>(code, ctx, closure, registers, budget),
                6 => self.invoke::<6, true>(code, ctx, closure, registers, budget),
                7 => self.invoke::<7, true>(code, ctx, closure, registers, budget),
                8 => self.invoke::<8, true>(code, ctx, closure, registers, budget),
                9..=16 => self.invoke::<16, false>(code, ctx, closure, registers, budget),
                17..=32 => self.invoke::<32, false>(code, ctx, closure, registers, budget),
                33..=64 => self.invoke::<64, false>(code, ctx, closure, registers, budget),
                65..=128 => self.invoke::<128, false>(code, ctx, closure, registers, budget),
                129..=256 => self.invoke::<256, false>(code, ctx, closure, registers, budget),
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
    fn invoke<'gc, const CAPACITY: usize, const EXACT: bool>(
        &self,
        code: &backend::Code,
        ctx: crate::Context<'gc>,
        closure: crate::Closure<'gc>,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        budget: u32,
    ) -> u32 {
        let register_count = if EXACT { CAPACITY } else { code.registers };
        let mut scratch = [std::mem::MaybeUninit::<abi::Slot>::uninit(); CAPACITY];
        for (slot, value) in scratch[..register_count]
            .iter_mut()
            .zip(registers.stack_frame[..register_count].iter().copied())
        {
            slot.write(abi::Slot::from_value(value));
        }
        // Borrow the initialized prefix of scalar scratch storage.
        let slots = unsafe {
            std::slice::from_raw_parts_mut(scratch.as_mut_ptr().cast::<abi::Slot>(), register_count)
        };
        #[cfg(all(test, not(miri)))]
        if let Some(kernel) = code
            .scalar_kernel
            .as_ref()
            .filter(|kernel| !kernel.integer_activation)
        {
            let pattern = kernel.scalar_leaf.expect("scalar kernel has no pattern");
            let binding = if pattern.permits_cell_entry(*registers.pc, slots) {
                closure
                    .upvalues()
                    .get(usize::from(pattern.upvalue))
                    .and_then(|cell| registers.projection_origin(cell.get()))
                    .and_then(|origin| leaf::Binding::from_origin(origin, slots))
            } else {
                None
            };
            if let Some(binding) = binding {
                return self.invoke_scalar_kernel(kernel, registers, slots, binding, budget);
            }
        }
        if code.projected_upvalues {
            projection::with_frame(
                registers,
                closure.upvalues(),
                slots,
                |registers, pointer, projection| {
                    self.invoke_frame::<true>(
                        code,
                        ctx,
                        closure,
                        registers,
                        (pointer, projection, register_count),
                        budget,
                    )
                },
            )
        } else {
            self.invoke_frame::<false>(
                code,
                ctx,
                closure,
                registers,
                (slots.as_mut_ptr(), None, register_count),
                budget,
            )
        }
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    fn invoke_scalar_kernel<'gc>(
        &self,
        code: &backend::Code,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        slots: &mut [abi::Slot],
        binding: leaf::Binding,
        budget: u32,
    ) -> u32 {
        let pc = *registers.pc;
        let (exit, delta) = binding
            .with_native(slots, |pointer, view| unsafe {
                code.invoke_cell_raw(pointer, pc, budget, view)
            })
            .expect("invalid exiting scalar kernel");
        for (slot, dest) in slots.iter().copied().zip(registers.stack_frame.iter_mut()) {
            slot.write_back(dest);
        }
        *registers.pc = usize::try_from(exit.pc).expect("native PC exceeds host range");
        delta
            .apply_registers(registers)
            .expect("invalid scalar kernel commit target");
        let mut manager = self.0.borrow_mut();
        #[cfg(test)]
        {
            manager.scalar_native_counts.0 = manager
                .scalar_native_counts
                .0
                .saturating_add(u64::from(delta.reads));
            manager.scalar_native_counts.1 = manager
                .scalar_native_counts
                .1
                .saturating_add(u64::from(delta.writes));
        }
        manager.stats.native_upvalue_reads = manager
            .stats
            .native_upvalue_reads
            .saturating_add(u64::from(delta.reads));
        manager.stats.native_upvalue_writes = manager
            .stats
            .native_upvalue_writes
            .saturating_add(u64::from(delta.writes));
        manager.stats.record_native_exit(&exit);
        exit.instructions
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[inline(always)]
    fn invoke_frame<'gc, const PROJECTED: bool>(
        &self,
        code: &backend::Code,
        ctx: crate::Context<'gc>,
        closure: crate::Closure<'gc>,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        binding: (
            *mut abi::Slot,
            Option<&mut dyn projection::Bridge<'gc>>,
            usize,
        ),
        budget: u32,
    ) -> u32 {
        let (slots, projection, register_count) = binding;
        debug_assert!(PROJECTED || projection.is_none());
        let mut frame = helpers::Frame {
            ctx,
            closure,
            registers,
            count: helpers::Counts::default(),
            slot_count: register_count,
            panic: None,
            projection,
        };
        let pc = *frame.registers.pc;
        let projection = if PROJECTED {
            frame
                .projection
                .as_deref()
                .map_or(std::ptr::null_mut(), |projection| projection.view_pointer())
        } else {
            std::ptr::null_mut()
        };
        let mut host = abi::Host {
            data: (&mut frame as *mut helpers::Frame<'_, '_, '_, '_>).cast(),
            projection,
        };
        #[cfg(test)]
        let scalar_binding = code.scalar_leaf.and_then(|pattern| {
            let cell = closure.upvalues().get(usize::from(pattern.upvalue))?.get();
            let origin = frame.registers.projection_origin(cell)?;
            let scratch = unsafe { std::slice::from_raw_parts(slots, register_count) };
            leaf::Binding::from_origin(origin, scratch)
        });
        #[cfg(test)]
        let (exit, scalar_delta) = if let Some(binding) = scalar_binding {
            let scratch = unsafe { std::slice::from_raw_parts_mut(slots, register_count) };
            let (exit, delta) = binding
                .with_native(scratch, |pointer, view| {
                    host.projection = view.cast();
                    unsafe { code.invoke_raw(pointer, pc, budget, &mut host) }
                })
                .expect("invalid exiting scalar-cell leaf");
            (exit, Some(delta))
        } else {
            (
                unsafe { code.invoke_raw(slots, pc, budget, &mut host) },
                None,
            )
        };
        #[cfg(not(test))]
        let exit = unsafe { code.invoke_raw(slots, pc, budget, &mut host) };
        let slots = unsafe { std::slice::from_raw_parts(slots, register_count) };
        for (slot, dest) in slots
            .iter()
            .copied()
            .zip(frame.registers.stack_frame[..register_count].iter_mut())
        {
            slot.write_back(dest);
        }
        if frame.panic.is_none() {
            *frame.registers.pc = usize::try_from(exit.pc).expect("native PC exceeds host range");
            #[cfg(test)]
            if let Some(delta) = &scalar_delta {
                delta
                    .apply_registers(frame.registers)
                    .expect("invalid scalar-cell commit target");
            }
            if PROJECTED {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Some(projection) = frame.projection.as_deref_mut() {
                        projection
                            .flush(ctx, frame.registers)
                            .expect("invalid exiting projection");
                    }
                }));
                if let Err(payload) = result {
                    frame.panic = Some(payload);
                }
            }
        }
        if PROJECTED {
            let projected = frame
                .projection
                .as_deref()
                .map_or(projection::Counts::default(), |projection| {
                    projection.counts()
                });
            frame.count.upvalue_reads = frame
                .count
                .upvalue_reads
                .saturating_add(u64::from(projected.reads));
            frame.count.upvalue_writes = frame
                .count
                .upvalue_writes
                .saturating_add(u64::from(projected.writes));
        }
        #[cfg(test)]
        if let Some(delta) = &scalar_delta {
            frame.count.upvalue_reads = frame
                .count
                .upvalue_reads
                .saturating_add(u64::from(delta.reads));
            frame.count.upvalue_writes = frame
                .count
                .upvalue_writes
                .saturating_add(u64::from(delta.writes));
        }
        let mut manager = self.0.borrow_mut();
        #[cfg(test)]
        if let Some(delta) = scalar_delta {
            manager.scalar_native_counts.0 = manager
                .scalar_native_counts
                .0
                .saturating_add(u64::from(delta.reads));
            manager.scalar_native_counts.1 = manager
                .scalar_native_counts
                .1
                .saturating_add(u64::from(delta.writes));
        }
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
        manager.stats.record_native_exit(&exit);
        if let Some(payload) = frame.panic.take() {
            drop(manager);
            std::panic::resume_unwind(payload);
        }
        exit.instructions
    }
}

#[cfg(test)]
mod runtime_owner_tests {
    use super::*;

    #[test]
    fn exact_charge_survives_clones_and_releases_after_final_runtime_drop() {
        let runtime = Runtime::new();
        let host = runtime.0.borrow().host.clone();
        let expected = 3 * LedgerRef::allocation_bytes() + RuntimeOwner::allocation_bytes();
        assert_eq!(
            (host.accounted(), host.bootstrap_bytes(), host.current()),
            (expected, expected, 0)
        );
        let peer = runtime.clone();
        assert_eq!(RuntimeOwner::strong_count(&runtime.0), 2);
        runtime.0.borrow_mut().config.hot_threshold = 17;
        drop(runtime);
        assert_eq!(host.accounted(), expected);
        assert_eq!(peer.0.borrow().config.hot_threshold, 17);
        peer.0.borrow_mut().clear_registrations();
        assert_eq!(host.accounted(), expected);
        drop(peer);
        assert_eq!(
            (host.accounted(), host.bootstrap_bytes(), host.current()),
            (
                LedgerRef::allocation_bytes(),
                LedgerRef::allocation_bytes(),
                0
            )
        );
    }

    #[test]
    fn quota_and_underlying_refusal_drop_manager_and_retain_no_runtime_charge() {
        for underlying in [false, true] {
            let manager = Manager::default();
            let host = manager.host.clone();
            let baseline = host.accounted();
            host.set_limit(baseline + RuntimeOwner::allocation_bytes() - usize::from(!underlying));
            let result = if underlying {
                global_owner::with_allocation_denied(|| Runtime::try_new(manager))
            } else {
                Runtime::try_new(manager)
            };
            assert!(result.is_err());
            assert_eq!(
                (host.accounted(), host.bootstrap_bytes(), host.current()),
                (
                    LedgerRef::allocation_bytes(),
                    LedgerRef::allocation_bytes(),
                    0
                )
            );
            assert_eq!(host.refusals(), usize::from(!underlying));
        }
    }

    #[test]
    fn exact_ceiling_admits_and_cell_borrow_unwind_preserves_owner() {
        let manager = Manager::default();
        let host = manager.host.clone();
        let expected = host.accounted() + RuntimeOwner::allocation_bytes();
        host.set_limit(expected);
        let runtime = Runtime::try_new(manager).unwrap();
        assert_eq!(host.accounted(), expected);
        let borrow = runtime.0.borrow();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.0.borrow_mut()))
                .is_err()
        );
        drop(borrow);
        runtime.0.borrow_mut().clear_registrations();
        assert_eq!(host.accounted(), expected);
        drop(runtime);
        assert_eq!(host.accounted(), LedgerRef::allocation_bytes());
    }

    #[test]
    fn runtime_owner_remains_non_send_and_non_sync() {
        trait AmbiguousSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousSend<u8> for T {}
        trait AmbiguousSync<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousSync<u8> for T {}
        let _ = <RuntimeOwner as AmbiguousSend<_>>::check;
        let _ = <RuntimeOwner as AmbiguousSync<_>>::check;
        let _ = <Runtime as AmbiguousSend<_>>::check;
        let _ = <Runtime as AmbiguousSync<_>>::check;
    }
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod scheduling_tests;

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
    fn hotness_threshold_saturates_without_duplicate_or_exhausted_requests() {
        let mut manager = Manager::default();
        manager.configure(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 2,
            max_compile_attempts: 1,
            ..Default::default()
        });
        manager.tracked.insert(1, Tracking::default());
        manager.enqueue(1, false);
        assert_eq!(manager.tracked[&1].hotness, 1);
        assert!(manager.queue.is_empty());
        manager.enqueue(1, false);
        assert_eq!(manager.queue.as_slice(), &[1]);
        manager.tracked.get_mut(&1).unwrap().hotness = u32::MAX;
        for _ in 0..3 {
            manager.enqueue(1, false);
        }
        assert_eq!(manager.tracked[&1].hotness, u32::MAX);
        assert_eq!(manager.queue.as_slice(), &[1]);
        assert_eq!(manager.stats.compilation_requests, 1);
        assert_eq!(manager.next_request(), Some(1));
        for force in [false, true] {
            manager.enqueue(1, force);
            assert!(manager.queue.is_empty());
        }
        assert_eq!(manager.tracked[&1].attempts, 1);
        assert_eq!(manager.stats.compilation_requests, 1);
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
    fn mapping_counter_retains_detached_lease_after_runtime_destruction() {
        let runtime = Runtime::new();
        runtime.0.borrow_mut().configure(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        });
        request(&runtime, 1).unwrap();
        let lease = runtime.lookup(1).unwrap();
        let (host, counter, metadata, bytes, bootstrap) = {
            let manager = runtime.0.borrow();
            (
                manager.host.clone(),
                MappingCounter::new(manager.metadata.0.clone()),
                manager.metadata.clone(),
                runtime.usage(),
                manager.host.bootstrap_bytes(),
            )
        };
        assert!(bytes > 0);
        let requested = counter.requested();
        assert!(requested > 0 && requested <= bytes);
        runtime.0.borrow_mut().clear_registrations();
        drop(runtime);
        assert_eq!(
            host.bootstrap_bytes(),
            bootstrap - RuntimeOwner::allocation_bytes() - LedgerRef::allocation_bytes()
        );
        assert_eq!(counter.load(Ordering::Relaxed), bytes);
        assert_eq!(counter.requested(), requested);
        assert_executable(&lease);
        assert!(matches!(
            backend::compile_in(
                &snapshot(),
                counter.clone(),
                bytes,
                metadata.clone(),
                work::Limits::from(&JitConfig::default()),
                backend::Failure::None,
            ),
            Err(JitError::ResourceLimit("native mappings"))
        ));
        assert_eq!(counter.load(Ordering::Relaxed), bytes);
        assert_eq!(counter.requested(), requested);
        assert_executable(&lease);
        drop(lease);
        assert_eq!((counter.load(Ordering::Relaxed), host.current()), (0, 0));
        assert_eq!(counter.requested(), 0);
        let recovered = backend::compile_in(
            &snapshot(),
            counter.clone(),
            bytes,
            metadata,
            work::Limits::from(&JitConfig::default()),
            backend::Failure::None,
        )
        .unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), bytes);
        drop(recovered);
        assert_eq!((counter.load(Ordering::Relaxed), host.current()), (0, 0));
    }

    #[test]
    fn cached_owner_refusal_preserves_peer_and_refused_source_then_recovers() {
        for failure in [
            backend::Failure::RefuseOwnerStorage,
            backend::Failure::RefuseOwnerAllocation,
        ] {
            let mut lua = crate::Lua::empty();
            lua.gc_stop();
            lua.set_jit_config(JitConfig {
                mode: JitMode::Auto,
                ..Default::default()
            })
            .unwrap();
            let peer =
                lua.enter(|ctx| ctx.stash(crate::Closure::load(ctx, None, b"return 42").unwrap()));
            assert_eq!(lua.prepare_jit().unwrap(), 1);
            let baseline = lua.jit_stats().code_bytes;
            let source = lua.enter(|ctx| {
                ctx.stash(crate::Closure::load(ctx, None, b"local x=40 return x+2").unwrap())
            });
            lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = failure);
            assert!(matches!(
                lua.prepare_jit(),
                Err(JitError::ResourceLimit("JIT metadata"))
            ));
            let stats = lua.jit_stats();
            assert_eq!(
                (
                    stats.code_bytes,
                    stats.snapshot_bytes,
                    stats.installed_regions,
                    stats.compilation_failures
                ),
                (baseline, 0, 1, 1)
            );
            let peer_executor = lua
                .enter(|ctx| ctx.stash(crate::Executor::start(ctx, ctx.fetch(&peer).into(), ())));
            assert_eq!(lua.execute::<i64>(&peer_executor).unwrap(), 42);
            assert!(lua.jit_stats().native_entries > 0);
            lua.enter(|ctx| {
                let mut manager = ctx.jit().0.borrow_mut();
                manager.memory_failure = backend::Failure::None;
                manager
                    .metadata
                    .0
                    .set_limit(manager.config.max_metadata_bytes);
                manager.metadata.0.fail_after(usize::MAX);
            });
            let mut config = lua.jit_config();
            config.mode = JitMode::Off;
            lua.set_jit_config(config).unwrap();
            let source_executor = lua
                .enter(|ctx| ctx.stash(crate::Executor::start(ctx, ctx.fetch(&source).into(), ())));
            let entries = lua.jit_stats().native_entries;
            assert_eq!(lua.execute::<i64>(&source_executor).unwrap(), 42);
            assert_eq!(lua.jit_stats().native_entries, entries);
            let mut config = lua.jit_config();
            config.mode = JitMode::Auto;
            lua.set_jit_config(config).unwrap();
            lua.clear_jit_cache();
            assert_eq!(lua.prepare_jit().unwrap(), 2);
            let recovered = lua
                .enter(|ctx| ctx.stash(crate::Executor::start(ctx, ctx.fetch(&source).into(), ())));
            assert_eq!(lua.execute::<i64>(&recovered).unwrap(), 42);
            assert!(lua.jit_stats().native_entries > entries);
            drop((peer, source, peer_executor, source_executor, recovered));
            lua.gc_collect();
            lua.gc_collect();
            assert_eq!(
                lua.jit_stats().accounted_jit_bytes,
                lua.jit_stats().bootstrap_bytes
            );
        }
    }

    #[test]
    fn host_ceiling_shrink_retains_live_lease_charges_until_final_drop() {
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
        lua.prepare_jit().unwrap();
        let lease = runtime.lookup(identity).unwrap();
        lua.set_memory_limit(Some(0));
        lua.set_jit_config(JitConfig::default()).unwrap();
        lua.clear_jit_cache();
        drop(closure);
        lua.gc_collect();
        lua.gc_collect();
        assert!(runtime.lookup(identity).is_none());
        let stats = lua.jit_stats();
        assert!(stats.code_bytes > 0 && stats.metadata_bytes > 0);
        assert_eq!(stats.snapshot_bytes, 0);
        assert_eq!(
            stats.accounted_jit_bytes,
            stats.code_bytes + stats.metadata_bytes + stats.bootstrap_bytes
        );
        assert_eq!(
            lua.accounted_memory(),
            lua.total_memory() + stats.accounted_jit_bytes
        );
        assert_executable(&lease);
        drop(lease);
        assert_eq!(
            lua.jit_stats().accounted_jit_bytes,
            lua.jit_stats().bootstrap_bytes
        );
        assert_eq!(
            lua.accounted_memory(),
            lua.total_memory() + lua.jit_stats().bootstrap_bytes
        );
    }

    #[test]
    fn host_charges_reclaim_after_mapping_or_protection_failure() {
        for failure in [backend::Failure::Allocate, backend::Failure::Protect] {
            let mut lua = crate::Lua::empty();
            lua.set_jit_config(JitConfig {
                mode: JitMode::Auto,
                ..Default::default()
            })
            .unwrap();
            let closure =
                lua.enter(|ctx| ctx.stash(crate::Closure::load(ctx, None, b"return 42").unwrap()));
            lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = failure);
            assert!(matches!(lua.prepare_jit(), Err(JitError::Unavailable(_))));
            let stats = lua.jit_stats();
            assert_eq!(
                (
                    stats.code_bytes,
                    stats.snapshot_bytes,
                    stats.host_allocation_refusals
                ),
                (0, 0, 0)
            );
            assert_eq!(
                stats.accounted_jit_bytes,
                stats.metadata_bytes + stats.bootstrap_bytes
            );
            lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = backend::Failure::None);
            lua.clear_jit_cache();
            assert_eq!(lua.prepare_jit().unwrap(), 1);
            let executor = lua.enter(|ctx| {
                ctx.stash(crate::Executor::start(ctx, ctx.fetch(&closure).into(), ()))
            });
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), 42);
            assert!(lua.jit_stats().native_entries > 0);
            drop(executor);
            drop(closure);
            lua.gc_collect();
            lua.gc_collect();
            assert_eq!(
                lua.jit_stats().accounted_jit_bytes,
                lua.jit_stats().bootstrap_bytes
            );
        }
    }

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
            let owners = owner::Shared::strong_count(&lease.code);
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
            assert_eq!(owner::Shared::strong_count(&lease.code), owners);
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
                owner::Shared::strong_count(&lease.code),
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
            assert!(owner::Shared::ptr_eq(&manager.code[&1].code, &lease.code));
            assert_eq!(owner::Shared::strong_count(&lease.code), owner_count);
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
    fn oversized_image_preserves_cached_peer_and_does_not_retry_eviction() {
        let runtime = Runtime::new();
        runtime.0.borrow_mut().configure(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        });
        request(&runtime, 1).unwrap();
        let bytes = runtime.usage();
        runtime.0.borrow_mut().config.max_code_bytes = bytes;
        let source = format!("local n=0 {} return n", "n=n+1 ".repeat(64));
        let snapshot = || {
            crate::Lua::empty().enter(|ctx| {
                let prototype =
                    crate::FunctionPrototype::compile(ctx, "oversized", source.as_bytes()).unwrap();
                ir::Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
            })
        };
        let enqueue = || {
            let mut manager = runtime.0.borrow_mut();
            manager.tracked.entry(2).or_default();
            manager.enqueue(2, true);
            assert_eq!(manager.next_request(), Some(2));
        };
        enqueue();
        let result = runtime.compile(2, snapshot());
        assert!(
            runtime.lookup(1).is_some(),
            "oversized image evicted its peer"
        );
        assert!(matches!(
            result,
            Err(JitError::ResourceLimit("native image size"))
        ));
        assert_eq!(runtime.usage(), bytes);
        {
            let manager = runtime.0.borrow();
            assert_eq!(manager.stats.cache_evictions, 0);
            assert_eq!(manager.stats.cache_eviction_refusals, 0);
            assert_eq!(manager.stats.compilation_failures, 1);
            assert_eq!(manager.tracked[&2].attempts, 1);
            assert!(!manager.code.contains_key(&2));
        }
        assert_executable(&runtime.lookup(1).unwrap());
        runtime.0.borrow_mut().config.max_code_bytes = JitConfig::default().max_code_bytes;
        enqueue();
        runtime.compile(2, snapshot()).unwrap();
        assert!(runtime.usage() > 2 * bytes);
        assert_executable(&runtime.lookup(1).unwrap());
        let prepared = runtime.lookup(2).unwrap();
        let mut slots = vec![abi::Slot::from_value(crate::Value::Nil); prepared.code.registers];
        let mut pc = 0;
        let mut instructions = 0;
        loop {
            let exit = prepared.code.invoke(&mut slots, pc, 64);
            instructions += exit.instructions;
            assert!(instructions < 1024);
            if exit.instructions == 0 {
                break;
            }
            pc = exit.pc as usize;
        }
        assert!(instructions > 64);
        assert_eq!((slots[0].tag, slots[0].bits), (abi::INTEGER, 64));
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
    fn oversized_image_preserves_all_unleased_cache_entries() {
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
            Err(JitError::ResourceLimit("native image size"))
        ));
        assert_eq!(runtime.usage(), 2 * bytes);
        let manager = runtime.0.borrow();
        assert_eq!(manager.stats.cache_evictions, 0);
        assert_eq!(manager.stats.cache_eviction_refusals, 0);
        assert_eq!(manager.stats.compilation_failures, 1);
        assert_eq!(manager.tracked[&3].attempts, 1);
        assert!(manager.code.contains_key(&1));
        assert!(manager.code.contains_key(&2));
        assert!(!manager.code.contains_key(&3));
        drop(manager);
        assert_executable(&runtime.lookup(1).unwrap());
        assert_executable(&runtime.lookup(2).unwrap());
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
        let mut bare: MetadataMap<u64, owner::Shared<backend::Code>> =
            metadata_map(allocator.clone());
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

pub(crate) mod projection;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[cfg(test)]
mod runtime_projection_tests {
    use super::*;
    use crate::{
        closure::{UpValue, UpValueState},
        thread::LuaRegisters,
        Closure, Value,
    };

    fn projected_code(snapshot: &ir::Snapshot, total: resources::MappingCounter) -> backend::Code {
        backend::compile_projected_in(
            snapshot,
            total,
            128 * 1024,
            resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
            work::Limits::from(&JitConfig::default()),
            backend::Failure::None,
        )
        .unwrap()
    }

    #[test]
    fn default_admission_keeps_rejected_projection_disabled() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"_ENV = 42; return _ENV").unwrap();
            let snapshot = ir::Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
            let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
            let code = backend::compile(&snapshot, total.clone(), 128 * 1024).unwrap();
            assert!(!code.projected_upvalues);
            drop(code);
            assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn projected_module_failures_release_all_images_and_preserve_peer() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"_ENV = 42; return _ENV").unwrap();
            let snapshot = ir::Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
            let metadata_ledger = resources::Ledger::new(2 * 1024 * 1024);
            let metadata = resources::BudgetAllocator(metadata_ledger.clone());
            let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
            let limits = work::Limits::from(&JitConfig::default());
            let peer = backend::compile_projected_in(
                &snapshot,
                total.clone(),
                128 * 1024,
                metadata.clone(),
                limits,
                backend::Failure::None,
            )
            .unwrap();
            let snapshot_baseline = snapshot.operations.allocator().0.current();
            let metadata_baseline = metadata_ledger.current();
            let mapped_baseline = total.load(std::sync::atomic::Ordering::Relaxed);
            for failure in [
                backend::Failure::Allocate,
                backend::Failure::Protect,
                backend::Failure::ProtectAfterFirst,
                backend::Failure::RequireReleasedWorkspace(snapshot_baseline),
                backend::Failure::RequireSignatures(snapshot_baseline),
                backend::Failure::RequireRelocationCopy(snapshot_baseline),
            ] {
                let result = backend::compile_projected_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    metadata.clone(),
                    limits,
                    failure,
                );
                if matches!(
                    failure,
                    backend::Failure::Allocate
                        | backend::Failure::Protect
                        | backend::Failure::ProtectAfterFirst
                ) {
                    assert!(result.is_err());
                } else {
                    assert!(result.is_ok());
                }
                drop(result);
                assert_eq!(
                    snapshot.operations.allocator().0.current(),
                    snapshot_baseline
                );
                assert_eq!(metadata_ledger.current(), metadata_baseline);
                assert_eq!(
                    total.load(std::sync::atomic::Ordering::Relaxed),
                    mapped_baseline
                );
                let mut slots = vec![abi::Slot::from_value(Value::Nil); peer.registers];
                assert_eq!(peer.invoke(&mut slots, 0, 1).instructions, 1);
            }
            drop(peer);
            assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            assert_eq!(metadata_ledger.current(), 0);
        });
    }

    #[test]
    fn admitted_scalar_write_and_read_preserve_each_slice_and_closed_cell() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"_ENV = 42; return _ENV").unwrap();
            let snapshot = ir::Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
            let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
            let code = projected_code(&snapshot, total.clone());
            assert!(code.projected_upvalues);
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            closure.set_upvalue(&ctx, 0, cell);
            let runtime = Runtime::new();
            let mut canonical = vec![Value::Nil; code.registers];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                for (index, operation) in snapshot.operations.iter().enumerate() {
                    if matches!(operation, crate::opcode::Operation::Return { .. }) {
                        break;
                    }
                    assert_eq!(*registers.pc, index);
                    assert_eq!(
                        runtime.invoke::<256, false>(&code, ctx, closure, &mut registers, 1),
                        1
                    );
                    assert_eq!(*registers.pc, index + 1);
                    if matches!(operation, crate::opcode::Operation::SetUpValue { .. }) {
                        assert!(matches!(
                            cell.get(),
                            UpValueState::Closed(Value::Integer(42))
                        ));
                    }
                }
                assert!(matches!(registers.stack_frame[0], Value::Integer(42)));
            });
            let stats = runtime.0.borrow().stats;
            assert_eq!(
                (stats.native_upvalue_reads, stats.native_upvalue_writes),
                (1, 1)
            );
            assert_eq!((stats.helper_calls, stats.helper_instructions), (0, 0));
            drop(code);
            assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn both_boundary_modes_preserve_real_entry_pc_counts_and_reference_results() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return _ENV").unwrap();
            let snapshot = ir::Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
            let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
            for projected in [false, true] {
                let code = if projected {
                    projected_code(&snapshot, total.clone())
                } else {
                    backend::compile(&snapshot, total.clone(), 128 * 1024).unwrap()
                };
                assert_eq!(code.projected_upvalues, projected);
                for value in [
                    Value::Integer(41),
                    Value::Number(-0.0),
                    Value::Table(crate::Table::new(&ctx)),
                ] {
                    let cell = UpValue::new(&ctx, UpValueState::Closed(value));
                    closure.set_upvalue(&ctx, 0, cell);
                    let runtime = Runtime::new();
                    let mut canonical = vec![Value::Nil; code.registers];
                    let mut pc = 0;
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                        assert_eq!(
                            runtime.invoke::<256, false>(&code, ctx, closure, &mut registers, 1),
                            1
                        );
                        assert_eq!(*registers.pc, 1);
                        let actual = registers.stack_frame[0];
                        let expected = abi::Slot::from_value(value);
                        let encoded = abi::Slot::from_value(actual);
                        assert_eq!((encoded.tag, encoded.bits), (expected.tag, expected.bits));
                        if let Value::Table(table) = value {
                            let Value::Table(result) = actual else {
                                panic!("reference result lost");
                            };
                            assert_eq!(result, table);
                        }
                    });
                    let stats = runtime.0.borrow().stats;
                    assert_eq!(
                        (
                            stats.native_instructions,
                            stats.helper_calls,
                            stats.helper_instructions,
                            stats.native_upvalue_reads
                        ),
                        (
                            1,
                            u64::from(!projected || matches!(value, Value::Table(_))),
                            u64::from(!projected || matches!(value, Value::Table(_))),
                            1
                        )
                    );
                    assert_eq!(stats.native_upvalue_writes, 0);
                }
                drop(code);
                assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            }
        });
    }
}
