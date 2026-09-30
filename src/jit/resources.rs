use std::{
    alloc::Layout,
    ptr::NonNull,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use allocator_api2::alloc::{AllocError, Allocator, Global};
use ottavino_gc_arena::Collect;

pub(crate) struct Ledger {
    current: AtomicUsize,
    peak: AtomicUsize,
    limit: AtomicUsize,
    refusals: AtomicUsize,
    #[cfg(test)]
    allocations_before_failure: AtomicUsize,
}

impl Ledger {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            limit: AtomicUsize::new(limit),
            refusals: AtomicUsize::new(0),
            #[cfg(test)]
            allocations_before_failure: AtomicUsize::new(usize::MAX),
        })
    }

    pub fn current(&self) -> usize {
        self.current.load(Ordering::Relaxed)
    }
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
    pub fn refusals(&self) -> usize {
        self.refusals.load(Ordering::Relaxed)
    }
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }
    pub fn fits(&self, bytes: usize) -> bool {
        self.current()
            .checked_add(bytes)
            .is_some_and(|usage| usage <= self.limit.load(Ordering::Relaxed))
    }

    fn reserve(&self, bytes: usize) -> Result<(), AllocError> {
        let old = self
            .current
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.limit.load(Ordering::Relaxed))
            })
            .map_err(|_| {
                self.refused();
                AllocError
            })?;
        self.peak.fetch_max(old + bytes, Ordering::Relaxed);
        Ok(())
    }

    fn release(&self, bytes: usize) {
        self.current.fetch_sub(bytes, Ordering::Relaxed);
    }

    fn refused(&self) {
        let _ = self
            .refusals
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
    }
}

#[cfg(test)]
pub(super) fn owned<T: Clone>(values: &[T]) -> allocator_api2::vec::Vec<T, BudgetAllocator> {
    let mut result =
        allocator_api2::vec::Vec::new_in(BudgetAllocator(Ledger::new(2 * 1024 * 1024)));
    result.try_reserve_exact(values.len()).unwrap();
    result.extend_from_slice(values);
    result
}

#[derive(Clone, Collect)]
#[collect(require_static)]
pub(crate) struct BudgetAllocator(pub Arc<Ledger>);

unsafe impl Allocator for BudgetAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.0.reserve(layout.size())?;
        #[cfg(test)]
        let refused = self
            .0
            .allocations_before_failure
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                if remaining == usize::MAX {
                    Some(remaining)
                } else {
                    remaining.checked_sub(1)
                }
            })
            .is_err();
        #[cfg(not(test))]
        let refused = false;
        let result = if refused {
            Err(AllocError)
        } else {
            Global.allocate(layout)
        };
        match result {
            Ok(pointer) => Ok(pointer),
            Err(error) => {
                self.0.release(layout.size());
                self.0.refused();
                Err(error)
            }
        }
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        unsafe { Global.deallocate(pointer, layout) };
        self.0.release(layout.size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use allocator_api2::vec::Vec;

    #[test]
    fn failed_growth_preserves_contents_and_charges_retained_capacity() {
        let ledger = Ledger::new(64);
        let mut values = Vec::new_in(BudgetAllocator(ledger.clone()));
        values.try_reserve_exact(8).unwrap();
        values.extend(0..8u64);
        assert_eq!(ledger.current(), 64);
        assert!(values.try_reserve(1).is_err());
        assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(ledger.current(), 64);
        assert_eq!(ledger.refusals(), 1);
        values.clear();
        assert_eq!(ledger.current(), 64);
        drop(values);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn successful_growth_charges_old_and_new_allocations_until_release() {
        let ledger = Ledger::new(192);
        let mut values = Vec::new_in(BudgetAllocator(ledger.clone()));
        values.try_reserve_exact(8).unwrap();
        values.extend(0..8u64);
        values.try_reserve_exact(8).unwrap();
        assert_eq!(ledger.current(), 128);
        assert_eq!(ledger.peak(), 192);
        assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        drop(values);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn reduced_limits_preserve_live_owners_and_refuse_new_allocations() {
        let ledger = Ledger::new(64);
        let allocator = BudgetAllocator(ledger.clone());
        let layout = Layout::from_size_align(64, 8).unwrap();
        let pointer = allocator.allocate(layout).unwrap();
        ledger.set_limit(1);
        assert!(allocator
            .allocate(Layout::from_size_align(1, 1).unwrap())
            .is_err());
        assert_eq!(ledger.current(), 64);
        unsafe { allocator.deallocate(pointer.cast(), layout) };
        assert_eq!(ledger.current(), 0);
        let pointer = allocator
            .allocate(Layout::from_size_align(1, 1).unwrap())
            .unwrap();
        unsafe { allocator.deallocate(pointer.cast(), Layout::from_size_align(1, 1).unwrap()) };
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn underlying_allocation_failure_releases_reserved_bytes() {
        let ledger = Ledger::new(64);
        ledger
            .allocations_before_failure
            .store(0, Ordering::Relaxed);
        let allocator = BudgetAllocator(ledger.clone());
        assert!(allocator
            .allocate(Layout::from_size_align(64, 8).unwrap())
            .is_err());
        assert_eq!(ledger.current(), 0);
        assert_eq!(ledger.peak(), 64);
        assert_eq!(ledger.refusals(), 1);
    }

    #[test]
    fn partial_snapshot_failure_releases_the_first_owned_vector() {
        let ledger = Ledger::new(4096);
        ledger
            .allocations_before_failure
            .store(1, Ordering::Relaxed);
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let proto =
                crate::FunctionPrototype::compile(ctx, "snapshot-failure", b"return 42").unwrap();
            assert!(!proto.constants.is_empty());
            assert!(matches!(
                super::super::ir::Snapshot::new_in(&proto, 4096, BudgetAllocator(ledger.clone())),
                Err(super::super::JitError::ResourceLimit("prototype snapshot"))
            ));
        });
        assert_eq!(ledger.current(), 0);
        assert!(ledger.peak() > 0);
    }

    #[test]
    fn queue_refusal_does_not_strand_a_registration_or_increment_attempts() {
        let mut manager = super::super::Manager::default();
        manager.config.mode = super::super::JitMode::Auto;
        manager.tracked.try_reserve(1).unwrap();
        manager.tracked.insert(1, super::super::Tracking::default());
        manager.metadata.0.set_limit(manager.metadata.0.current());
        manager.enqueue(1, true);
        assert!(manager.queue.is_empty());
        assert!(!manager.tracked[&1].queued);
        assert_eq!(manager.tracked[&1].attempts, 0);
        assert_eq!(manager.stats.compilation_requests, 0);
        manager.metadata.0.set_limit(4096);
        manager.enqueue(1, true);
        assert_eq!(manager.next_request(), Some(1));
        assert_eq!(manager.tracked[&1].attempts, 1);
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn installation_refusal_reclaims_generated_mappings() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let proto =
                crate::FunctionPrototype::compile(ctx, "install-refusal", b"return 42").unwrap();
            super::super::ir::Snapshot::new(&proto, 4096, 4096).unwrap()
        });
        let runtime = super::super::Runtime::new();
        {
            let mut manager = runtime.0.borrow_mut();
            manager.config.mode = super::super::JitMode::Auto;
            manager.tracked.try_reserve(1).unwrap();
            manager.tracked.insert(1, super::super::Tracking::default());
            manager
                .metadata
                .0
                .allocations_before_failure
                .store(2, Ordering::Relaxed);
        }
        assert!(matches!(
            runtime.compile(1, snapshot),
            Err(super::super::JitError::ResourceLimit("JIT metadata"))
        ));
        assert_eq!(runtime.usage(), 0);
        let manager = runtime.0.borrow();
        assert!(manager.code.is_empty());
        assert_eq!(manager.stats.installed_regions, 0);
        assert_eq!(manager.stats.compilation_failures, 1);
        assert_eq!(manager.metadata.0.refusals(), 1);
    }

    #[test]
    fn reservation_overflow_is_refused_without_changing_usage() {
        let ledger = Ledger::new(usize::MAX);
        ledger.reserve(usize::MAX).unwrap();
        assert!(ledger.reserve(1).is_err());
        assert_eq!(ledger.current(), usize::MAX);
        ledger.release(usize::MAX);
        assert_eq!(ledger.current(), 0);
    }
}
