use std::{ffi::c_void, marker::PhantomData, ptr};

use super::*;

pub(super) const VERSION: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct View {
    pub version: u64,
    pub slots: *mut Slot,
    pub length: u64,
    pub first: i64,
    pub dirty: *mut u64,
    pub counts: *mut Counts,
    pub writable: u64,
}

pub(super) type Entry = unsafe extern "C" fn(*const c_void, i64, *mut Slot) -> u32;

pub(in crate::jit) type KernelEntry =
    unsafe extern "C" fn(*mut Slot, u64, u32, *mut abi::Exit, *const c_void);

pub(super) struct Session<'a> {
    view: *const View,
    buffers: PhantomData<&'a mut ()>,
}

impl<const CAPACITY: usize> Window<'_, '_, CAPACITY> {
    pub(super) fn with_native<R>(&mut self, call: impl FnOnce(&mut Session<'_>) -> R) -> R {
        let view = View {
            version: VERSION,
            slots: self.slots.as_mut_ptr(),
            length: self.length as u64,
            first: self.first,
            dirty: ptr::from_mut(&mut self.dirty),
            counts: ptr::from_mut(&mut *self.counts),
            writable: u64::from(matches!(self.backing, Backing::Write(_))),
        };
        call(&mut Session {
            view: ptr::from_ref(&view),
            buffers: PhantomData,
        })
    }
}

impl Session<'_> {
    /// Invokes a helper-free kernel over disjoint initialized scratch and window buffers.
    ///
    /// # Safety
    /// Scratch covers the kernel's source register prefix. The kernel obeys the
    /// scalar access contract of invoke, preserves PC/budget bounds, and retains no pointers.
    pub(super) unsafe fn invoke_kernel(
        &mut self,
        entry: KernelEntry,
        slots: &mut [Slot],
        pc: u64,
        budget: u32,
    ) -> abi::Exit {
        let mut exit = abi::Exit::default();
        unsafe { entry(slots.as_mut_ptr(), pc, budget, &mut exit, self.view.cast()) };
        exit
    }

    /// Invokes a helper-free entry over the scoped scalar mirror and one separate slot.
    ///
    /// # Safety
    /// Entry preserves the descriptor, retains no pointers, and accesses only the
    /// described buffers. Writes require writable access and valid scalar values;
    /// dirty bits identify only initialized cells. No callbacks or unwinding occur.
    pub(super) unsafe fn invoke(&mut self, entry: Entry, key: i64, slot: &mut Slot) -> u32 {
        unsafe { entry(self.view.cast(), key, ptr::from_mut(slot)) }
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<View>() == 56);
    assert!(std::mem::offset_of!(View, slots) == 8);
    assert!(std::mem::offset_of!(View, length) == 16);
    assert!(std::mem::offset_of!(View, first) == 24);
    assert!(std::mem::offset_of!(View, dirty) == 32);
    assert!(std::mem::offset_of!(View, counts) == 40);
    assert!(std::mem::offset_of!(View, writable) == 48);
};

const _: () = {
    assert!(std::mem::size_of::<Counts>() == 8);
    assert!(std::mem::offset_of!(Counts, writes) == 4);
};

#[cfg(test)]
pub(super) mod tests;
