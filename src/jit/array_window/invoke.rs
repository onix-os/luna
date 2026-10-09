use super::{native::KernelEntry, plan::Plan, *};

pub(super) struct Outcome {
    pub exit: abi::Exit,
    pub counts: Counts,
}

/// Executes a source-bound kernel after fresh receiver and window admission.
///
/// # Safety
/// Entry matches plan and the initialized register prefix, obeys the scoped scalar
/// kernel contract, and remains executable for the call. Slots and canonical are
/// the same frame; reference slots resolve to their canonical values.
pub(super) unsafe fn invoke<'gc>(
    entry: KernelEntry,
    plan: Plan,
    ctx: Context<'gc>,
    canonical: &[Value<'gc>],
    slots: &mut [Slot],
    pc: usize,
    budget: u32,
) -> Option<Outcome> {
    if slots.len() != canonical.len() || budget == 0 {
        return None;
    }
    let receiver = usize::from(plan.table);
    let slot = slots.get(receiver)?;
    if slot.tag != abi::REFERENCE || slot.bits != 0 {
        return None;
    }
    let Value::Table(table) = *canonical.get(receiver)? else {
        return None;
    };
    let array_length = table
        .into_inner()
        .try_borrow()
        .ok()?
        .raw_table
        .array()
        .len();
    let (first, length) = plan.window(pc, slots, array_length)?;
    let mut counts = Counts::default();
    let exit = with_window::<_, 64>(
        ctx,
        table,
        first,
        length,
        plan.access,
        &mut counts,
        |window| {
            window.with_native(|session| unsafe {
                session.invoke_kernel(entry, slots, pc as u64, budget)
            })
        },
    )
    .ok()?;
    Some(Outcome { exit, counts })
}

#[cfg(test)]
mod tests;
