use crate::opcode::{Operation, RCIndex};

use super::*;
use crate::jit::ir::Snapshot;

pub(super) const MAX_OPERATIONS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) struct Plan {
    pub start: usize,
    pub end: usize,
    pub base: u8,
    pub table: u8,
    pub access: Access,
}

impl Plan {
    pub(in crate::jit) fn new(source: &Snapshot, end: usize) -> Option<Self> {
        let Operation::NumericForLoop { base, jump } = *source.operations.get(end)? else {
            return None;
        };
        source.verify().ok()?;
        let start = end.checked_add(1)?.checked_add_signed(isize::from(jump))?;
        if start >= end || end - start + 1 > MAX_OPERATIONS {
            return None;
        }
        let mut table = None;
        let mut access = Access::Read;
        let mut destinations = [false; 256];
        let scalar_operand = |operand| match operand {
            RCIndex::Register(_) => true,
            RCIndex::Constant(index) => scalar(source.constants[usize::from(index.0)]),
        };
        for operation in &source.operations[start..end] {
            if let Operation::GetTable {
                table: receiver,
                key,
                ..
            }
            | Operation::SetTable {
                table: receiver,
                key,
                ..
            } = *operation
            {
                if !matches!(key, RCIndex::Register(index) if index.0 == base.0 + 3)
                    || table.is_some_and(|previous| previous != receiver.0)
                {
                    return None;
                }
                table = Some(receiver.0);
            }
            let destination = match *operation {
                Operation::GetTable { dest, .. } => Some(dest.0),
                Operation::SetTable { value, .. } => {
                    if !scalar_operand(value) {
                        return None;
                    }
                    access = Access::Write;
                    None
                }
                Operation::Add { dest, left, right }
                | Operation::Sub { dest, left, right }
                | Operation::Mul { dest, left, right } => {
                    if !scalar_operand(left) || !scalar_operand(right) {
                        return None;
                    }
                    Some(dest.0)
                }
                Operation::Move { dest, .. } => Some(dest.0),
                Operation::LoadConstant { dest, constant } => {
                    if !scalar(source.constants[usize::from(constant.0)]) {
                        return None;
                    }
                    Some(dest.0)
                }
                _ => return None,
            };
            if let Some(destination) = destination {
                if (base.0..=base.0 + 3).contains(&destination) {
                    return None;
                }
                destinations[usize::from(destination)] = true;
            }
        }
        let table = table?;
        if destinations[usize::from(table)] || (base.0..=base.0 + 3).contains(&table) {
            return None;
        }
        Some(Self {
            start,
            end,
            base: base.0,
            table,
            access,
        })
    }

    pub(super) fn window(
        self,
        pc: usize,
        slots: &[Slot],
        array_length: usize,
    ) -> Option<(i64, usize)> {
        if !(self.start..=self.end).contains(&pc) {
            return None;
        }
        let base = usize::from(self.base);
        let control = slots.get(base..base.checked_add(4)?)?;
        if control.iter().any(|slot| slot.tag != abi::INTEGER) {
            return None;
        }
        let key = control[3].bits as i64;
        let index = usize::try_from(key.checked_sub(1)?).ok()?;
        if index >= array_length {
            return None;
        }
        let first = if (control[2].bits as i64) < 0 {
            key.saturating_sub(LIMIT as i64 - 1).max(1)
        } else {
            key
        };
        let start = usize::try_from(first - 1).ok()?;
        Some((first, (array_length - start).min(LIMIT)))
    }

    pub(super) fn budget_window(
        self,
        pc: usize,
        slots: &[Slot],
        array_length: usize,
        budget: u32,
    ) -> Option<(i64, usize)> {
        let original = self.window(pc, slots, array_length)?;
        let base = usize::from(self.base);
        let control = &slots[base..base + 4];
        if control[0].bits != control[3].bits || budget == 0 {
            return Some(original);
        }
        let fuel = budget.min(LIMIT as u32) as usize;
        let Some(until_loop) = (self.end - pc).checked_add(1) else {
            return Some(original);
        };
        let Some(span) = (self.end - self.start).checked_add(1) else {
            return Some(original);
        };
        let advances = if fuel > until_loop {
            1 + (fuel - until_loop - 1) / span
        } else {
            0
        };
        let key = control[3].bits as i64;
        let Some(last) = (control[2].bits as i64)
            .checked_mul(advances as i64)
            .and_then(|delta| key.checked_add(delta))
        else {
            return Some(original);
        };
        let Some(high) = original.0.checked_add(original.1 as i64 - 1) else {
            return Some(original);
        };
        let first = key.min(last).max(original.0);
        let last = key.max(last).min(high);
        Some((first, (last - first + 1) as usize))
    }
}

#[cfg(test)]
mod tests;
