use super::{Plan, Snapshot};
use crate::{jit::abi, opcode::Operation, Value};

#[derive(Clone, Copy, Debug)]
pub(in crate::jit) struct Source(Plan);

impl Source {
    pub(in crate::jit) fn admit(source: &Snapshot) -> Option<Self> {
        let plan = Plan::recognize(source)?;
        for op in &source.operations {
            if let Operation::LoadConstant { constant, .. } = op {
                if source.constants[usize::from(constant.0)].tag > abi::NUMBER {
                    return None;
                }
            }
        }
        Some(Self(plan))
    }

    pub(in crate::jit) fn plan(self) -> Plan {
        self.0
    }

    pub(in crate::jit) fn fits(self, slots: &[abi::Slot], frame: &[Value<'_>]) -> bool {
        let width = usize::from(self.0.registers);
        slots.len() == width && frame.len() >= width && slots.iter().all(|s| s.tag <= abi::NUMBER)
    }
}

#[cfg(test)]
mod tests;
