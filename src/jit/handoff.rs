use std::sync::{Mutex, MutexGuard, PoisonError};

use allocator_api2::alloc::AllocError;

use super::{atomic_owner::AtomicShared, resources::BudgetAllocator};

pub(super) struct Handoff<T>(AtomicShared<Mutex<Option<T>>>);

impl<T> Handoff<T> {
    pub fn try_new(value: T, allocator: BudgetAllocator) -> Result<Self, AllocError> {
        AtomicShared::try_new(Mutex::new(Some(value)), allocator).map(Self)
    }

    pub fn with_mut<R>(&self, operation: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mut value = self.borrow_mut();
        value.as_mut().map(operation)
    }

    pub fn borrow_mut(&self) -> MutexGuard<'_, Option<T>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn take(&self) -> Option<T> {
        self.borrow_mut().take()
    }

    #[cfg(test)]
    pub fn allocation_bytes() -> usize {
        AtomicShared::<Mutex<Option<T>>>::allocation_bytes()
    }
}

impl<T> Clone for Handoff<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use super::*;
    use crate::jit::resources::Ledger;

    struct Probe(Arc<AtomicUsize>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn exact_charge_last_slot_release_and_single_transfer() {
        let bytes = Handoff::<Probe>::allocation_bytes();
        let host = Ledger::new(bytes);
        let metadata = Ledger::child(bytes, host.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let slot =
            Handoff::try_new(Probe(drops.clone()), BudgetAllocator(metadata.clone())).unwrap();
        let provider = slot.clone();
        assert_eq!((metadata.current(), host.current()), (bytes, bytes));
        let image = slot.take().unwrap();
        assert!(provider.take().is_none());
        assert!(provider
            .with_mut(|_| panic!("detached image accessed"))
            .is_none());
        drop(slot);
        assert_eq!((metadata.current(), host.current()), (bytes, bytes));
        drop(provider);
        assert_eq!(
            (
                metadata.current(),
                host.current(),
                drops.load(Ordering::Relaxed)
            ),
            (0, 0, 0)
        );
        drop(image);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn admission_and_allocation_refusal_drop_input_and_allow_retry() {
        let bytes = Handoff::<Probe>::allocation_bytes();
        for cause in 0..3 {
            let host = Ledger::new(if cause == 1 { bytes - 1 } else { bytes });
            let metadata = Ledger::child(if cause == 0 { bytes - 1 } else { bytes }, host.clone());
            if cause == 2 {
                metadata.fail_after(0);
            }
            let drops = Arc::new(AtomicUsize::new(0));
            assert!(
                Handoff::try_new(Probe(drops.clone()), BudgetAllocator(metadata.clone())).is_err()
            );
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    drops.load(Ordering::Relaxed)
                ),
                (0, 0, 1)
            );
            assert_eq!(metadata.refusals(), 1);
            metadata.set_limit(bytes);
            host.set_limit(bytes);
            metadata.fail_after(usize::MAX);
            let retry =
                Handoff::try_new(Probe(drops.clone()), BudgetAllocator(metadata.clone())).unwrap();
            drop(retry);
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    drops.load(Ordering::Relaxed)
                ),
                (0, 0, 2)
            );
        }
    }

    #[test]
    fn unclaimed_value_drops_only_after_last_provider_owner() {
        let ledger = Ledger::new(4096);
        let drops = Arc::new(AtomicUsize::new(0));
        let slot = Handoff::try_new(Probe(drops.clone()), BudgetAllocator(ledger.clone())).unwrap();
        let provider = slot.clone();
        drop(slot);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), Handoff::<Probe>::allocation_bytes());
        drop(provider);
        assert_eq!((ledger.current(), drops.load(Ordering::Relaxed)), (0, 1));
    }

    #[test]
    fn mutation_error_and_poison_keep_value_owned_and_transferable() {
        let ledger = Ledger::new(4096);
        let slot = Handoff::try_new(0usize, BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!(
            slot.with_mut(|value| {
                *value = 41;
                Err::<(), _>(7)
            }),
            Some(Err(7))
        );
        assert!(catch_unwind(AssertUnwindSafe(|| {
            slot.with_mut(|value| {
                *value += 1;
                panic!("provider panic")
            });
        }))
        .is_err());
        assert_eq!(slot.take(), Some(42));
        assert!(slot.take().is_none());
        drop(slot);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn provider_worker_transfers_borrowed_aligned_value_without_copying() {
        #[repr(align(256))]
        struct Aligned<'a>(&'a AtomicUsize);
        let observed = AtomicUsize::new(0);
        let ledger = Ledger::new(4096);
        let slot = Handoff::try_new(Aligned(&observed), BudgetAllocator(ledger.clone())).unwrap();
        std::thread::scope(|scope| {
            let provider = slot.clone();
            scope
                .spawn(move || {
                    provider.with_mut(|value| {
                        assert_eq!((value as *const _ as usize) % 256, 0);
                        value.0.store(42, Ordering::Relaxed);
                    })
                })
                .join()
                .unwrap();
        });
        let value = slot.take().unwrap();
        drop(slot);
        assert_eq!(ledger.current(), 0);
        assert_eq!(value.0.load(Ordering::Relaxed), 42);
    }
}
