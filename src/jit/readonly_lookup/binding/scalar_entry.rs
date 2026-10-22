use super::*;
use crate::{jit::readonly_lookup::scalar_entry::Source, thread::LuaRegisters};

pub(in crate::jit) struct Outcome {
    pub exit: abi::Exit,
    pub reads: u64,
}

impl Source {
    /// Captures a fresh binding and invokes its scalar-input entry without a helper frame.
    ///
    /// # Safety
    /// Entry matches this capability's source and six-argument ABI. Scratch and
    /// canonical registers are disjoint. Entry retains no pointers, invokes no
    /// users, cannot collect or mutate the environment, and accepts a null Host.
    #[cfg(test)]
    #[inline(always)]
    pub(in crate::jit) unsafe fn invoke_scalar_fresh<'gc>(
        self,
        entry: Entry,
        closure: Closure<'gc>,
        hints: &EntryBytes,
        registers: &mut LuaRegisters<'gc, '_>,
        slots: &mut [abi::Slot],
        pc: usize,
        budget: u32,
    ) -> Result<Option<Outcome>, Fault> {
        if !self.fits(slots, registers.stack_frame) {
            return Ok(None);
        }
        unsafe { self.invoke_admitted(entry, closure, hints, registers, slots, pc, budget) }
    }

    /// Consumes a marshal certificate and invokes its matching scalar-input entry.
    ///
    /// # Safety
    /// Entry matches this source and six-argument ABI. Canonical and scratch storage
    /// are disjoint. Entry retains no pointers, invokes no users, cannot collect or
    /// mutate the environment, and accepts a null Host.
    #[inline(always)]
    pub(in crate::jit) unsafe fn invoke_marshaled<'gc>(
        self,
        entry: Entry,
        closure: Closure<'gc>,
        hints: &EntryBytes,
        registers: &mut LuaRegisters<'gc, '_>,
        inputs: &mut abi::transport_inputs::Inputs<'_>,
        pc: usize,
        budget: u32,
    ) -> Result<Option<Outcome>, Fault> {
        if !self.fits_extents(inputs.slots(), registers.stack_frame) {
            return Ok(None);
        }
        let Some(slots) = inputs.take_scalar() else {
            return Ok(None);
        };
        unsafe { self.invoke_admitted(entry, closure, hints, registers, slots, pc, budget) }
    }

    #[inline(always)]
    unsafe fn invoke_admitted<'gc>(
        self,
        entry: Entry,
        closure: Closure<'gc>,
        hints: &EntryBytes,
        registers: &mut LuaRegisters<'gc, '_>,
        slots: &mut [abi::Slot],
        pc: usize,
        budget: u32,
    ) -> Result<Option<Outcome>, Fault> {
        let plan = self.plan();
        let mut scope = ();
        let Some(bound) = plan.capture(closure, hints, &mut scope) else {
            return Ok(None);
        };
        let mut view = View {
            binding: plan.binding(),
            slot: abi::Slot::from_value(bound.value),
            executed: 0,
        };
        let mut exit = abi::Exit::default();
        unsafe {
            entry(
                slots.as_mut_ptr(),
                pc as u64,
                budget.min(64),
                &mut exit,
                std::ptr::null_mut(),
                &mut view,
            );
        }
        match view.executed {
            0 => {}
            1 => registers.stack_frame[usize::from(plan.destination())] = bound.value,
            _ => return Err(Fault::Marker),
        }
        if exit.instructions > budget.min(64) || (view.executed == 1 && exit.instructions == 0) {
            return Err(Fault::Budget);
        }
        Ok(Some(Outcome {
            exit,
            reads: view.executed,
        }))
    }
}
