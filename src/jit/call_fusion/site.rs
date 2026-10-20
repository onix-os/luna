use super::super::{abi, access::Access, ir::Snapshot, leaf};
use crate::{
    opcode::Operation,
    types::{RegisterIndex, UpValueDescriptor},
};

#[cfg(test)]
mod tests;

mod binding;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(in crate::jit) mod kernel;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(in crate::jit) use binding::Input;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Leaf {
    width: u16,
    read: u8,
    result: u8,
    parent: u8,
}

impl Leaf {
    pub(super) fn recognize(
        source: &Snapshot,
        fixed_params: u8,
        varargs: bool,
        captures: &[UpValueDescriptor],
    ) -> Option<Self> {
        if source.operations.len() != 4
            || fixed_params != 1
            || varargs
            || source.upvalues != 1
            || source.prototypes != 0
            || !source.constants.is_empty()
            || !(2..=256).contains(&source.registers)
        {
            return None;
        }
        source.verify().ok()?;
        let [UpValueDescriptor::ParentLocal(parent)] = captures else {
            return None;
        };
        let pattern = leaf::Pattern::from_operations(&source.operations)?;
        if pattern.upvalue != 0
            || pattern.read.0 == 0
            || pattern.arithmetic != leaf::Arithmetic::Add
            || pattern.right != leaf::Operand::Register(RegisterIndex(0))
            || !matches!(source.operations[3], Operation::Return { count, .. } if count.to_constant() == Some(0))
        {
            return None;
        }
        Some(Self {
            width: source.registers as u16,
            read: pattern.read.0,
            result: pattern.result.0,
            parent: parent.0,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Argument {
    Register(u8),
    Integer(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) struct Site {
    leaf: Leaf,
    registers: u16,
    region_pc: u32,
    return_pc: u32,
    prototype: u8,
    holder: u8,
    temporary: u8,
    argument: Argument,
}

impl Site {
    pub(super) fn recognize(source: &Snapshot, leaf: Leaf, prototype: u8) -> Option<Self> {
        if source.operations.len() > 256
            || source.constants.len() > 256
            || usize::from(prototype) >= source.prototypes
            || usize::from(leaf.parent) >= source.registers
        {
            return None;
        }
        source.verify().ok()?;
        let mut creation = None;
        let mut call = None;
        for (pc, &op) in source.operations.iter().enumerate() {
            match op {
                Operation::Closure { dest, proto } if proto.0 == prototype => {
                    if creation.replace((pc, dest.0)).is_some() {
                        return None;
                    }
                }
                Operation::Closure { .. } => return None,
                Operation::Call {
                    func,
                    args,
                    returns,
                } => {
                    if args.to_constant() != Some(1)
                        || returns.to_constant() != Some(0)
                        || call.replace((pc, func.0)).is_some()
                    {
                        return None;
                    }
                }
                _ => {}
            }
        }
        let (creation_pc, holder) = creation?;
        let (call_pc, temporary) = call?;
        let region_pc = call_pc.checked_sub(2)?;
        if creation_pc >= region_pc
            || holder >= temporary
            || leaf.parent >= temporary
            || holder == leaf.parent
            || call_pc + 1 >= source.operations.len()
            || !matches!(source.operations[region_pc], Operation::Move { dest, source } if dest.0 == temporary && source.0 == holder)
        {
            return None;
        }
        let argument_register = temporary.checked_add(1)?;
        let argument = match source.operations[region_pc + 1] {
            Operation::Move { dest, source }
                if dest.0 == argument_register && source.0 < temporary =>
            {
                Argument::Register(source.0)
            }
            Operation::LoadConstant { dest, constant } if dest.0 == argument_register => {
                let value = source.constants[usize::from(constant.0)];
                if value.tag != abi::INTEGER {
                    return None;
                }
                Argument::Integer(value.bits as i64)
            }
            _ => return None,
        };
        for (pc, &op) in source.operations.iter().enumerate() {
            let jump = match op {
                Operation::Jump {
                    offset,
                    close_upvalues,
                } if close_upvalues.is_none() => Some(offset),
                Operation::NumericForPrep { jump, .. } | Operation::NumericForLoop { jump, .. } => {
                    Some(jump)
                }
                _ => None,
            };
            if let Some(offset) = jump {
                let target = (pc + 1).checked_add_signed(isize::from(offset))?;
                if target == region_pc + 1 || target == call_pc {
                    return None;
                }
            }
            match op {
                Operation::Closure { .. } if pc == creation_pc => continue,
                Operation::Call { .. } if pc == call_pc => continue,
                Operation::Return { .. } => continue,
                Operation::Move { .. }
                | Operation::LoadConstant { .. }
                | Operation::LoadNil { .. }
                | Operation::LoadBool {
                    skip_next: false, ..
                }
                | Operation::Not { .. }
                | Operation::NumericForPrep { .. }
                | Operation::NumericForLoop { .. }
                | Operation::Add { .. }
                | Operation::Sub { .. }
                | Operation::Mul { .. }
                | Operation::Div { .. } => {}
                Operation::Jump { close_upvalues, .. } if close_upvalues.is_none() => {}
                _ => return None,
            }
            if Access::new(op, source).writes.contains(holder) {
                return None;
            }
        }
        Some(Self {
            leaf,
            registers: source.registers as u16,
            region_pc: region_pc as u32,
            return_pc: (call_pc + 1) as u32,
            prototype,
            holder,
            temporary,
            argument,
        })
    }

    pub(super) fn materialize(self, slots: &mut [abi::Slot]) -> Option<i64> {
        if slots.len() != usize::from(self.registers) {
            return None;
        }
        let holder = *slots.get(usize::from(self.holder))?;
        let sum = *slots.get(usize::from(self.leaf.parent))?;
        let argument = match self.argument {
            Argument::Register(index) => *slots.get(usize::from(index))?,
            Argument::Integer(value) => abi::Slot {
                tag: abi::INTEGER,
                bits: value as u64,
            },
        };
        if holder.tag != abi::REFERENCE
            || holder.bits != 0
            || sum.tag != abi::INTEGER
            || argument.tag != abi::INTEGER
        {
            return None;
        }
        let result = abi::Slot {
            tag: abi::INTEGER,
            bits: sum.bits.wrapping_add(argument.bits),
        };
        for (index, slot) in slots[usize::from(self.temporary)..].iter_mut().enumerate() {
            *slot = abi::Slot {
                tag: abi::NIL,
                bits: 0,
            };
            if index < usize::from(self.leaf.width) {
                if index == 0 {
                    *slot = argument;
                }
                if index == usize::from(self.leaf.read) {
                    *slot = sum;
                }
                if index == usize::from(self.leaf.result) {
                    *slot = result;
                }
            }
        }
        slots[usize::from(self.leaf.parent)] = result;
        Some(result.bits as i64)
    }
}
