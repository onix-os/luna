use std::{
    alloc::Layout,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use allocator_api2::alloc::{AllocError, Allocator, Global};
use ottavino_gc_arena::Collect;
#[cfg(test)]
use std::sync::Arc;

use super::global_owner::GlobalShared;

pub(crate) type LedgerRef = GlobalShared<Ledger, HeaderCharge>;

pub(crate) struct HeaderCharge {
    root: Option<LedgerRef>,
    bytes: usize,
}

impl HeaderCharge {
    pub(super) fn try_new(ledger: LedgerRef, bytes: usize) -> Result<Self, AllocError> {
        let root = MappingCounter::new(ledger).0;
        let old = root
            .current
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(bytes).filter(|next| {
                    !root.enforce_bootstrap || *next <= root.limit.load(Ordering::Relaxed)
                })
            })
            .map_err(|_| {
                root.refused();
                AllocError
            })?;
        root.bootstrap.fetch_add(bytes, Ordering::Relaxed);
        root.accounted_peak
            .fetch_max(old + bytes, Ordering::Relaxed);
        Ok(Self {
            root: Some(root),
            bytes,
        })
    }
}

impl Drop for HeaderCharge {
    fn drop(&mut self) {
        if let Some(root) = &self.root {
            root.bootstrap.fetch_sub(self.bytes, Ordering::Relaxed);
            root.current.fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}

pub(crate) struct Ledger {
    parent: Option<LedgerRef>,
    bootstrap: AtomicUsize,
    accounted_peak: AtomicUsize,
    enforce_bootstrap: bool,
    mapped: AtomicUsize,
    current: AtomicUsize,
    peak: AtomicUsize,
    limit: AtomicUsize,
    refusals: AtomicUsize,
    #[cfg(test)]
    allocations_before_failure: AtomicUsize,
}

impl Ledger {
    #[cfg(test)]
    pub(super) fn fail_after(&self, allocations: usize) {
        self.allocations_before_failure
            .store(allocations, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn new(limit: usize) -> LedgerRef {
        Self::with_parent(limit, None, false)
    }

    pub fn host(limit: usize) -> LedgerRef {
        Self::with_parent(limit, None, true)
    }

    pub fn try_host(limit: usize) -> Result<LedgerRef, AllocError> {
        Self::try_with_parent(limit, None, true)
    }

    pub fn child(limit: usize, parent: LedgerRef) -> LedgerRef {
        Self::with_parent(limit, Some(parent), false)
    }

    pub fn try_child(limit: usize, parent: LedgerRef) -> Result<LedgerRef, AllocError> {
        Self::try_with_parent(limit, Some(parent), false)
    }

    fn with_parent(limit: usize, parent: Option<LedgerRef>, enforce: bool) -> LedgerRef {
        let result = match parent {
            Some(parent) => Self::try_child(limit, parent),
            None if enforce => Self::try_host(limit),
            None => Self::try_with_parent(limit, None, false),
        };
        result.unwrap_or_else(|_| std::alloc::handle_alloc_error(LedgerRef::allocation_layout()))
    }

    fn try_with_parent(
        limit: usize,
        parent: Option<LedgerRef>,
        enforce: bool,
    ) -> Result<LedgerRef, AllocError> {
        let bytes = LedgerRef::allocation_bytes();
        if parent.is_none() && enforce && bytes > limit {
            return Err(AllocError);
        }
        let charge = if let Some(parent) = &parent {
            HeaderCharge::try_new(parent.clone(), bytes)?
        } else {
            HeaderCharge {
                root: None,
                bytes: 0,
            }
        };
        let root_bytes = if parent.is_none() { bytes } else { 0 };
        GlobalShared::try_new(
            Self {
                parent,
                bootstrap: AtomicUsize::new(root_bytes),
                accounted_peak: AtomicUsize::new(root_bytes),
                enforce_bootstrap: enforce,
                mapped: AtomicUsize::new(0),
                current: AtomicUsize::new(root_bytes),
                peak: AtomicUsize::new(0),
                limit: AtomicUsize::new(limit),
                refusals: AtomicUsize::new(0),
                #[cfg(test)]
                allocations_before_failure: AtomicUsize::new(usize::MAX),
            },
            charge,
        )
    }

    pub fn current(&self) -> usize {
        self.current
            .load(Ordering::Relaxed)
            .saturating_sub(self.bootstrap_bytes())
    }
    pub fn accounted(&self) -> usize {
        self.current.load(Ordering::Relaxed)
    }
    pub fn bootstrap_bytes(&self) -> usize {
        self.bootstrap.load(Ordering::Relaxed)
    }
    pub fn accounted_peak(&self) -> usize {
        self.accounted_peak.load(Ordering::Relaxed)
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
        (if self.enforce_bootstrap {
            self.accounted()
        } else {
            self.current()
        })
        .checked_add(bytes)
        .is_some_and(|usage| usage <= self.limit.load(Ordering::Relaxed))
    }

    fn reserve(&self, bytes: usize) -> Result<(), AllocError> {
        let old = self
            .current
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(bytes).filter(|next| {
                    next.saturating_sub(if self.enforce_bootstrap {
                        0
                    } else {
                        self.bootstrap_bytes()
                    }) <= self.limit.load(Ordering::Relaxed)
                })
            })
            .map_err(|_| {
                self.refused();
                AllocError
            })?;
        if let Some(parent) = &self.parent {
            if parent.reserve(bytes).is_err() {
                self.current.fetch_sub(bytes, Ordering::Relaxed);
                self.refused();
                return Err(AllocError);
            }
        }
        self.peak.fetch_max(
            (old + bytes).saturating_sub(self.bootstrap_bytes()),
            Ordering::Relaxed,
        );
        self.accounted_peak
            .fetch_max(old + bytes, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn release(&self, bytes: usize) {
        self.current.fetch_sub(bytes, Ordering::Relaxed);
        if let Some(parent) = &self.parent {
            parent.release(bytes);
        }
    }

    pub(super) fn reserve_external(&self, bytes: usize) -> Result<(), AllocError> {
        match &self.parent {
            Some(parent) => parent.reserve(bytes),
            None => Ok(()),
        }
    }

    pub(super) fn release_external(&self, bytes: usize) {
        if let Some(parent) = &self.parent {
            parent.release(bytes);
        }
    }

    fn refused(&self) {
        let _ = self
            .refusals
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
    }
}

#[derive(Clone)]
pub(crate) struct MappingCounter(LedgerRef);

impl MappingCounter {
    pub fn new(mut ledger: LedgerRef) -> Self {
        while let Some(parent) = &ledger.parent {
            ledger = parent.clone();
        }
        Self(ledger)
    }
}

impl Deref for MappingCounter {
    type Target = AtomicUsize;

    fn deref(&self) -> &AtomicUsize {
        &self.0.mapped
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    #[test]
    fn exact_root_and_nested_header_lifetimes_leave_only_live_owners_charged() {
        let bytes = LedgerRef::allocation_bytes();
        let host = Ledger::try_host(3 * bytes).unwrap();
        assert_eq!(
            (host.accounted(), host.bootstrap_bytes(), host.current()),
            (bytes, bytes, 0)
        );
        let child = Ledger::try_child(8, host.clone()).unwrap();
        let leaf = Ledger::try_child(8, child.clone()).unwrap();
        assert_eq!(
            (host.accounted(), host.bootstrap_bytes()),
            (3 * bytes, 3 * bytes)
        );
        assert!(Ledger::try_child(8, host.clone()).is_err());
        assert_eq!(host.refusals(), 1);
        drop(child);
        assert_eq!(host.bootstrap_bytes(), 3 * bytes);
        drop(leaf);
        assert_eq!(
            (host.accounted(), host.bootstrap_bytes(), host.current()),
            (bytes, bytes, 0)
        );
        assert_eq!(host.accounted_peak(), 3 * bytes);
    }

    #[test]
    fn one_byte_short_root_child_and_underlying_refusal_retain_no_new_charge() {
        let bytes = LedgerRef::allocation_bytes();
        assert!(Ledger::try_host(bytes - 1).is_err());
        let host = Ledger::try_host(2 * bytes - 1).unwrap();
        assert!(Ledger::try_child(0, host.clone()).is_err());
        assert_eq!(host.accounted(), bytes);
        host.set_limit(2 * bytes);
        super::super::global_owner::with_allocation_denied(|| {
            assert!(Ledger::try_child(0, host.clone()).is_err());
        });
        assert_eq!((host.accounted(), host.bootstrap_bytes()), (bytes, bytes));
        let child = Ledger::try_child(0, host.clone()).unwrap();
        assert_eq!(host.accounted(), 2 * bytes);
        drop(child);
        assert_eq!(host.accounted(), bytes);
    }

    #[test]
    fn concurrent_header_and_payload_admission_share_one_atomic_ceiling() {
        let bytes = LedgerRef::allocation_bytes();
        let host = Ledger::try_host(2 * bytes).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut workers = std::vec::Vec::new();
        for header in [true, false] {
            let host = host.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                if header {
                    let child = Ledger::try_child(0, host);
                    barrier.wait();
                    child.is_ok()
                } else {
                    let allocator = BudgetAllocator(host);
                    let layout = Layout::from_size_align(bytes, 8).unwrap();
                    let allocation = allocator.allocate(layout);
                    barrier.wait();
                    if let Ok(pointer) = allocation {
                        unsafe { allocator.deallocate(pointer.cast(), layout) };
                        true
                    } else {
                        false
                    }
                }
            }));
        }
        let admitted = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(admitted, 1);
        assert_eq!(
            (host.accounted(), host.bootstrap_bytes(), host.refusals()),
            (bytes, bytes, 1)
        );
        assert_eq!(host.accounted_peak(), 2 * bytes);
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;

    #[test]
    fn nested_children_and_clones_share_root_without_allocation() {
        let host = Ledger::new(0);
        let child = Ledger::child(0, host.clone());
        let leaf = Ledger::child(0, child.clone());
        host.fail_after(0);
        let first = MappingCounter::new(host.clone());
        let second = MappingCounter::new(leaf.clone());
        let third = second.clone();
        assert!(LedgerRef::ptr_eq(&first.0, &host));
        assert!(LedgerRef::ptr_eq(&first.0, &second.0));
        assert_eq!(first.fetch_add(4096, Ordering::Relaxed), 0);
        assert_eq!(third.load(Ordering::Relaxed), 4096);
        assert_eq!(second.fetch_sub(4096, Ordering::Relaxed), 4096);
        assert_eq!(first.load(Ordering::Relaxed), 0);
        for ledger in [&host, &child, &leaf] {
            assert_eq!(
                (ledger.current(), ledger.peak(), ledger.refusals()),
                (0, 0, 0)
            );
        }
    }

    #[test]
    fn final_counter_keeps_root_alive_after_all_allocator_handles_drop() {
        let host = Ledger::new(4096);
        let child = Ledger::child(4096, host.clone());
        let counter = MappingCounter::new(child.clone());
        let peer = counter.clone();
        counter.store(4096, Ordering::Relaxed);
        drop((host, child, counter));
        assert_eq!(LedgerRef::strong_count(&peer.0), 1);
        assert_eq!(peer.fetch_sub(4096, Ordering::Relaxed), 4096);
        drop(peer);
    }

    #[test]
    fn independent_roots_keep_mapping_and_host_quotas_distinct() {
        let first = Ledger::new(128);
        let second = Ledger::new(128);
        let counter = MappingCounter::new(first.clone());
        let other = MappingCounter::new(second.clone());
        let allocator = BudgetAllocator(first.clone());
        let layout = Layout::from_size_align(64, 8).unwrap();
        let allocation = allocator.allocate(layout).unwrap();
        counter.store(4096, Ordering::Relaxed);
        assert_eq!((first.current(), other.load(Ordering::Relaxed)), (64, 0));
        assert!(!first.fits(65));
        unsafe { allocator.deallocate(allocation.cast(), layout) };
        assert_eq!(
            (first.current(), counter.load(Ordering::Relaxed)),
            (0, 4096)
        );
        counter.store(0, Ordering::Relaxed);
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
pub(crate) struct BudgetAllocator(pub LedgerRef);

#[derive(Default, Collect)]
#[collect(require_static)]
pub(crate) struct Compactor {
    skipped: u8,
}

#[derive(Clone, Copy)]
pub(crate) enum Compaction {
    Unchanged,
    Compacted(usize),
    Refused,
}

impl Compactor {
    pub fn needed(&mut self, len: usize, capacity: usize) -> bool {
        if len == 0 || capacity < 64 || len > capacity / 4 {
            if self.skipped != 0 {
                self.skipped = 0;
            }
            return len == 0 && capacity != 0;
        }
        true
    }

    fn ready(&mut self, len: usize, capacity: usize) -> bool {
        if !self.needed(len, capacity) {
            return false;
        }
        if len == 0 {
            return true;
        }
        if self.skipped != 0 {
            self.skipped -= 1;
            return false;
        }
        true
    }

    fn refused(&mut self) -> Compaction {
        self.skipped = 8;
        Compaction::Refused
    }

    pub fn map<K: Eq + std::hash::Hash, V>(
        &mut self,
        values: &mut super::MetadataMap<K, V>,
    ) -> Compaction {
        if !self.ready(values.len(), values.capacity()) {
            return Compaction::Unchanged;
        }
        let allocator = values.allocator().clone();
        let before = allocator.0.current();
        let mut replacement =
            hashbrown::HashMap::with_hasher_in(values.hasher().clone(), allocator.clone());
        if replacement.try_reserve(values.len()).is_err() {
            return self.refused();
        }
        replacement.extend(values.drain());
        *values = replacement;
        Compaction::Compacted(before.saturating_sub(allocator.0.current()))
    }

    pub fn vector<T>(
        &mut self,
        values: &mut allocator_api2::vec::Vec<T, BudgetAllocator>,
    ) -> Compaction {
        if std::mem::size_of::<T>() == 0 {
            self.skipped = 0;
            return Compaction::Unchanged;
        }
        if !self.ready(values.len(), values.capacity()) {
            return Compaction::Unchanged;
        }
        let allocator = values.allocator().clone();
        let before = allocator.0.current();
        let mut replacement = allocator_api2::vec::Vec::new_in(allocator.clone());
        if replacement.try_reserve_exact(values.len()).is_err() {
            return self.refused();
        }
        replacement.append(values);
        *values = replacement;
        Compaction::Compacted(before.saturating_sub(allocator.0.current()))
    }
}

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
    fn children_share_parent_capacity_without_losing_individual_limits() {
        let host = Ledger::new(16);
        let left = Ledger::child(64, host.clone());
        let right = Ledger::child(64, host.clone());
        let mut first = Vec::<u8, _>::new_in(BudgetAllocator(left.clone()));
        let mut second = Vec::<u8, _>::new_in(BudgetAllocator(right.clone()));
        first.try_reserve_exact(9).unwrap();
        assert!(second.try_reserve_exact(8).is_err());
        assert_eq!((left.current(), right.current(), host.current()), (9, 0, 9));
        assert_eq!((right.refusals(), host.refusals()), (1, 1));
        second.try_reserve_exact(7).unwrap();
        assert_eq!((host.current(), host.peak()), (16, 16));
        let tiny = Ledger::child(2, host.clone());
        assert!(Vec::<u8, _>::new_in(BudgetAllocator(tiny.clone()))
            .try_reserve_exact(3)
            .is_err());
        assert_eq!(
            (tiny.refusals(), host.refusals(), host.current()),
            (1, 1, 16)
        );
        drop(first);
        assert_eq!(host.current(), 7);
        drop(second);
        assert_eq!((left.current(), right.current(), host.current()), (0, 0, 0));
    }

    #[test]
    fn parent_reservation_rolls_back_on_underlying_allocation_failure() {
        let host = Ledger::new(64);
        let child = Ledger::child(64, host.clone());
        child.fail_after(0);
        assert!(Vec::<u8, _>::new_in(BudgetAllocator(child.clone()))
            .try_reserve_exact(8)
            .is_err());
        assert_eq!((child.current(), host.current()), (0, 0));
        assert_eq!((child.peak(), host.peak()), (8, 8));
        assert_eq!((child.refusals(), host.refusals()), (1, 0));
    }

    #[test]
    fn parent_growth_charges_old_and_new_and_lowered_limit_denies_growth() {
        let host = Ledger::new(32);
        let child = Ledger::child(64, host.clone());
        let mut values = Vec::<u8, _>::new_in(BudgetAllocator(child.clone()));
        values.try_reserve_exact(16).unwrap();
        values.resize(16, 42);
        assert!(values.try_reserve_exact(1).is_err());
        assert_eq!(
            (child.current(), host.current(), values.as_slice()),
            (16, 16, &[42; 16][..])
        );
        host.set_limit(15);
        assert!(Vec::<u8, _>::new_in(BudgetAllocator(child.clone()))
            .try_reserve_exact(1)
            .is_err());
        assert_eq!((child.current(), host.current()), (16, 16));
        host.set_limit(33);
        values.try_reserve_exact(1).unwrap();
        assert_eq!((child.current(), host.current(), host.peak()), (17, 17, 33));
        drop(values);
        assert_eq!((child.current(), host.current()), (0, 0));
    }

    #[test]
    fn external_pages_share_parent_but_not_child_metadata_quota() {
        let host = Ledger::new(24);
        let child = Ledger::child(4, host.clone());
        let mut values = Vec::<u8, _>::new_in(BudgetAllocator(child.clone()));
        values.try_reserve_exact(4).unwrap();
        child.reserve_external(20).unwrap();
        assert_eq!((child.current(), host.current()), (4, 24));
        assert!(child.reserve_external(1).is_err());
        assert_eq!(
            (child.current(), host.current(), host.refusals()),
            (4, 24, 1)
        );
        child.release_external(20);
        assert_eq!(host.current(), 4);
        drop(values);
        assert_eq!(host.current(), 0);
    }

    #[test]
    fn concurrent_children_cannot_overbook_parent_capacity() {
        let host = Ledger::new(64);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut threads = std::vec::Vec::new();
        for _ in 0..2 {
            let child = Ledger::child(128, host.clone());
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let mut values = Vec::<u8, _>::new_in(BudgetAllocator(child));
                let admitted = values.try_reserve_exact(40).is_ok();
                barrier.wait();
                (admitted, values)
            }));
        }
        let results: std::vec::Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|(admitted, _)| *admitted).count(), 1);
        assert_eq!((host.current(), host.peak(), host.refusals()), (40, 40, 1));
        drop(results);
        assert_eq!(host.current(), 0);
    }

    #[test]
    fn compaction_moves_map_owners_and_charges_old_and_new_capacity() {
        let ledger = Ledger::new(1024 * 1024);
        let mut values = super::super::metadata_map(BudgetAllocator(ledger.clone()));
        values.try_reserve(128).unwrap();
        let owner = std::rc::Rc::new(42);
        let keys = [
            0u64,
            1,
            127,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            i64::MAX as u64,
            1 << 63,
            u64::MAX,
        ];
        for key in keys {
            values.insert(key, owner.clone());
        }
        let hashes = keys.map(|key| std::hash::BuildHasher::hash_one(values.hasher(), key));
        let capacity = values.capacity();
        let before = ledger.current();
        let mut compactor = Compactor::default();
        let Compaction::Compacted(bytes) = compactor.map(&mut values) else {
            panic!("map was not compacted")
        };
        let after = ledger.current();
        assert!(values.capacity() < capacity);
        assert_eq!(bytes, before - after);
        assert_eq!(ledger.peak(), before + after);
        assert_eq!(std::rc::Rc::strong_count(&owner), 9);
        assert_eq!(
            keys.map(|key| std::hash::BuildHasher::hash_one(values.hasher(), key)),
            hashes
        );
        for key in keys {
            assert!(std::rc::Rc::ptr_eq(&values[&key], &owner));
        }
        assert!(matches!(compactor.map(&mut values), Compaction::Unchanged));
        drop(values);
        assert_eq!(ledger.current(), 0);
        assert_eq!(std::rc::Rc::strong_count(&owner), 1);
    }

    #[test]
    fn compaction_quota_failure_keeps_map_unchanged_and_backs_off() {
        let ledger = Ledger::new(1024 * 1024);
        let mut values = super::super::metadata_map(BudgetAllocator(ledger.clone()));
        values.try_reserve(128).unwrap();
        for key in 0..8u64 {
            values.insert(key, key * 2);
        }
        let capacity = values.capacity();
        let before = ledger.current();
        ledger.set_limit(before);
        let mut compactor = Compactor::default();
        assert!(matches!(compactor.map(&mut values), Compaction::Refused));
        assert_eq!(ledger.current(), before);
        assert_eq!(ledger.peak(), before);
        assert_eq!(values.capacity(), capacity);
        assert_eq!(ledger.refusals(), 1);
        ledger.set_limit(1024 * 1024);
        for _ in 0..32 {
            assert!(compactor.needed(values.len(), values.capacity()));
        }
        for _ in 0..8 {
            assert!(matches!(compactor.map(&mut values), Compaction::Unchanged));
        }
        assert_eq!(ledger.refusals(), 1);
        assert!(matches!(
            compactor.map(&mut values),
            Compaction::Compacted(_)
        ));
        for key in 0..8 {
            assert_eq!(values[&key], key * 2);
        }
        assert!(ledger.current() < before);
    }

    #[test]
    fn compaction_map_underlying_failure_and_empty_release_preserve_accounting() {
        let ledger = Ledger::new(1024 * 1024);
        let mut values = super::super::metadata_map(BudgetAllocator(ledger.clone()));
        values.try_reserve(128).unwrap();
        for key in 0..8u64 {
            values.insert(key, key * 2);
        }
        let capacity = values.capacity();
        let before = ledger.current();
        ledger
            .allocations_before_failure
            .store(0, Ordering::Relaxed);
        let mut compactor = Compactor::default();
        assert!(matches!(compactor.map(&mut values), Compaction::Refused));
        assert_eq!(ledger.current(), before);
        assert!(ledger.peak() > before);
        assert_eq!(values.capacity(), capacity);
        for key in 0..8 {
            assert_eq!(values[&key], key * 2);
        }
        values.clear();
        assert!(
            matches!(compactor.map(&mut values), Compaction::Compacted(bytes) if bytes == before)
        );
        assert_eq!(values.capacity(), 0);
        assert_eq!(ledger.current(), 0);
        assert_eq!(ledger.refusals(), 1);
        assert!(matches!(compactor.map(&mut values), Compaction::Unchanged));
    }

    #[test]
    fn compaction_vector_quota_failure_and_dense_reset_preserve_order() {
        let ledger = Ledger::new(1024 * 1024);
        let mut values = Vec::new_in(BudgetAllocator(ledger.clone()));
        values.try_reserve_exact(128).unwrap();
        values.extend(0..8u64);
        let before = ledger.current();
        ledger.set_limit(before);
        let mut compactor = Compactor::default();
        assert!(matches!(compactor.vector(&mut values), Compaction::Refused));
        assert_eq!(values.capacity(), 128);
        assert_eq!(ledger.current(), before);
        assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        values.resize(33, 7);
        assert!(matches!(
            compactor.vector(&mut values),
            Compaction::Unchanged
        ));
        values.truncate(8);
        ledger.set_limit(1024 * 1024);
        assert!(matches!(
            compactor.vector(&mut values),
            Compaction::Compacted(_)
        ));
        assert_eq!(values.capacity(), 8);
        assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn compaction_underlying_failure_preserves_vector_order_and_retries() {
        let ledger = Ledger::new(1024 * 1024);
        let mut values = Vec::new_in(BudgetAllocator(ledger.clone()));
        values.try_reserve_exact(128).unwrap();
        values.extend(0..8u64);
        let before = ledger.current();
        ledger
            .allocations_before_failure
            .store(0, Ordering::Relaxed);
        let mut compactor = Compactor::default();
        assert!(matches!(compactor.vector(&mut values), Compaction::Refused));
        assert_eq!(values.capacity(), 128);
        assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(ledger.current(), before);
        assert_eq!(ledger.peak(), before + 64);
        ledger
            .allocations_before_failure
            .store(usize::MAX, Ordering::Relaxed);
        for _ in 0..8 {
            assert!(matches!(
                compactor.vector(&mut values),
                Compaction::Unchanged
            ));
        }
        let Compaction::Compacted(bytes) = compactor.vector(&mut values) else {
            panic!("vector was not compacted")
        };
        assert_eq!(bytes, before - 64);
        assert_eq!(values.capacity(), 8);
        assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(ledger.current(), 64);
        values.clear();
        assert!(matches!(
            compactor.vector(&mut values),
            Compaction::Compacted(64)
        ));
        assert_eq!(values.capacity(), 0);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn compaction_preserves_small_dense_and_zero_sized_storage() {
        let ledger = Ledger::new(1024 * 1024);
        let mut values = Vec::new_in(BudgetAllocator(ledger.clone()));
        values.try_reserve_exact(63).unwrap();
        values.push(1u64);
        let mut compactor = Compactor::default();
        assert!(matches!(
            compactor.vector(&mut values),
            Compaction::Unchanged
        ));
        values.try_reserve_exact(63).unwrap();
        let capacity = values.capacity();
        values.resize(capacity / 4 + 1, 7);
        assert!(matches!(
            compactor.vector(&mut values),
            Compaction::Unchanged
        ));
        values.truncate(capacity / 4);
        assert!(matches!(
            compactor.vector(&mut values),
            Compaction::Compacted(_)
        ));
        let mut zero = Vec::new_in(BudgetAllocator(ledger));
        zero.resize(3, ());
        assert!(matches!(compactor.vector(&mut zero), Compaction::Unchanged));
        assert_eq!(zero.len(), 3);
    }

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
        let payload = usize::MAX - ledger.bootstrap_bytes();
        ledger.reserve(payload).unwrap();
        assert!(ledger.reserve(1).is_err());
        assert_eq!(ledger.current(), payload);
        assert_eq!(ledger.accounted(), usize::MAX);
        ledger.release(payload);
        assert_eq!(ledger.current(), 0);
    }
}
