use super::*;

pub(super) struct Analysis<'f> {
    function: &'f Function,
    slots: IrValue,
    helpers: &'f [(u32, FuncRef)],
    visited: BudgetVec<bool, BudgetAllocator>,
    pending: BudgetVec<Block, BudgetAllocator>,
    remaining: usize,
}

impl<'f> Analysis<'f> {
    pub(super) fn new(
        function: &'f Function,
        slots: IrValue,
        host: IrValue,
        registers: usize,
        helpers: &'f [(u32, FuncRef)],
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        scan(function, slots, host, registers, helpers, |_| Ok(()))?;
        let blocks = function.dfg.num_blocks();
        let mut visited = BudgetVec::new_in(allocator.clone());
        let mut pending = BudgetVec::new_in(allocator);
        visited
            .try_reserve_exact(blocks)
            .map_err(|_| JitError::ResourceLimit("terminal liveness blocks"))?;
        pending
            .try_reserve_exact(blocks)
            .map_err(|_| JitError::ResourceLimit("terminal liveness queue"))?;
        visited.resize(blocks, false);
        let remaining = function
            .dfg
            .num_insts()
            .checked_add(blocks)
            .and_then(|n| n.checked_mul(64))
            .ok_or(JitError::ResourceLimit("terminal liveness work"))?;
        Ok(Self {
            function,
            slots,
            helpers,
            visited,
            pending,
            remaining,
        })
    }

    pub(super) fn admits(&mut self, bits_store: Inst, index: u32) -> bool {
        if !matches!(self.function.dfg.insts[bits_store], InstructionData::Store {
            opcode: Opcode::Store, args, offset, ..
        } if args[1] == self.slots && i64::from(i32::from(offset)) == i64::from(index) * 16 + 8)
        {
            return false;
        }
        let Some(remaining) = self.remaining.checked_sub(self.visited.len()) else {
            return false;
        };
        self.remaining = remaining;
        self.visited.fill(false);
        self.pending.clear();
        let Some(next) = self.function.layout.next_inst(bits_store) else {
            return false;
        };
        if !self.path(next, index) {
            return false;
        }
        while let Some(block) = self.pending.pop() {
            let Some(first) = self.function.layout.first_inst(block) else {
                return false;
            };
            if !self.path(first, index) {
                return false;
            }
        }
        true
    }

    fn path(&mut self, mut inst: Inst, index: u32) -> bool {
        let f = self.function;
        loop {
            let Some(remaining) = self.remaining.checked_sub(1) else {
                return false;
            };
            self.remaining = remaining;
            match f.dfg.insts[inst] {
                InstructionData::Load { arg, offset, .. } if arg == self.slots => {
                    if i32::from(offset) as u32 / 16 == index {
                        return false;
                    }
                }
                InstructionData::Store { args, offset, .. } if args[1] == self.slots => {
                    if i32::from(offset) as u32 / 16 == index {
                        return false;
                    }
                }
                InstructionData::Call {
                    opcode: Opcode::Call,
                    func_ref,
                    ..
                } => {
                    let Some(&(kind, _)) = self
                        .helpers
                        .iter()
                        .find(|(_, reference)| *reference == func_ref)
                    else {
                        return false;
                    };
                    if kind != abi::HELPER_GET_UP_TABLE {
                        return true;
                    }
                }
                InstructionData::CallIndirect { .. } => return false,
                _ => {}
            }
            if let Some(next) = f.layout.next_inst(inst) {
                inst = next;
                continue;
            }
            if f.dfg.insts[inst].opcode() == Opcode::Return {
                return true;
            }
            let edges =
                f.dfg.insts[inst].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables);
            if edges.is_empty() {
                return false;
            }
            for edge in edges {
                let Some(remaining) = self.remaining.checked_sub(1) else {
                    return false;
                };
                self.remaining = remaining;
                let target = edge.block(&f.dfg.value_lists);
                let Some(visited) = self.visited.get_mut(target.as_u32() as usize) else {
                    return false;
                };
                if !*visited {
                    *visited = true;
                    self.pending.push(target);
                }
            }
            return true;
        }
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
