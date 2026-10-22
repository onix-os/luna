use crate::{opcode::Operation, opcode::RCIndex, Value};

use super::{abi, ir::Snapshot};

#[derive(Clone, Copy)]
enum Operand {
    Register(u8),
    Numeric(bool),
}

impl Operand {
    fn recognize(index: RCIndex, source: &Snapshot, base: u8) -> Option<Self> {
        match index {
            RCIndex::Register(index) => (!(usize::from(base)..usize::from(base) + 4)
                .contains(&usize::from(index.0)))
            .then_some(Self::Register(index.0)),
            RCIndex::Constant(index) => {
                let slot = source.constants.get(usize::from(index.0))?;
                Some(Self::Numeric(matches!(
                    slot.tag,
                    abi::INTEGER | abi::NUMBER
                )))
            }
        }
    }

    fn numeric(self, frame: &[Value<'_>]) -> Option<bool> {
        match self {
            Self::Register(index) => frame
                .get(usize::from(index))
                .map(|value| matches!(value, Value::Integer(_) | Value::Number(_))),
            Self::Numeric(numeric) => Some(numeric),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Plan {
    pc: usize,
    base: u8,
    operands: [Operand; 2],
}

impl Plan {
    pub(super) fn recognize(source: &Snapshot) -> Option<Self> {
        source.verify().ok()?;
        let mut result = None;
        for (pc, op) in source.operations.iter().enumerate() {
            let Operation::NumericForLoop { base, jump: -2 } = op else {
                continue;
            };
            let [left, right] = match source.operations.get(pc.checked_sub(1)?)? {
                Operation::Add { left, right, .. }
                | Operation::Sub { left, right, .. }
                | Operation::Mul { left, right, .. }
                | Operation::Div { left, right, .. } => [*left, *right],
                _ => continue,
            };
            let Some(left) = Operand::recognize(left, source, base.0) else {
                continue;
            };
            let Some(right) = Operand::recognize(right, source, base.0) else {
                continue;
            };
            if matches!(
                (left, right),
                (Operand::Numeric(true), Operand::Numeric(true))
            ) {
                continue;
            }
            if result.is_some() {
                return None;
            }
            result = Some(Self {
                pc,
                base: base.0,
                operands: [left, right],
            });
        }
        result
    }

    pub(super) fn refuses(self, frame: &[Value<'_>], pc: usize, budget: u32) -> bool {
        if budget == 0 || (pc != self.pc && pc != self.pc - 1) {
            return false;
        }
        let [Some(left), Some(right)] = self.operands.map(|operand| operand.numeric(frame)) else {
            return false;
        };
        if left && right {
            return false;
        }
        if pc != self.pc {
            return true;
        }
        let base = usize::from(self.base);
        let Some(controls) = frame.get(base..base + 4) else {
            return false;
        };
        let (Value::Integer(index), Value::Integer(limit), Value::Integer(step)) =
            (controls[0], controls[1], controls[2])
        else {
            return false;
        };
        if step == 0 {
            return false;
        }
        let Some(next) = index.checked_add(step) else {
            return false;
        };
        if step > 0 {
            next <= limit
        } else {
            next >= limit
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(
    test,
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod native;
