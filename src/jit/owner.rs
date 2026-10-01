use std::{cell::Cell, marker::PhantomData, ops::Deref, ptr::NonNull, rc::Rc};

use allocator_api2::{
    alloc::{Allocator, Layout},
    boxed::Box,
};

use super::resources::BudgetAllocator;

struct Inner<T> {
    strong: Cell<usize>,
    allocator: BudgetAllocator,
    value: T,
}

pub(super) struct Shared<T> {
    pointer: NonNull<Inner<T>>,
    marker: PhantomData<Rc<T>>,
}

pub(super) struct Allocation<T> {
    pub pointer: NonNull<T>,
    pub allocator: BudgetAllocator,
}

impl<T> Drop for Allocation<T> {
    fn drop(&mut self) {
        unsafe {
            self.allocator
                .deallocate(self.pointer.cast(), Layout::new::<T>())
        };
    }
}

impl<T> Shared<T> {
    pub fn try_new(
        value: T,
        allocator: BudgetAllocator,
    ) -> Result<Self, allocator_api2::alloc::AllocError> {
        let inner = Box::try_new_in(
            Inner {
                strong: Cell::new(1),
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

    pub fn strong_count(this: &Self) -> usize {
        unsafe { this.pointer.as_ref() }.strong.get()
    }

    #[cfg(test)]
    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        left.pointer == right.pointer
    }

    #[cfg(test)]
    pub fn allocation_bytes() -> usize {
        Layout::new::<Inner<T>>().size()
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        let strong = &unsafe { self.pointer.as_ref() }.strong;
        strong.set(
            strong
                .get()
                .checked_add(1)
                .expect("JIT owner count overflow"),
        );
        Self {
            pointer: self.pointer,
            marker: PhantomData,
        }
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &unsafe { self.pointer.as_ref() }.value
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        let inner = unsafe { self.pointer.as_ref() };
        let strong = inner.strong.get();
        inner.strong.set(strong - 1);
        if strong == 1 {
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
        cell::RefCell,
        panic::{catch_unwind, AssertUnwindSafe},
    };

    use super::*;
    use crate::jit::resources::Ledger;

    struct Probe(Rc<Cell<usize>>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn exact_allocation_charge_survives_clones_until_final_drop() {
        let ledger = Ledger::new(65536);
        let dropped = Rc::new(Cell::new(0));
        let first =
            Shared::try_new(Probe(dropped.clone()), BudgetAllocator(ledger.clone())).unwrap();
        let bytes = Shared::<Probe>::allocation_bytes();
        assert_eq!((ledger.current(), ledger.peak()), (bytes, bytes));
        let second = first.clone();
        let third = second.clone();
        assert!(Shared::ptr_eq(&first, &second));
        assert_eq!(Shared::strong_count(&first), 3);
        drop(second);
        drop(first);
        assert_eq!(
            (
                dropped.get(),
                ledger.current(),
                Shared::strong_count(&third)
            ),
            (0, bytes, 1)
        );
        drop(third);
        assert_eq!((dropped.get(), ledger.current()), (1, 0));
    }

    #[test]
    fn one_byte_short_and_underlying_refusal_drop_input_without_retained_charge() {
        for underlying in [false, true] {
            let bytes = Shared::<Probe>::allocation_bytes();
            let ledger = Ledger::new(if underlying { bytes } else { bytes - 1 });
            if underlying {
                ledger.fail_after(0);
            }
            let dropped = Rc::new(Cell::new(0));
            assert!(
                Shared::try_new(Probe(dropped.clone()), BudgetAllocator(ledger.clone())).is_err()
            );
            assert_eq!(
                (dropped.get(), ledger.current(), ledger.refusals()),
                (1, 0, 1)
            );
        }
    }

    #[test]
    fn exact_limit_admits_and_parent_refusal_releases_both_ledgers() {
        let bytes = Shared::<u64>::allocation_bytes();
        let host = Ledger::new(bytes);
        let child = Ledger::child(bytes, host.clone());
        let owner = Shared::try_new(42u64, BudgetAllocator(child.clone())).unwrap();
        assert_eq!((child.current(), host.current()), (bytes, bytes));
        let peer = owner.clone();
        drop(owner);
        assert_eq!(*peer, 42);
        assert!(Shared::try_new(99u64, BudgetAllocator(child.clone())).is_err());
        drop(peer);
        assert_eq!((child.current(), host.current()), (0, 0));
        host.set_limit(bytes - 1);
        assert!(Shared::try_new(42u64, BudgetAllocator(child.clone())).is_err());
        assert_eq!(
            (child.current(), host.current(), host.refusals()),
            (0, 0, 1)
        );
    }

    #[test]
    fn over_aligned_value_and_live_reference_survive_other_owner_drops() {
        #[repr(align(256))]
        struct Aligned([u8; 17]);
        let ledger = Ledger::new(65536);
        let first = Shared::try_new(Aligned([42; 17]), BudgetAllocator(ledger.clone())).unwrap();
        let second = first.clone();
        let value = &first.0;
        assert_eq!(std::ptr::from_ref(&*first).addr() % 256, 0);
        drop(second);
        assert_eq!(value, &[42; 17]);
        assert_eq!(ledger.current(), Shared::<Aligned>::allocation_bytes());
        drop(first);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn shared_interior_mutation_and_distinct_identity_match_rc() {
        let ledger = Ledger::new(65536);
        let first = Shared::try_new(RefCell::new(42), BudgetAllocator(ledger.clone())).unwrap();
        let alias = first.clone();
        let separate = Shared::try_new(RefCell::new(42), BudgetAllocator(ledger.clone())).unwrap();
        *alias.borrow_mut() = 99;
        assert_eq!(*first.borrow(), 99);
        assert_eq!(*separate.borrow(), 42);
        assert!(Shared::ptr_eq(&first, &alias));
        assert!(!Shared::ptr_eq(&first, &separate));
        drop((first, alias, separate));
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn panicking_value_drop_still_reclaims_allocation_and_parent_charge() {
        struct Panics;
        impl Drop for Panics {
            fn drop(&mut self) {
                panic!("drop probe");
            }
        }
        let host = Ledger::new(65536);
        let child = Ledger::child(65536, host.clone());
        let first = Shared::try_new(Panics, BudgetAllocator(child.clone())).unwrap();
        let second = first.clone();
        drop(first);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(second))).is_err());
        assert_eq!((child.current(), host.current()), (0, 0));
    }

    #[test]
    fn count_overflow_panics_without_changing_live_owner_count() {
        let ledger = Ledger::new(65536);
        let first = Shared::try_new(42, BudgetAllocator(ledger.clone())).unwrap();
        unsafe { first.pointer.as_ref() }.strong.set(usize::MAX);
        assert!(catch_unwind(AssertUnwindSafe(|| first.clone())).is_err());
        assert_eq!(Shared::strong_count(&first), usize::MAX);
        unsafe { first.pointer.as_ref() }.strong.set(1);
        drop(first);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn mixed_clone_retirement_sequence_matches_standard_rc() {
        let ledger = Ledger::new(65536);
        let first = Shared::try_new(42, BudgetAllocator(ledger.clone())).unwrap();
        let reference = Rc::new(42);
        let mut owners = std::vec::Vec::new();
        let mut peers = std::vec::Vec::new();
        for index in 0..1024 {
            if index % 3 == 2 {
                owners.pop();
                peers.pop();
            } else {
                owners.push(first.clone());
                peers.push(reference.clone());
            }
            assert_eq!(Shared::strong_count(&first), Rc::strong_count(&reference));
            assert_eq!(ledger.current(), Shared::<i32>::allocation_bytes());
        }
        drop(owners);
        assert_eq!(Shared::strong_count(&first), 1);
        drop(first);
        assert_eq!(ledger.current(), 0);
    }
}
