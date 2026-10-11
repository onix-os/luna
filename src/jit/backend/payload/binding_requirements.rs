use super::*;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct Mask([u64; 4]);

impl Mask {
    pub(super) fn words(self) -> [u64; 4] {
        self.0
    }

    pub(super) fn contains(self, index: usize) -> bool {
        index < 256 && self.0[index / 64] & (1 << (index % 64)) != 0
    }

    fn insert(&mut self, index: usize) {
        self.0[index / 64] |= 1 << (index % 64);
    }

    fn union(&mut self, other: Self) {
        for (left, right) in self.0.iter_mut().zip(other.0) {
            *left |= right;
        }
    }

    fn before(self, definitions: Self, uses: Self) -> Self {
        Self(std::array::from_fn(|i| {
            uses.0[i] | (self.0[i] & !definitions.0[i])
        }))
    }
}

#[derive(Clone, Copy, Default)]
struct Facts {
    uses: Mask,
    definitions: Mask,
    live: Mask,
}

pub(in crate::jit::backend) struct Requirements {
    entries: BudgetVec<Mask, BudgetAllocator>,
}

fn refused() -> JitError {
    JitError::ResourceLimit("payload entry binding analysis")
}

fn workspace<T: Clone>(
    count: usize,
    value: T,
    allocator: BudgetAllocator,
) -> Result<BudgetVec<T, BudgetAllocator>, JitError> {
    let mut result = BudgetVec::new_in(allocator);
    result.try_reserve_exact(count).map_err(|_| refused())?;
    result.resize(count, value);
    Ok(result)
}

fn work(f: &Function, entries: usize) -> Result<usize, JitError> {
    f.dfg
        .num_insts()
        .checked_add(f.dfg.num_blocks())
        .and_then(|n| n.checked_add(entries))
        .and_then(|n| n.checked_mul(512))
        .ok_or_else(refused)
}

fn charge(remaining: &mut usize) -> Result<(), JitError> {
    *remaining = remaining.checked_sub(1).ok_or_else(refused)?;
    Ok(())
}

fn edges(f: &Function, block: Block) -> Result<&[cranelift_codegen::ir::BlockCall], JitError> {
    if !f.layout.is_block_inserted(block) {
        return Err(invalid());
    }
    let last = f.layout.last_inst(block).ok_or_else(invalid)?;
    let data = &f.dfg.insts[last];
    let edges = data.branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables);
    if edges.is_empty() && data.opcode() != Opcode::Return {
        return Err(invalid());
    }
    if edges
        .iter()
        .any(|edge| !f.layout.is_block_inserted(edge.block(&f.dfg.value_lists)))
    {
        return Err(invalid());
    }
    Ok(edges)
}

impl Requirements {
    pub(in crate::jit::backend) fn into_metadata(
        self,
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        let mut entries = BudgetVec::new_in(allocator);
        entries
            .try_reserve_exact(self.entries.len())
            .map_err(|_| refused())?;
        entries.extend(self.entries.iter().copied());
        Ok(Self { entries })
    }

    pub(in crate::jit::backend) fn new(
        f: &Function,
        slots: IrValue,
        host: IrValue,
        registers: usize,
        helpers: &[(u32, FuncRef)],
        headers: &[Block],
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        let mut facts = workspace(f.dfg.num_blocks(), Facts::default(), allocator.clone())?;
        scan_with_tags(f, slots, host, registers, helpers, true, |rewrite| {
            match rewrite {
                Rewrite::Read(inst, index) => {
                    let block = f.layout.inst_block(inst).ok_or_else(invalid)?;
                    let facts = &mut facts[block.as_u32() as usize];
                    if !facts.definitions.contains(index as usize) {
                        facts.uses.insert(index as usize);
                    }
                }
                Rewrite::Write {
                    bits_store, index, ..
                } => {
                    let block = f.layout.inst_block(bits_store).ok_or_else(invalid)?;
                    facts[block.as_u32() as usize]
                        .definitions
                        .insert(index as usize);
                }
                Rewrite::Helper(..) => {}
            }
            Ok(())
        })?;
        let mut remaining = work(f, headers.len())?;
        loop {
            let mut changed = false;
            for block in std::iter::successors(f.layout.last_block(), |&b| f.layout.prev_block(b)) {
                charge(&mut remaining)?;
                let mut after = Mask::default();
                for edge in edges(f, block)? {
                    charge(&mut remaining)?;
                    after.union(facts[edge.block(&f.dfg.value_lists).as_u32() as usize].live);
                }
                let facts = &mut facts[block.as_u32() as usize];
                let live = after.before(facts.definitions, facts.uses);
                changed |= live != facts.live;
                facts.live = live;
            }
            if !changed {
                break;
            }
        }
        let mut entries = BudgetVec::new_in(allocator.clone());
        entries
            .try_reserve_exact(headers.len())
            .map_err(|_| refused())?;
        for &header in headers {
            edges(f, header)?;
            entries.push(facts[header.as_u32() as usize].live);
        }
        drop(facts);
        let requirements = Self { entries };
        requirements.verify(f, slots, host, registers, helpers, headers, allocator)?;
        Ok(requirements)
    }

    pub(super) fn get(&self, pc: usize) -> Option<Mask> {
        self.entries.get(pc).copied()
    }

    fn verify(
        &self,
        f: &Function,
        slots: IrValue,
        host: IrValue,
        registers: usize,
        helpers: &[(u32, FuncRef)],
        headers: &[Block],
        allocator: BudgetAllocator,
    ) -> Result<(), JitError> {
        let mut reads = Mask::default();
        scan_with_tags(f, slots, host, registers, helpers, true, |rewrite| {
            if let Rewrite::Read(_, index) = rewrite {
                reads.insert(index as usize);
            }
            Ok(())
        })?;
        if self.entries.len() != headers.len() {
            return Err(invalid());
        }
        let mut visited = workspace(f.dfg.num_blocks(), false, allocator.clone())?;
        let mut pending = BudgetVec::new_in(allocator);
        pending
            .try_reserve_exact(visited.len())
            .map_err(|_| refused())?;
        let mut remaining = work(f, headers.len())?;
        for (&header, &mask) in headers.iter().zip(&self.entries) {
            edges(f, header)?;
            if (registers..256).any(|i| mask.contains(i)) {
                return Err(invalid());
            }
            for index in (0..registers).filter(|&i| reads.contains(i) && !mask.contains(i)) {
                remaining = remaining.checked_sub(visited.len()).ok_or_else(refused)?;
                visited.fill(false);
                pending.push(header);
                visited[header.as_u32() as usize] = true;
                while let Some(block) = pending.pop() {
                    charge(&mut remaining)?;
                    let mut defined = false;
                    for inst in f.layout.block_insts(block) {
                        charge(&mut remaining)?;
                        match f.dfg.insts[inst] {
                            InstructionData::Load { arg, offset, .. }
                                if arg == slots && i32::from(offset) as usize / 16 == index =>
                            {
                                return Err(invalid());
                            }
                            InstructionData::Store { args, offset, .. }
                                if args[1] == slots
                                    && i32::from(offset) as usize == index * 16 + 8 =>
                            {
                                defined = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                    if defined {
                        continue;
                    }
                    for edge in edges(f, block)? {
                        charge(&mut remaining)?;
                        let target = edge.block(&f.dfg.value_lists);
                        let at = target.as_u32() as usize;
                        if !visited[at] {
                            visited[at] = true;
                            pending.push(target);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "binding_requirements_tests.rs"]
mod tests;
