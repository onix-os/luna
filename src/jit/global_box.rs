use std::alloc::Layout;

use allocator_api2::{alloc::AllocError, boxed::Box as BudgetBox};

use super::resources::BudgetAllocator;

pub(super) struct Charge {
    allocator: BudgetAllocator,
    bytes: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.allocator.0.release(self.bytes);
    }
}

/// Transfers an initialized Global allocation to a standard Box.
/// The returned charge must outlive the box, including unwinding.
pub(super) fn try_new<T>(
    value: T,
    allocator: BudgetAllocator,
) -> Result<(Box<T>, Charge), AllocError> {
    let boxed = BudgetBox::try_new_in(value, allocator)?;
    let (pointer, allocator) = BudgetBox::into_raw_with_allocator(boxed);
    let value = unsafe { Box::from_raw(pointer) };
    Ok((
        value,
        Charge {
            allocator,
            bytes: Layout::new::<T>().size(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
    };

    use super::*;
    use crate::jit::resources::Ledger;

    #[test]
    fn exact_charge_outlives_box_and_destructor_observes_reservation() {
        struct Probe {
            ledger: crate::jit::resources::LedgerRef,
            dropped: Rc<Cell<usize>>,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                assert_eq!(self.ledger.current(), Layout::new::<Self>().size());
                self.dropped.set(self.dropped.get() + 1);
            }
        }
        let host = Ledger::new(65536);
        let ledger = Ledger::child(65536, host.clone());
        let dropped = Rc::new(Cell::new(0));
        let charge;
        let value;
        (value, charge) = try_new(
            Probe {
                ledger: ledger.clone(),
                dropped: dropped.clone(),
            },
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        let bytes = Layout::new::<Probe>().size();
        assert_eq!((ledger.current(), host.current()), (bytes, bytes));
        drop(value);
        assert_eq!(
            (dropped.get(), ledger.current(), host.current()),
            (1, bytes, bytes)
        );
        drop(charge);
        assert_eq!((ledger.current(), host.current()), (0, 0));
    }

    #[test]
    fn child_parent_underlying_refusal_drop_input_and_restore_charges() {
        struct Probe(Rc<Cell<usize>>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let bytes = Layout::new::<Probe>().size();
        for cause in 0..3 {
            let host = Ledger::new(if cause == 1 { bytes - 1 } else { bytes });
            let ledger = Ledger::child(if cause == 0 { bytes - 1 } else { bytes }, host.clone());
            let dropped = Rc::new(Cell::new(0));
            if cause == 2 {
                ledger.fail_after(0);
            }
            assert!(try_new(Probe(dropped.clone()), BudgetAllocator(ledger.clone())).is_err());
            assert_eq!((dropped.get(), ledger.current(), host.current()), (1, 0, 0));
            host.set_limit(bytes);
            ledger.set_limit(bytes);
            ledger.fail_after(usize::MAX);
            let charge;
            let value;
            (value, charge) =
                try_new(Probe(dropped.clone()), BudgetAllocator(ledger.clone())).unwrap();
            drop(value);
            drop(charge);
            assert_eq!((dropped.get(), ledger.current(), host.current()), (2, 0, 0));
        }
    }

    #[test]
    fn over_aligned_borrow_and_zero_sized_box_use_global_layout() {
        #[repr(align(256))]
        struct Aligned<'a>(&'a usize);
        let value = 42;
        let ledger = Ledger::new(Layout::new::<Aligned<'_>>().size());
        let charge;
        let boxed;
        (boxed, charge) = try_new(Aligned(&value), BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!((&*boxed as *const Aligned<'_> as usize) % 256, 0);
        assert_eq!(*boxed.0, 42);
        drop(boxed);
        drop(charge);
        assert_eq!(ledger.current(), 0);
        ledger.set_limit(0);
        let (zero, zero_charge) = try_new((), BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!((ledger.current(), ledger.peak()), (0, 256));
        drop(zero);
        drop(zero_charge);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn trait_object_transfer_preserves_payload_layout_and_drop_order() {
        trait Answer: Send {
            fn answer(&self) -> usize;
        }
        struct Provider {
            ledger: crate::jit::resources::LedgerRef,
            dropped: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Answer for Provider {
            fn answer(&self) -> usize {
                42
            }
        }
        impl Drop for Provider {
            fn drop(&mut self) {
                assert_eq!(self.ledger.current(), Layout::new::<Self>().size());
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let ledger = Ledger::new(4096);
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let charge;
        let value;
        (value, charge) = try_new(
            Provider {
                ledger: ledger.clone(),
                dropped: dropped.clone(),
            },
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        let erased: Box<dyn Answer + Send> = value;
        assert_eq!(erased.answer(), 42);
        assert_eq!(ledger.current(), Layout::new::<Provider>().size());
        drop(erased);
        assert_eq!(dropped.load(std::sync::atomic::Ordering::Relaxed), 1);
        drop(charge);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn scope_panic_drops_box_before_charge_and_reclaims_global_allocation() {
        struct Probe(crate::jit::resources::LedgerRef);
        impl Drop for Probe {
            fn drop(&mut self) {
                assert_eq!(self.0.current(), Layout::new::<Self>().size());
            }
        }
        let ledger = Ledger::new(4096);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _charge;
            let _value;
            (_value, _charge) =
                try_new(Probe(ledger.clone()), BudgetAllocator(ledger.clone())).unwrap();
            panic!("scope unwind");
        }))
        .is_err());
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn value_panic_keeps_charge_until_standard_box_unwind_deallocation() {
        struct Probe(crate::jit::resources::LedgerRef);
        impl Drop for Probe {
            fn drop(&mut self) {
                assert_eq!(self.0.current(), Layout::new::<Self>().size());
                panic!("value unwind");
            }
        }
        let host = Ledger::new(4096);
        let ledger = Ledger::child(4096, host.clone());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let charge;
            let value;
            (value, charge) =
                try_new(Probe(ledger.clone()), BudgetAllocator(ledger.clone())).unwrap();
            drop(value);
            drop(charge);
        }))
        .is_err());
        assert_eq!((ledger.current(), host.current()), (0, 0));
    }
}
