use std::panic::{catch_unwind, AssertUnwindSafe};

use super::*;

const REFUSED: u32 = 3;
const _: () = assert!(
    REFUSED != abi::HELPER_COMPLETED
        && REFUSED != abi::HELPER_DECLINED
        && REFUSED != abi::HELPER_PANICKED
);
type Helper = unsafe extern "C" fn(*mut (), u32, u32, u32, u32) -> u32;

#[repr(C)]
struct Bridge {
    control: *mut (),
    helpers: &'static [Helper; 10],
}

struct Functions<const CAPACITY: usize>;

impl<const CAPACITY: usize> Functions<CAPACITY> {
    const HELPERS: [Helper; 10] = [
        helper::<CAPACITY, 1>,
        helper::<CAPACITY, 2>,
        helper::<CAPACITY, 3>,
        helper::<CAPACITY, 4>,
        helper::<CAPACITY, 5>,
        helper::<CAPACITY, 6>,
        helper::<CAPACITY, 7>,
        helper::<CAPACITY, 8>,
        helper::<CAPACITY, 9>,
        helper::<CAPACITY, 10>,
    ];
}

unsafe extern "C" fn helper<const CAPACITY: usize, const KIND: u32>(
    control: *mut (),
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    unsafe { boundary::<CAPACITY>(control, |bound| bound.helper::<KIND>(a, b, c, pc)) }
}

unsafe fn boundary<const CAPACITY: usize>(
    control: *mut (),
    body: impl FnOnce(&mut FrameScope<'_, '_, '_, '_, '_, CAPACITY>) -> Result<u32, Refusal>,
) -> u32 {
    let bound = unsafe { &mut *control.cast::<FrameScope<'_, '_, '_, '_, '_, CAPACITY>>() };
    if bound.refusal.is_some() {
        return REFUSED;
    }
    if bound.scope.stopped {
        if unsafe { bound.frame.as_ref() }.panic.is_some() {
            return abi::HELPER_PANICKED;
        }
        bound.refusal = Some(Refusal::Stopped);
        return REFUSED;
    }
    match catch_unwind(AssertUnwindSafe(|| body(bound))) {
        Ok(Ok(result)) => result,
        Ok(Err(refusal)) => {
            bound.scope.stopped = true;
            bound.refusal = Some(refusal);
            REFUSED
        }
        Err(payload) => {
            bound.scope.stopped = true;
            unsafe { bound.frame.as_mut() }.panic = Some(payload);
            abi::HELPER_PANICKED
        }
    }
}

fn with_bridge<const CAPACITY: usize, R>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    body: impl FnOnce(*mut Packed, &Bridge) -> R,
) -> Result<R, Refusal> {
    with_frame_scope::<CAPACITY, _>(frame, |bound| {
        let slots = bound.scope.descriptors.as_ptr();
        let bridge = Bridge {
            control: std::ptr::from_mut(bound).cast(),
            helpers: &Functions::<CAPACITY>::HELPERS,
        };
        let result = body(slots, &bridge);
        bound.refusal.map_or(Ok(result), Err)
    })?
}

#[path = "bridge_tests.rs"]
mod tests;

#[path = "bridge_reentry.rs"]
mod reentry;
