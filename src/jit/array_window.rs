use crate::{Context, Table, Value};

use super::abi::{self, Slot};

const LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    Capacity,
    Bounds,
    Borrowed,
    Observed,
    ReadOnly,
    Weak,
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Counts {
    pub reads: u32,
    pub writes: u32,
}

enum Backing<'a, 'gc> {
    Read(&'a [Value<'gc>]),
    Write(&'a mut [Value<'gc>]),
}

pub(super) struct Window<'a, 'gc, const CAPACITY: usize> {
    backing: Backing<'a, 'gc>,
    slots: [Slot; CAPACITY],
    first: i64,
    length: usize,
    dirty: u64,
    counts: &'a mut Counts,
}

fn scalar(slot: Slot) -> bool {
    match slot.tag {
        abi::NIL => slot.bits == 0,
        abi::BOOLEAN => slot.bits <= 1,
        abi::INTEGER | abi::NUMBER => true,
        _ => false,
    }
}

impl<'a, 'gc, const CAPACITY: usize> Window<'a, 'gc, CAPACITY> {
    fn new(backing: Backing<'a, 'gc>, first: i64, counts: &'a mut Counts) -> Self {
        let values = match &backing {
            Backing::Read(values) => *values,
            Backing::Write(values) => &**values,
        };
        let mut slots = [Slot {
            tag: abi::NIL,
            bits: 0,
        }; CAPACITY];
        for (slot, value) in slots.iter_mut().zip(values.iter().copied()) {
            *slot = Slot::from_value(value);
        }
        let length = values.len();
        Self {
            backing,
            slots,
            first,
            length,
            dirty: 0,
            counts,
        }
    }

    fn index(&self, key: i64) -> Option<usize> {
        let index = usize::try_from(key.checked_sub(self.first)?).ok()?;
        (index < self.length).then_some(index)
    }

    pub fn read(&mut self, key: i64) -> Option<Slot> {
        let slot = self.slots[self.index(key)?];
        if !scalar(slot) {
            return None;
        }
        self.counts.reads = self.counts.reads.saturating_add(1);
        Some(slot)
    }

    pub fn write(&mut self, key: i64, slot: Slot) -> bool {
        if !matches!(self.backing, Backing::Write(_)) || !scalar(slot) {
            return false;
        }
        let Some(index) = self.index(key) else {
            return false;
        };
        self.slots[index] = slot;
        self.dirty |= 1u64 << index;
        self.counts.writes = self.counts.writes.saturating_add(1);
        true
    }
}

impl<const CAPACITY: usize> Drop for Window<'_, '_, CAPACITY> {
    fn drop(&mut self) {
        if let Backing::Write(values) = &mut self.backing {
            let mut dirty = self.dirty;
            while dirty != 0 {
                let index = dirty.trailing_zeros() as usize;
                self.slots[index].write_back(&mut values[index]);
                dirty &= dirty - 1;
            }
        }
    }
}

pub(super) fn with_window<'gc, R, const CAPACITY: usize>(
    ctx: Context<'gc>,
    table: Table<'gc>,
    first: i64,
    length: usize,
    access: Access,
    counts: &mut Counts,
    body: impl FnOnce(&mut Window<'_, 'gc, CAPACITY>) -> R,
) -> Result<R, Refusal> {
    if CAPACITY > LIMIT || length == 0 || length > CAPACITY {
        return Err(Refusal::Capacity);
    }
    let start = usize::try_from(first.checked_sub(1).ok_or(Refusal::Bounds)?)
        .map_err(|_| Refusal::Bounds)?;
    let end = start.checked_add(length).ok_or(Refusal::Bounds)?;
    let inner = table.into_inner();
    let state = inner.try_borrow().map_err(|_| Refusal::Borrowed)?;
    if state.metatable.is_some() || state.intercept_all_writes {
        return Err(Refusal::Observed);
    }
    if state.raw_table.has_weak_keys() || state.raw_table.has_weak_values() {
        return Err(Refusal::Weak);
    }
    if access == Access::Write && state.readonly {
        return Err(Refusal::ReadOnly);
    }
    let values = state
        .raw_table
        .array()
        .get(start..end)
        .ok_or(Refusal::Bounds)?;
    if access == Access::Read {
        let mut window = Window::<CAPACITY>::new(Backing::Read(values), first, counts);
        return Ok(body(&mut window));
    }
    drop(state);
    let mut state = inner.try_borrow_mut(&ctx).map_err(|_| Refusal::Borrowed)?;
    let values = state
        .raw_table
        .array_mut()
        .get_mut(start..end)
        .ok_or(Refusal::Bounds)?;
    let mut window = Window::<CAPACITY>::new(Backing::Write(values), first, counts);
    Ok(body(&mut window))
}

#[cfg(test)]
mod tests;

mod native;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod lowering;
