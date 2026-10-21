use super::{access::Access, ir::Snapshot};
use crate::opcode::{Operation, RCIndex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Plan {
    helper_pc: u32,
    call_pc: u32,
    destination: u8,
    upvalue: u8,
    constant: u8,
    registers: u16,
}

impl Plan {
    pub(in crate::jit) fn helper_pc(self) -> u32 {
        self.helper_pc
    }
    pub(in crate::jit) fn destination(self) -> u8 {
        self.destination
    }

    pub fn recognize(source: &Snapshot) -> Option<Self> {
        source.verify().ok()?;
        source.prepared_keys.as_ref()?;
        let mut lookup = None;
        for (pc, &op) in source.operations.iter().enumerate() {
            match op {
                Operation::GetUpTable {
                    dest,
                    table,
                    key: RCIndex::Constant(key),
                } => {
                    if lookup.is_some() {
                        return None;
                    }
                    lookup = Some((pc, dest.0, table.0, key.0));
                }
                Operation::Move { .. } | Operation::LoadConstant { .. } => {}
                Operation::LoadBool { .. }
                | Operation::LoadNil { .. }
                | Operation::NumericForPrep { .. }
                | Operation::NumericForLoop { .. }
                | Operation::Test { .. }
                | Operation::TestSet { .. }
                | Operation::Not { .. }
                | Operation::Add { .. }
                | Operation::Sub { .. }
                | Operation::Mul { .. }
                | Operation::Div { .. }
                | Operation::Call { .. }
                | Operation::TailCall { .. }
                | Operation::Return { .. } => {}
                Operation::Jump { close_upvalues, .. } if close_upvalues.is_none() => {}
                _ => return None,
            }
        }
        let (helper_pc, destination, upvalue, constant) = lookup?;
        for pc in helper_pc + 1..source.operations.len() {
            let op = source.operations[pc];
            if let Operation::Call { func, .. } = op {
                return (func.0 == destination).then_some(Self {
                    helper_pc: helper_pc as u32,
                    call_pc: pc as u32,
                    destination,
                    upvalue,
                    constant,
                    registers: source.registers as u16,
                });
            }
            match op {
                Operation::Move { .. }
                | Operation::LoadConstant { .. }
                | Operation::LoadNil { .. }
                | Operation::LoadBool {
                    skip_next: false, ..
                } => {}
                _ => return None,
            }
            let access = Access::new(op, source);
            if access.reads.contains(destination) || access.writes.contains(destination) {
                return None;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(in crate::jit) mod scalar_entry;

pub(super) mod binding;

#[cfg(test)]
mod publication;

#[cfg(all(
    test,
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) mod runtime;
