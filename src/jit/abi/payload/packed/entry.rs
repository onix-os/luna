use super::*;

pub(crate) const RETURNED: u32 = 4;
pub(crate) type Entry =
    unsafe extern "C" fn(*mut Packed, u64, u32, *mut abi::Exit, *const Bridge) -> u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Admission(Refusal),
    Refused(Refusal),
    WriterDeclined,
    Panicked,
    Status(u32),
    Exit,
}

/// Invokes a verified packed entry without replaying a failed gateway.
///
/// # Safety
/// Entry covers exactly `registers` cells, implements the packed gateway and
/// descriptor lifetime contracts, and retains no frame pointers after returning.
pub(crate) unsafe fn invoke<const CAPACITY: usize>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    entry: Entry,
    registers: usize,
    pc: usize,
    budget: u32,
) -> Result<abi::Exit, Failure> {
    if registers != frame.slot_count {
        return Err(Failure::Admission(Refusal::Bounds));
    }
    if frame.panic.is_some() {
        return Err(Failure::Panicked);
    }
    let mut entered = false;
    let budget = budget.min(64);
    let unwritten_pc = u64::MAX - u64::from(pc as u64 == u64::MAX);
    let result = with_bridge::<CAPACITY, _>(frame, |slots, bridge| {
        entered = true;
        let mut exit = abi::Exit {
            pc: unwritten_pc,
            instructions: u32::MAX,
            reason: u32::MAX,
        };
        let status = unsafe { entry(slots, pc as u64, budget, &mut exit, bridge) };
        match status {
            RETURNED => Ok(exit),
            abi::HELPER_DECLINED => Err(Failure::WriterDeclined),
            abi::HELPER_PANICKED => Err(Failure::Panicked),
            status => Err(Failure::Status(status)),
        }
    });
    let result = result.map_err(|refusal| {
        if entered {
            Failure::Refused(refusal)
        } else {
            Failure::Admission(refusal)
        }
    })?;
    match result {
        Ok(exit)
            if exit.pc != unwritten_pc
                && exit.instructions <= budget
                && exit.reason <= crate::jit::exits::Kind::Panic as u32
                && (exit.reason == crate::jit::exits::Kind::Panic as u32)
                    == frame.panic.is_some() =>
        {
            Ok(exit)
        }
        Ok(_) => Err(Failure::Exit),
        Err(Failure::Panicked) if frame.panic.is_none() => {
            Err(Failure::Status(abi::HELPER_PANICKED))
        }
        Err(error) => Err(error),
    }
}

#[path = "entry_tests.rs"]
mod tests;

#[cfg(not(miri))]
pub(crate) use tests::check_native_helper;
