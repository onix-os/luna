use crate::{
    opcode::{Operation, RCIndex},
    FunctionPrototype,
};

use super::{
    abi::{self, Slot},
    resources::BudgetAllocator,
    JitError,
};
use allocator_api2::vec::Vec;

pub(crate) struct Snapshot {
    pub(super) operations: Vec<Operation, BudgetAllocator>,
    pub(super) constants: Vec<Slot, BudgetAllocator>,
    pub(super) registers: usize,
    pub(super) upvalues: usize,
    pub(super) prototypes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RegisterIndex, VarCount};

    #[test]
    fn fixed_set_list_sources_must_fit_the_declared_frame() {
        let mut snapshot = Snapshot {
            operations: super::super::resources::owned(&[
                Operation::SetList {
                    base: RegisterIndex(0),
                    count: VarCount::constant(2),
                },
                Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[]),
            registers: 4,
            upvalues: 0,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        snapshot.operations[0] = Operation::SetList {
            base: RegisterIndex(0),
            count: VarCount::constant(3),
        };
        assert!(matches!(snapshot.verify(), Err(JitError::Compilation(_))));
    }
}

impl Snapshot {
    #[cfg(test)]
    pub fn new(
        proto: &FunctionPrototype<'_>,
        max_instructions: usize,
        max_bytes: usize,
    ) -> Result<Self, JitError> {
        Self::new_in(
            proto,
            max_instructions,
            BudgetAllocator(super::resources::Ledger::new(max_bytes)),
        )
    }

    pub fn new_in(
        proto: &FunctionPrototype<'_>,
        max_instructions: usize,
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        let bytes = proto
            .opcodes
            .len()
            .checked_mul(std::mem::size_of::<Operation>())
            .and_then(|bytes| {
                bytes.checked_add(
                    proto
                        .constants
                        .len()
                        .checked_mul(std::mem::size_of::<Slot>())?,
                )
            })
            .ok_or(JitError::ResourceLimit("snapshot size overflow"))?;
        if proto.opcodes.len() > max_instructions
            || proto.stack_size > 256
            || !allocator.0.fits(bytes)
        {
            return Err(JitError::ResourceLimit("prototype snapshot"));
        }
        let mut operations = Vec::new_in(allocator.clone());
        operations
            .try_reserve_exact(proto.opcodes.len())
            .map_err(|_| JitError::ResourceLimit("prototype snapshot"))?;
        operations.extend(proto.opcodes.iter().map(|op| op.decode()));
        let mut constants = Vec::new_in(allocator);
        constants
            .try_reserve_exact(proto.constants.len())
            .map_err(|_| JitError::ResourceLimit("prototype snapshot"))?;
        constants.extend(proto.constants.iter().copied().map(Slot::from_constant));
        let snapshot = Self {
            operations,
            constants,
            registers: usize::from(proto.stack_size),
            upvalues: proto.upvalues.len(),
            prototypes: proto.prototypes.len(),
        };
        snapshot.verify()?;
        Ok(snapshot)
    }

    pub(super) fn verify(&self) -> Result<(), JitError> {
        use Operation::*;
        if self.registers > 256 || self.operations.len() > u32::MAX as usize {
            return Err(JitError::Compilation("invalid snapshot capacity".into()));
        }
        for (index, slot) in self.constants.iter().enumerate() {
            let valid = match slot.tag {
                abi::NIL | abi::REFERENCE => slot.bits == 0,
                abi::BOOLEAN => slot.bits <= 1,
                abi::INTEGER | abi::NUMBER => true,
                _ => false,
            };
            if !valid {
                return Err(JitError::Compilation(format!(
                    "invalid constant at index {index}"
                )));
            }
        }
        let reg = |index: u8| usize::from(index) < self.registers;
        let range = |base: u8, count: usize| {
            usize::from(base)
                .checked_add(count)
                .is_some_and(|end| end <= self.registers)
        };
        let rc = |operand: RCIndex| match operand {
            RCIndex::Register(index) => reg(index.0),
            RCIndex::Constant(index) => usize::from(index.0) < self.constants.len(),
        };
        for (pc, op) in self.operations.iter().copied().enumerate() {
            let valid = match op {
                Move { dest, source }
                | Not { dest, source }
                | Minus { dest, source }
                | Length { dest, source }
                | BitNot { dest, source } => reg(dest.0) && reg(source.0),
                LoadConstant { dest, constant } => {
                    reg(dest.0) && usize::from(constant.0) < self.constants.len()
                }
                LoadBool { dest, .. } => reg(dest.0),
                LoadNil { dest, count } => range(dest.0, usize::from(count)),
                NewTable { dest, .. } => reg(dest.0),
                GetUpValue { dest, source } => reg(dest.0) && usize::from(source.0) < self.upvalues,
                Closure { dest, proto } => reg(dest.0) && usize::from(proto.0) < self.prototypes,
                GetTable { dest, table, key } => reg(dest.0) && reg(table.0) && rc(key),
                SetTable { table, key, value } => reg(table.0) && rc(key) && rc(value),
                GetUpTable { dest, table, key } => {
                    reg(dest.0) && rc(key) && usize::from(table.0) < self.upvalues
                }
                SetUpTable { table, key, value } => {
                    rc(key) && rc(value) && usize::from(table.0) < self.upvalues
                }
                SetList { base, count } => {
                    range(base.0, 2 + usize::from(count.to_constant().unwrap_or(0)))
                }
                Call {
                    func,
                    args,
                    returns,
                } => {
                    range(func.0, 1 + usize::from(args.to_constant().unwrap_or(0)))
                        && range(func.0, usize::from(returns.to_constant().unwrap_or(0)))
                }
                TailCall { func, args } => {
                    range(func.0, 1 + usize::from(args.to_constant().unwrap_or(0)))
                }
                Return { start, count } | VarArgs { dest: start, count } => {
                    range(start.0, usize::from(count.to_constant().unwrap_or(0)))
                }
                MarkToBeClosed { source } => reg(source.0),
                SetUpValue { dest, source } => reg(source.0) && usize::from(dest.0) < self.upvalues,
                Jump { close_upvalues, .. } => close_upvalues.to_u8().is_none_or(reg),
                Test { value, .. } => reg(value.0),
                TestSet { dest, value, .. } => reg(dest.0) && reg(value.0),
                NumericForPrep { base, .. } | NumericForLoop { base, .. } => range(base.0, 4),
                GenericForCall { base, var_count } => range(base.0, 3 + usize::from(var_count)),
                GenericForLoop { base, .. } => range(base.0, 2),
                Method { base, table, key } => range(base.0, 2) && reg(table.0) && rc(key),
                Concat {
                    dest,
                    source,
                    count,
                } => reg(dest.0) && range(source.0, usize::from(count)),
                Eq { left, right, .. } | Less { left, right, .. } | LessEq { left, right, .. } => {
                    rc(left) && rc(right)
                }
                Add { dest, left, right }
                | Sub { dest, left, right }
                | Mul { dest, left, right }
                | Div { dest, left, right }
                | IDiv { dest, left, right }
                | Mod { dest, left, right }
                | Pow { dest, left, right }
                | BitAnd { dest, left, right }
                | BitOr { dest, left, right }
                | BitXor { dest, left, right }
                | ShiftLeft { dest, left, right }
                | ShiftRight { dest, left, right } => reg(dest.0) && rc(left) && rc(right),
            };
            if !valid {
                return Err(JitError::Compilation(format!("invalid operand at PC {pc}")));
            }
            super::flow::successors(op, pc, self.operations.len())?;
        }
        if self.operations.is_empty() {
            return Err(JitError::Compilation("empty prototype".into()));
        }
        Ok(())
    }
}
