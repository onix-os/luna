use std::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::Deref,
    ptr::NonNull,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use allocator_api2::alloc::{AllocError, Allocator, Global, Layout};

#[cfg(test)]
thread_local! {
    static DENIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn with_allocation_denied<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            DENIED.set(self.0);
        }
    }
    let reset = Reset(DENIED.replace(true));
    let result = f();
    drop(reset);
    result
}

struct Inner<T, C> {
    strong: AtomicUsize,
    value: T,
    charge: ManuallyDrop<C>,
}

pub(crate) struct GlobalShared<T, C> {
    pointer: NonNull<Inner<T, C>>,
    marker: PhantomData<Arc<(T, C)>>,
}

unsafe impl<T: Send + Sync, C: Send + Sync> Send for GlobalShared<T, C> {}
unsafe impl<T: Send + Sync, C: Send + Sync> Sync for GlobalShared<T, C> {}

struct Allocation<T, C> {
    pointer: NonNull<Inner<T, C>>,
    _charge: C,
}

impl<T, C> Drop for Allocation<T, C> {
    fn drop(&mut self) {
        unsafe { Global.deallocate(self.pointer.cast(), Layout::new::<Inner<T, C>>()) };
    }
}

impl<T, C> GlobalShared<T, C> {
    pub fn allocation_layout() -> Layout {
        Layout::new::<Inner<T, C>>()
    }

    pub fn allocation_bytes() -> usize {
        Self::allocation_layout().size()
    }

    pub fn try_new(value: T, charge: C) -> Result<Self, AllocError> {
        #[cfg(test)]
        let denied = DENIED.get();
        #[cfg(not(test))]
        let denied = false;
        let allocation = if denied {
            Err(AllocError)
        } else {
            Global.allocate(Self::allocation_layout())
        };
        let allocation = match allocation {
            Ok(allocation) => allocation,
            Err(error) => {
                drop(value);
                drop(charge);
                return Err(error);
            }
        };
        let pointer = allocation.cast::<Inner<T, C>>();
        unsafe {
            pointer.as_ptr().write(Inner {
                strong: AtomicUsize::new(1),
                value,
                charge: ManuallyDrop::new(charge),
            });
        }
        Ok(Self {
            pointer,
            marker: PhantomData,
        })
    }

    #[cfg(test)]
    pub fn strong_count(this: &Self) -> usize {
        unsafe { this.pointer.as_ref() }
            .strong
            .load(Ordering::Relaxed)
    }

    #[cfg(any(
        test,
        all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        left.pointer == right.pointer
    }
}

impl<T, C> Clone for GlobalShared<T, C> {
    fn clone(&self) -> Self {
        unsafe { self.pointer.as_ref() }
            .strong
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_add(1)
            })
            .expect("JIT global owner count overflow");
        Self {
            pointer: self.pointer,
            marker: PhantomData,
        }
    }
}

impl<T, C> Deref for GlobalShared<T, C> {
    type Target = T;

    fn deref(&self) -> &T {
        &unsafe { self.pointer.as_ref() }.value
    }
}

impl<T, C> Drop for GlobalShared<T, C> {
    fn drop(&mut self) {
        let inner = unsafe { self.pointer.as_ref() };
        if inner.strong.fetch_sub(1, Ordering::AcqRel) == 1 {
            let charge = unsafe { ManuallyDrop::take(&mut (*self.pointer.as_ptr()).charge) };
            let allocation = Allocation {
                pointer: self.pointer,
                _charge: charge,
            };
            unsafe {
                std::ptr::drop_in_place(std::ptr::addr_of_mut!((*self.pointer.as_ptr()).value))
            };
            drop(allocation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
        thread,
    };

    struct Value(Rc<Cell<usize>>, bool);
    impl Drop for Value {
        fn drop(&mut self) {
            assert_eq!(self.0.replace(1), 0);
            assert!(!self.1, "injected value panic");
        }
    }
    struct Charge(Rc<Cell<usize>>);
    impl Drop for Charge {
        fn drop(&mut self) {
            assert_eq!(self.0.replace(2), 1);
        }
    }

    #[test]
    fn final_clone_destroys_value_before_releasing_charge() {
        let state = Rc::new(Cell::new(0));
        let first =
            GlobalShared::try_new(Value(state.clone(), false), Charge(state.clone())).unwrap();
        let peer = first.clone();
        assert!(GlobalShared::ptr_eq(&first, &peer));
        assert_eq!(GlobalShared::strong_count(&first), 2);
        drop(first);
        assert_eq!(state.get(), 0);
        drop(peer);
        assert_eq!(state.get(), 2);
    }

    #[test]
    fn value_panic_still_deallocates_and_releases_charge_once() {
        let state = Rc::new(Cell::new(0));
        let owner =
            GlobalShared::try_new(Value(state.clone(), true), Charge(state.clone())).unwrap();
        assert!(catch_unwind(AssertUnwindSafe(|| drop(owner))).is_err());
        assert_eq!(state.get(), 2);
    }

    #[test]
    fn allocation_refusal_drops_both_inputs_without_retention() {
        let state = Rc::new(Cell::new(0));
        with_allocation_denied(|| {
            assert!(
                GlobalShared::try_new(Value(state.clone(), false), Charge(state.clone())).is_err()
            );
        });
        assert_eq!(state.get(), 2);
        let recovered = GlobalShared::try_new(42, ()).unwrap();
        assert_eq!(*recovered, 42);
    }

    #[test]
    fn exact_layout_supports_overaligned_borrowed_values() {
        #[repr(align(256))]
        struct Aligned<'a>(&'a str);
        let source = String::from("rooted");
        let owner = GlobalShared::try_new(Aligned(&source), ()).unwrap();
        assert_eq!((&*owner as *const Aligned<'_> as usize) % 256, 0);
        assert_eq!(owner.0, "rooted");
        assert_eq!(
            GlobalShared::<Aligned<'_>, ()>::allocation_layout().align(),
            256
        );
    }

    #[test]
    fn count_overflow_preserves_original_owner() {
        let owner = GlobalShared::try_new(42, ()).unwrap();
        unsafe { owner.pointer.as_ref() }
            .strong
            .store(usize::MAX, Ordering::Relaxed);
        assert!(catch_unwind(AssertUnwindSafe(|| owner.clone())).is_err());
        assert_eq!(GlobalShared::strong_count(&owner), usize::MAX);
        unsafe { owner.pointer.as_ref() }
            .strong
            .store(1, Ordering::Relaxed);
    }

    #[test]
    fn final_drop_acquires_disjoint_writes_from_all_owners() {
        struct Probe([std::cell::UnsafeCell<usize>; 4], Arc<AtomicUsize>);
        unsafe impl Sync for Probe {}
        impl Drop for Probe {
            fn drop(&mut self) {
                for (index, field) in self.0.iter().enumerate() {
                    assert_eq!(unsafe { *field.get() }, index + 1);
                }
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let owner = GlobalShared::try_new(
            Probe(
                std::array::from_fn(|_| std::cell::UnsafeCell::new(0)),
                dropped.clone(),
            ),
            (),
        )
        .unwrap();
        let mut threads = Vec::new();
        for index in 0..4 {
            let peer = owner.clone();
            threads.push(thread::spawn(move || {
                unsafe {
                    *peer.0[index].get() = index + 1;
                }
                drop(peer);
            }));
        }
        drop(owner);
        for worker in threads {
            worker.join().unwrap();
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }
}
