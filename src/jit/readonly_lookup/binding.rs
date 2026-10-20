use std::{cell::Ref, marker::PhantomData};

use crate::{closure::UpValueState, table::TableState, Closure, Constant, Table, Value};

use super::super::{abi, entry_bytes::EntryBytes, helpers};
use super::Plan;

#[repr(C)]
pub(in crate::jit) struct View {
    pub binding: [u64; 2],
    pub slot: abi::Slot,
    pub executed: u64,
}

pub(in crate::jit) type Entry =
    unsafe extern "C" fn(*mut abi::Slot, u64, u32, *mut abi::Exit, *mut abi::Host, *mut View);

#[derive(Debug, PartialEq, Eq)]
pub(in crate::jit) enum Fault {
    Marker,
    Budget,
}

pub(in crate::jit) struct Bound<'entry, 'gc> {
    plan: Plan,
    closure: Closure<'gc>,
    table: Table<'gc>,
    value: Value<'gc>,
    _guard: Ref<'gc, TableState<'gc>>,
    _entry: PhantomData<&'entry mut ()>,
}

impl Plan {
    pub(in crate::jit) fn binding(self) -> [u64; 2] {
        [
            (u64::from(self.helper_pc) << 32) | u64::from(self.call_pc),
            (u64::from(self.registers) << 24)
                | (u64::from(self.destination) << 16)
                | (u64::from(self.upvalue) << 8)
                | u64::from(self.constant),
        ]
    }

    pub(in crate::jit) fn with_binding<'gc, R>(
        self,
        closure: Closure<'gc>,
        hints: &EntryBytes,
        run: impl for<'entry> FnOnce(Bound<'entry, 'gc>) -> R,
    ) -> Option<R> {
        let hint = hints.hint(self.helper_pc() as usize)?;
        hint.load()?;
        let prototype = closure.prototype();
        let Constant::String(key) = *prototype.constants.get(usize::from(self.constant))? else {
            return None;
        };
        let UpValueState::Closed(Value::Table(table)) = closure
            .upvalues()
            .get(usize::from(self.upvalue))?
            .get()
            .get()
        else {
            return None;
        };
        let (guard, value) = table.try_strong_canonical_hint(key, hint)?;
        Some(run(Bound {
            plan: self,
            closure,
            table,
            value,
            _guard: guard,
            _entry: PhantomData,
        }))
    }

    /// Captures a current closed binding and enters it without an intervening callback.
    ///
    /// # Safety
    /// Entry implements this plan's verified six-argument ABI with disjoint frame,
    /// slot and view storage, retains no pointers and cannot invoke users or mutate
    /// the environment before its terminating Call exit.
    /// It observes no lookup destination before that exit and marks execution once.
    pub(in crate::jit) unsafe fn invoke_fresh<'gc>(
        self,
        entry: Entry,
        frame: &mut helpers::Frame<'gc, '_, '_, '_>,
        slots: &mut [abi::Slot],
        pc: usize,
        budget: u32,
    ) -> Result<Option<abi::Exit>, Fault> {
        if !self.fits(frame, slots) {
            return Ok(None);
        }
        let Some(hints) = frame.read_hints else {
            return Ok(None);
        };
        self.with_binding(frame.closure, hints, |bound| unsafe {
            bound.invoke_captured(entry, frame, slots, pc, budget)
        })
        .unwrap_or(Ok(None))
    }

    fn fits(self, frame: &helpers::Frame<'_, '_, '_, '_>, slots: &[abi::Slot]) -> bool {
        let width = usize::from(self.registers);
        frame.slot_count == width
            && slots.len() == width
            && frame.registers.stack_frame.len() >= width
            && frame.projection.is_none()
            && frame.panic.is_none()
    }
}

impl<'gc> Bound<'_, 'gc> {
    /// Invokes matching verified read-only code with disjoint frame, slot and view storage.
    ///
    /// # Safety
    /// Entry implements this plan's six-argument ABI and cannot call users, mutate the
    /// environment or observe the lookup destination before its terminating Call exit.
    /// It retains no pointers and reports the lookup with a single execution marker.
    pub(in crate::jit) unsafe fn invoke(
        self,
        entry: Entry,
        frame: &mut helpers::Frame<'gc, '_, '_, '_>,
        slots: &mut [abi::Slot],
        pc: usize,
        budget: u32,
    ) -> Result<Option<abi::Exit>, Fault> {
        let current = frame
            .closure
            .upvalues()
            .get(usize::from(self.plan.upvalue))
            .map(|cell| cell.get().get());
        if frame.closure != self.closure
            || !matches!(current, Some(UpValueState::Closed(Value::Table(table))) if table == self.table)
            || !self.plan.fits(frame, slots)
        {
            return Ok(None);
        }
        unsafe { self.invoke_captured(entry, frame, slots, pc, budget) }
    }

    unsafe fn invoke_captured(
        self,
        entry: Entry,
        frame: &mut helpers::Frame<'gc, '_, '_, '_>,
        slots: &mut [abi::Slot],
        pc: usize,
        budget: u32,
    ) -> Result<Option<abi::Exit>, Fault> {
        let mut view = View {
            binding: self.plan.binding(),
            slot: abi::Slot::from_value(self.value),
            executed: 0,
        };
        let mut host = abi::Host {
            data: (frame as *mut helpers::Frame<'_, '_, '_, '_>).cast(),
            projection: std::ptr::null_mut(),
        };
        let mut exit = abi::Exit::default();
        unsafe {
            entry(
                slots.as_mut_ptr(),
                pc as u64,
                budget.min(64),
                &mut exit,
                &mut host,
                &mut view,
            )
        };
        match view.executed {
            0 => {}
            1 => {
                frame.registers.stack_frame[usize::from(self.plan.destination())] = self.value;
                frame.count.table_reads += 1;
                frame.count.upvalue_reads += 1;
            }
            _ => return Err(Fault::Marker),
        }
        if exit.instructions > budget.min(64) || (view.executed == 1 && exit.instructions == 0) {
            return Err(Fault::Budget);
        }
        Ok(Some(exit))
    }
}

const _: () = {
    assert!(std::mem::size_of::<View>() == 40);
    assert!(std::mem::offset_of!(View, slot) == 16);
    assert!(std::mem::offset_of!(View, executed) == 32);
};

#[cfg(test)]
mod tests;
