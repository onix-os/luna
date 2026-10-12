use crate::opcode::Operation;

use super::{calls, ir::Snapshot, work, JitError};

const MAX_REGISTERS: usize = 16;
const MAX_OPERATIONS: usize = 32;

pub(super) struct Plan<'a> {
    pair: calls::Plan<'a>,
    start: usize,
    call: usize,
    end: usize,
    base: usize,
}

impl<'a> Plan<'a> {
    fn region(caller: &Snapshot, callee: &Snapshot, call: usize) -> Option<(usize, usize, usize)> {
        if caller.registers > MAX_REGISTERS || callee.registers > MAX_REGISTERS {
            return None;
        }
        let end = call.checked_add(1)?;
        let Operation::NumericForLoop { base, jump } = *caller.operations.get(end)? else {
            return None;
        };
        let start = end.checked_add(1)?.checked_add_signed(isize::from(jump))?;
        if start > call || end + 1 >= caller.operations.len() || end - start + 1 > MAX_OPERATIONS {
            return None;
        }
        if !caller.operations[start..call]
            .iter()
            .all(|operation| matches!(operation, Operation::Move { .. }))
        {
            return None;
        }
        let Operation::Return { count, .. } = *callee.operations.get(3)? else {
            return None;
        };
        if count.to_constant() != Some(0) {
            return None;
        }
        Some((start, end, usize::from(base.0)))
    }

    pub(super) fn new(
        caller: &'a Snapshot,
        callee: &'a Snapshot,
        call: usize,
        limits: work::Limits,
    ) -> Result<Self, JitError> {
        let pair = calls::Plan::new(caller, callee, call, limits)?;
        let (start, end, base) = Self::region(caller, callee, call)
            .ok_or_else(|| JitError::Compilation("unsupported inline loop region".into()))?;
        Ok(Self {
            pair,
            start,
            call,
            end,
            base,
        })
    }

    pub(super) fn verify(&self, caller: &Snapshot, callee: &Snapshot) -> Result<(), JitError> {
        self.pair.verify(caller, callee)?;
        if Self::region(caller, callee, self.call) != Some((self.start, self.end, self.base)) {
            return Err(JitError::Compilation("inline loop source mismatch".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
