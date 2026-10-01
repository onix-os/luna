use std::{
    marker::PhantomData,
    ops::Deref,
    ptr::NonNull,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[cfg(test)]
use allocator_api2::alloc::Layout;
use allocator_api2::{alloc::AllocError, boxed::Box};

use super::{owner::Allocation, resources::BudgetAllocator};

struct Inner<T> {
    strong: AtomicUsize,
    allocator: BudgetAllocator,
    value: T,
}

pub(super) struct AtomicShared<T> {
    pointer: NonNull<Inner<T>>,
    marker: PhantomData<Arc<T>>,
}

unsafe impl<T: Send + Sync> Send for AtomicShared<T> {}
unsafe impl<T: Send + Sync> Sync for AtomicShared<T> {}

impl<T> AtomicShared<T> {
    pub fn try_new(value: T, allocator: BudgetAllocator) -> Result<Self, AllocError> {
        let inner = Box::try_new_in(
            Inner {
                strong: AtomicUsize::new(1),
                allocator: allocator.clone(),
                value,
            },
            allocator,
        )?;
        let (pointer, allocator) = Box::into_raw_with_allocator(inner);
        drop(allocator);
        Ok(Self {
            pointer: NonNull::new(pointer).unwrap(),
            marker: PhantomData,
        })
    }

    #[cfg(test)]
    pub fn allocation_bytes() -> usize {
        Layout::new::<Inner<T>>().size()
    }
}

impl<T> Clone for AtomicShared<T> {
    fn clone(&self) -> Self {
        unsafe { self.pointer.as_ref() }
            .strong
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |strong| {
                strong.checked_add(1)
            })
            .expect("JIT atomic owner count overflow");
        Self {
            pointer: self.pointer,
            marker: PhantomData,
        }
    }
}

impl<T> Deref for AtomicShared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &unsafe { self.pointer.as_ref() }.value
    }
}

impl<T> Drop for AtomicShared<T> {
    fn drop(&mut self) {
        let inner = unsafe { self.pointer.as_ref() };
        if inner.strong.fetch_sub(1, Ordering::AcqRel) == 1 {
            let allocation = Allocation {
                pointer: self.pointer,
                allocator: inner.allocator.clone(),
            };
            unsafe { std::ptr::drop_in_place(self.pointer.as_ptr()) };
            drop(allocation);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        sync::{atomic::AtomicBool, Barrier},
        thread,
    };

    use super::*;
    use crate::jit::resources::Ledger;

    #[test]
    fn concurrent_last_owner_observes_prior_mutation_and_drops_once() {
        struct Probe {
            written: AtomicBool,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                assert!(self.written.load(Ordering::Relaxed));
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let ledger = Ledger::new(4096);
        let owner = AtomicShared::try_new(
            Probe {
                written: AtomicBool::new(false),
                dropped: dropped.clone(),
            },
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        let peer = owner.clone();
        let barrier = Arc::new(Barrier::new(2));
        let gate = barrier.clone();
        let worker = thread::spawn(move || {
            for _ in 0..64 {
                let clone = peer.clone();
                assert_eq!(clone.dropped.load(Ordering::Relaxed), 0);
            }
            gate.wait();
            drop(peer);
        });
        owner.written.store(true, Ordering::Relaxed);
        barrier.wait();
        drop(owner);
        worker.join().unwrap();
        assert_eq!((dropped.load(Ordering::Relaxed), ledger.current()), (1, 0));
    }

    #[test]
    fn final_drop_acquires_writes_from_other_owners_without_external_sync() {
        struct Probe {
            fields: [std::cell::UnsafeCell<usize>; 4],
            dropped: Arc<AtomicUsize>,
        }
        unsafe impl Sync for Probe {}
        impl Drop for Probe {
            fn drop(&mut self) {
                for (index, field) in self.fields.iter().enumerate() {
                    assert_eq!(unsafe { *field.get() }, index + 1);
                }
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let ledger = Ledger::new(4096);
        let owner = AtomicShared::try_new(
            Probe {
                fields: std::array::from_fn(|_| std::cell::UnsafeCell::new(0)),
                dropped: dropped.clone(),
            },
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        let mut workers = Vec::new();
        for index in 0..4 {
            let peer = owner.clone();
            workers.push(thread::spawn(move || {
                unsafe { *peer.fields[index].get() = index + 1 };
                drop(peer);
            }));
        }
        drop(owner);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!((dropped.load(Ordering::Relaxed), ledger.current()), (1, 0));
    }

    #[test]
    fn exact_layout_alignment_borrow_and_refusal_match_live_owner() {
        #[repr(align(256))]
        struct Aligned<'a>(&'a usize);
        let value = 42;
        let bytes = AtomicShared::<Aligned<'_>>::allocation_bytes();
        let host = Ledger::new(bytes);
        let ledger = Ledger::child(bytes, host.clone());
        let owner =
            AtomicShared::try_new(Aligned(&value), BudgetAllocator(ledger.clone())).unwrap();
        let peer = owner.clone();
        assert_eq!((&*owner as *const Aligned<'_> as usize) % 256, 0);
        assert_eq!((ledger.current(), host.current()), (bytes, bytes));
        assert!(AtomicShared::try_new(Aligned(&value), BudgetAllocator(ledger.clone())).is_err());
        drop(owner);
        assert_eq!(*peer.0, 42);
        drop(peer);
        assert_eq!((ledger.current(), host.current()), (0, 0));
    }

    #[test]
    fn panicking_value_drop_still_deallocates_child_and_parent() {
        struct Panic;
        impl Drop for Panic {
            fn drop(&mut self) {
                panic!("atomic owner drop");
            }
        }
        let host = Ledger::new(4096);
        let ledger = Ledger::child(4096, host.clone());
        let owner = AtomicShared::try_new(Panic, BudgetAllocator(ledger.clone())).unwrap();
        assert!(catch_unwind(AssertUnwindSafe(|| drop(owner))).is_err());
        assert_eq!((ledger.current(), host.current()), (0, 0));
    }

    #[test]
    fn underlying_refusal_drops_input_once_and_restores_parent_charge() {
        struct Probe(Arc<AtomicUsize>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let host = Ledger::new(4096);
        let ledger = Ledger::child(4096, host.clone());
        let dropped = Arc::new(AtomicUsize::new(0));
        ledger.fail_after(0);
        assert!(
            AtomicShared::try_new(Probe(dropped.clone()), BudgetAllocator(ledger.clone())).is_err()
        );
        assert_eq!(
            (
                dropped.load(Ordering::Relaxed),
                ledger.current(),
                host.current()
            ),
            (1, 0, 0)
        );
        ledger.fail_after(usize::MAX);
        let recovered =
            AtomicShared::try_new(Probe(dropped.clone()), BudgetAllocator(ledger.clone())).unwrap();
        drop(recovered);
        assert_eq!(
            (
                dropped.load(Ordering::Relaxed),
                ledger.current(),
                host.current()
            ),
            (2, 0, 0)
        );
    }

    #[test]
    fn clone_retirement_count_matches_standard_arc_model() {
        let ledger = Ledger::new(4096);
        let owner = AtomicShared::try_new(42, BudgetAllocator(ledger.clone())).unwrap();
        let reference = Arc::new(42);
        let mut owners = Vec::new();
        let mut peers = Vec::new();
        for index in 0..1024 {
            if index % 3 == 2 {
                owners.pop();
                peers.pop();
            } else {
                owners.push(owner.clone());
                peers.push(reference.clone());
            }
            assert_eq!(
                unsafe { owner.pointer.as_ref() }
                    .strong
                    .load(Ordering::Relaxed),
                Arc::strong_count(&reference)
            );
            assert_eq!(ledger.current(), AtomicShared::<i32>::allocation_bytes());
        }
        drop(owners);
        drop(owner);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn overflow_panics_without_wrapping_or_invalidating_owner() {
        let ledger = Ledger::new(4096);
        let owner = AtomicShared::try_new(42, BudgetAllocator(ledger.clone())).unwrap();
        let strong = &unsafe { owner.pointer.as_ref() }.strong;
        strong.store(usize::MAX, Ordering::Relaxed);
        assert!(catch_unwind(AssertUnwindSafe(|| owner.clone())).is_err());
        assert_eq!(strong.load(Ordering::Relaxed), usize::MAX);
        strong.store(1, Ordering::Relaxed);
        drop(owner);
        assert_eq!(ledger.current(), 0);
    }
}
