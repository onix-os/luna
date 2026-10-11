use super::*;

pub(crate) const RETURNED: u32 = 4;
pub(crate) const STORE_FAILURE: u32 = 4;
pub(crate) const HELPER_FAILURE: u32 = 5;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Progress {
    pub pc: u64,
    pub instructions: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    pub cause: Failure,
    pub progress: Option<Progress>,
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
    unsafe { invoke_report::<CAPACITY>(frame, entry, registers, pc, budget) }
        .map_err(|fault| fault.cause)
}

/// Invokes a packed entry and returns checked progress on failed gateways.
///
/// # Safety
/// Entry obeys the packed entry contract and any failure progress it publishes
/// identifies the owning source instruction and its completed prefix count.
pub(crate) unsafe fn invoke_report<const CAPACITY: usize>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    entry: Entry,
    registers: usize,
    pc: usize,
    budget: u32,
) -> Result<abi::Exit, Fault> {
    let fault = |cause| Fault {
        cause,
        progress: None,
    };
    if registers != frame.slot_count {
        return Err(fault(Failure::Admission(Refusal::Bounds)));
    }
    if frame.panic.is_some() {
        return Err(fault(Failure::Panicked));
    }
    let budget = budget.min(64);
    let unwritten_pc = u64::MAX - u64::from(pc as u64 == u64::MAX);
    let ((status, exit), refusal, helper) =
        with_bridge_report::<CAPACITY, _>(frame, |slots, bridge| {
            let mut exit = abi::Exit {
                pc: unwritten_pc,
                instructions: u32::MAX,
                reason: u32::MAX,
            };
            let status = unsafe { entry(slots, pc as u64, budget, &mut exit, bridge) };
            (status, exit)
        })
        .map_err(|refusal| fault(Failure::Admission(refusal)))?;
    let failed_progress = progress(&exit, status, helper, budget, unwritten_pc);
    let result = match (refusal, status) {
        (Some(refusal), _) => Err(Failure::Refused(refusal)),
        (None, RETURNED) => Ok(exit),
        (None, abi::HELPER_DECLINED) => Err(Failure::WriterDeclined),
        (None, abi::HELPER_PANICKED) if frame.panic.is_some() => Err(Failure::Panicked),
        (None, status) => Err(Failure::Status(status)),
    };
    match result {
        Ok(mut exit)
            if exit.pc != unwritten_pc
                && exit.instructions <= budget
                && exit.reason <= crate::jit::exits::Kind::Panic as u32
                && (exit.reason == crate::jit::exits::Kind::Panic as u32)
                    == frame.panic.is_some() =>
        {
            if frame.panic.is_some() {
                if let Some((pc, abi::HELPER_COMPLETED)) = helper {
                    if exit.pc != u64::from(pc) || exit.instructions >= budget {
                        return Err(fault(Failure::Exit));
                    }
                    exit.pc += 1;
                    exit.instructions += 1;
                }
            }
            Ok(exit)
        }
        Ok(_) => Err(fault(Failure::Exit)),
        Err(cause) => Err(Fault {
            cause,
            progress: failed_progress,
        }),
    }
}

fn progress(
    exit: &abi::Exit,
    status: u32,
    helper: Option<(u32, u32)>,
    budget: u32,
    unwritten_pc: u64,
) -> Option<Progress> {
    if !matches!(
        status,
        abi::HELPER_DECLINED | abi::HELPER_PANICKED | REFUSED
    ) || exit.pc == unwritten_pc
        || exit.instructions >= budget
    {
        return None;
    }
    let completed = match (exit.reason, helper) {
        (STORE_FAILURE, None) => false,
        (HELPER_FAILURE, None) if status == REFUSED => false,
        (HELPER_FAILURE, Some((pc, result)))
            if exit.pc == u64::from(pc)
                && matches!(status, REFUSED | abi::HELPER_PANICKED)
                && result <= abi::HELPER_PANICKED =>
        {
            result == abi::HELPER_COMPLETED
        }
        _ => return None,
    };
    Some(Progress {
        pc: exit.pc.checked_add(u64::from(completed))?,
        instructions: exit.instructions + u32::from(completed),
    })
}

#[path = "entry_tests.rs"]
mod tests;

#[cfg(not(miri))]
pub(crate) use tests::check_native_failure_progress;
#[cfg(not(miri))]
pub(crate) use tests::check_native_helper;
#[cfg(not(miri))]
pub(crate) use tests::failed_progress as failure_probe;
