use cranelift_codegen::ir::{
    condcodes::IntCC, types, Block, BlockArg, BlockCall, Function, Inst, InstructionData,
    MemFlagsData, Opcode, Type, Value, ValueDef,
};

use super::{abi, JitError, Operation, RCIndex, Region, Snapshot, MAX_OPERATIONS, MAX_REGISTERS};

fn invalid() -> JitError {
    JitError::Compilation("integer loop translation".into())
}

fn require(valid: bool) -> Result<(), JitError> {
    if valid {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn call(
    function: &Function,
    actual: BlockCall,
    block: Block,
    args: &[Value],
) -> Result<(), JitError> {
    require(
        actual.block(&function.dfg.value_lists) == block
            && actual
                .args(&function.dfg.value_lists)
                .eq(args.iter().copied().map(BlockArg::Value)),
    )
}

fn preserved(before: &Function, after: &Function, generic: Block) -> Result<(), JitError> {
    require(
        before.name == after.name
            && before.params == after.params
            && before.signature == after.signature
            && before.sized_stack_slots == after.sized_stack_slots
            && before.dynamic_stack_slots == after.dynamic_stack_slots
            && before.global_values == after.global_values
            && before.stack_limit == after.stack_limit
            && before.srclocs == after.srclocs
            && before.debug_tags == after.debug_tags
            && before.dfg.dynamic_types == after.dfg.dynamic_types
            && before.dfg.signatures == after.dfg.signatures
            && before.dfg.ext_funcs == after.dfg.ext_funcs
            && before.dfg.constants == after.dfg.constants
            && before.dfg.immediates == after.dfg.immediates
            && before.dfg.exception_tables == after.dfg.exception_tables
            && before.dfg.mem_flags == after.dfg.mem_flags
            && before.dfg.alias_regions == after.dfg.alias_regions
            && before.dfg.values_labels == after.dfg.values_labels,
    )?;
    require(
        after
            .layout
            .blocks()
            .take(before.layout.blocks().count())
            .eq(before.layout.blocks()),
    )?;
    for block in before.dfg.blocks.iter() {
        require(before.dfg.block_params(block) == after.dfg.block_params(block))?;
    }
    for (value, definition) in before.dfg.values_and_defs() {
        require(
            after.dfg.value_is_real(value)
                && after.dfg.value_def(value) == definition
                && after.dfg.value_type(value) == before.dfg.value_type(value),
        )?;
    }
    for block in before.layout.blocks() {
        let target = if Some(block) == before.layout.entry_block() {
            generic
        } else {
            block
        };
        require(
            before
                .layout
                .block_insts(block)
                .eq(after.layout.block_insts(target)),
        )?;
        for inst in before.layout.block_insts(block) {
            require(
                before.dfg.insts[inst] == after.dfg.insts[inst]
                    && before.dfg.inst_args(inst) == after.dfg.inst_args(inst)
                    && before.dfg.inst_results(inst) == after.dfg.inst_results(inst)
                    && before.dfg.user_stack_map_entries(inst)
                        == after.dfg.user_stack_map_entries(inst),
            )?;
            require(
                before
                    .dfg
                    .inst_values(inst)
                    .map(|v| before.dfg.resolve_aliases(v))
                    .eq(after
                        .dfg
                        .inst_values(inst)
                        .map(|v| after.dfg.resolve_aliases(v))),
            )?;
            let old = before.dfg.insts[inst]
                .branch_destination(&before.dfg.jump_tables, &before.dfg.exception_tables);
            let new = after.dfg.insts[inst]
                .branch_destination(&after.dfg.jump_tables, &after.dfg.exception_tables);
            require(old.len() == new.len())?;
            for (old, new) in old.iter().zip(new) {
                require(
                    old.block(&before.dfg.value_lists) == new.block(&after.dfg.value_lists)
                        && old
                            .args(&before.dfg.value_lists)
                            .eq(new.args(&after.dfg.value_lists)),
                )?;
            }
        }
    }
    require(after.dfg.jump_tables.len() == before.dfg.jump_tables.len() + 1)?;
    for (id, table) in before.dfg.jump_tables.iter() {
        require(table == &after.dfg.jump_tables[id])?;
    }
    Ok(())
}

struct Scan<'a> {
    function: &'a Function,
    next: Option<Inst>,
    first_new: usize,
}

impl<'a> Scan<'a> {
    fn new(function: &'a Function, block: Block, first_new: usize) -> Self {
        Self {
            function,
            next: function.layout.first_inst(block),
            first_new,
        }
    }

    fn take(&mut self, opcode: Opcode, outputs: &[Type]) -> Result<Inst, JitError> {
        let inst = self.next.ok_or_else(invalid)?;
        self.next = self.function.layout.next_inst(inst);
        require(
            inst.as_u32() as usize >= self.first_new
                && self.function.dfg.insts[inst].opcode() == opcode
                && self.function.dfg.user_stack_map_entries(inst).is_none(),
        )?;
        let results = self.function.dfg.inst_results(inst);
        require(results.len() == outputs.len())?;
        for (index, (&value, &ty)) in results.iter().zip(outputs).enumerate() {
            require(
                self.function.dfg.value_is_real(value)
                    && self.function.dfg.value_def(value) == ValueDef::Result(inst, index)
                    && self.function.dfg.value_type(value) == ty,
            )?;
        }
        Ok(inst)
    }

    fn finish(self) -> Result<(), JitError> {
        require(self.next.is_none())
    }

    fn constant(&mut self, ty: Type, value: i64) -> Result<Value, JitError> {
        let inst = self.take(Opcode::Iconst, &[ty])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::UnaryImm { imm, .. } if i64::from(imm) == value),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn binary(
        &mut self,
        opcode: Opcode,
        ty: Type,
        left: Value,
        right: Value,
    ) -> Result<Value, JitError> {
        let inst = self.take(opcode, &[ty])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::Binary { args, .. } if args == [left, right]),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn compare(&mut self, condition: IntCC, left: Value, right: Value) -> Result<Value, JitError> {
        let inst = self.take(Opcode::Icmp, &[types::I8])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::IntCompare { cond, args, .. } if cond == condition && args == [left, right]),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn compare_imm(
        &mut self,
        condition: IntCC,
        left: Value,
        right: i64,
    ) -> Result<Value, JitError> {
        let constant = self.constant(self.function.dfg.value_type(left), right)?;
        self.compare(condition, left, constant)
    }

    fn load(&mut self, pointer: Value, offset: i32) -> Result<Value, JitError> {
        let inst = self.take(Opcode::Load, &[types::I64])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::Load { arg, offset: actual, flags, .. }
            if arg == pointer && i32::from(actual) == offset && self.function.dfg.mem_flags[flags] == MemFlagsData::new()),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn store(&mut self, pointer: Value, offset: i32, value: Value) -> Result<(), JitError> {
        let inst = self.take(Opcode::Store, &[])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::Store { args, offset: actual, flags, .. }
            if args == [value, pointer] && i32::from(actual) == offset && self.function.dfg.mem_flags[flags] == MemFlagsData::new()),
        )
    }

    fn jump(&mut self, target: Block, args: &[Value]) -> Result<(), JitError> {
        let inst = self.take(Opcode::Jump, &[])?;
        let InstructionData::Jump { destination, .. } = self.function.dfg.insts[inst] else {
            return Err(invalid());
        };
        call(self.function, destination, target, args)
    }

    fn branch(
        &mut self,
        condition: Value,
        targets: [(Block, &[Value]); 2],
    ) -> Result<(), JitError> {
        let inst = self.take(Opcode::Brif, &[])?;
        let InstructionData::Brif { arg, blocks, .. } = self.function.dfg.insts[inst] else {
            return Err(invalid());
        };
        require(arg == condition)?;
        for (actual, (block, args)) in blocks.into_iter().zip(targets) {
            call(self.function, actual, block, args)?;
        }
        Ok(())
    }
}

fn masks(source: &Snapshot, region: &Region) -> Result<(u16, u16), JitError> {
    source.verify()?;
    let p = region.plan;
    require(
        source.registers <= MAX_REGISTERS
            && p.start < p.end
            && p.end < source.operations.len().saturating_sub(1)
            && p.end - p.start < MAX_OPERATIONS,
    )?;
    let Operation::NumericForLoop { base, jump } = source.operations[p.end] else {
        return Err(invalid());
    };
    require(
        usize::from(base.0) == p.base
            && (p.end + 1).checked_add_signed(isize::from(jump)) == Some(p.start),
    )?;
    let mut used = (1 << base.0) | (1 << (base.0 + 1)) | (1 << (base.0 + 2)) | (1 << (base.0 + 3));
    let mut written = (1 << base.0) | (1 << (base.0 + 3));
    for op in source.operations[p.start..p.end].iter().copied() {
        let mut input = |operand: RCIndex| -> Result<(), JitError> {
            match operand {
                RCIndex::Register(index) => {
                    used |= 1 << index.0;
                    Ok(())
                }
                RCIndex::Constant(index) => {
                    require(source.constants[usize::from(index.0)].tag == abi::INTEGER)
                }
            }
        };
        let dest = match op {
            Operation::Add { dest, left, right }
            | Operation::Sub { dest, left, right }
            | Operation::Mul { dest, left, right } => {
                input(left)?;
                input(right)?;
                dest.0
            }
            Operation::Move { dest, source } => {
                input(RCIndex::Register(source))?;
                dest.0
            }
            Operation::LoadConstant { dest, constant } => {
                require(source.constants[usize::from(constant.0)].tag == abi::INTEGER)?;
                dest.0
            }
            _ => return Err(invalid()),
        };
        used |= 1 << dest;
        written |= 1 << dest;
    }
    require(used == p.used && written == p.written && used.count_ones() <= 8)?;
    Ok((used, written))
}

fn state(
    function: &Function,
    block: Block,
    prefix: &[Type],
    mask: u16,
) -> Result<[Option<Value>; MAX_REGISTERS], JitError> {
    let params = function.dfg.block_params(block);
    require(params.len() == prefix.len() + mask.count_ones() as usize)?;
    for (index, &value) in params.iter().enumerate() {
        require(
            function.dfg.value_is_real(value)
                && function.dfg.value_def(value) == ValueDef::Param(block, index)
                && function.dfg.value_type(value)
                    == prefix.get(index).copied().unwrap_or(types::I64),
        )?;
    }
    let mut result = [None; MAX_REGISTERS];
    let mut position = prefix.len();
    for (index, value) in result.iter_mut().enumerate() {
        if mask & (1 << index) != 0 {
            *value = Some(params[position]);
            position += 1;
        }
    }
    Ok(result)
}

fn arguments(
    count: Value,
    values: &[Option<Value>; MAX_REGISTERS],
    mask: u16,
) -> Result<([Value; MAX_REGISTERS + 1], usize), JitError> {
    let mut args = [count; MAX_REGISTERS + 1];
    let mut len = 1;
    for (index, value) in values.iter().enumerate() {
        if mask & (1 << index) != 0 {
            args[len] = value.ok_or_else(invalid)?;
            len += 1;
        }
    }
    Ok((args, len))
}

fn operand(
    scan: &mut Scan<'_>,
    source: &Snapshot,
    values: &[Option<Value>; MAX_REGISTERS],
    input: RCIndex,
) -> Result<Value, JitError> {
    match input {
        RCIndex::Register(index) => values[usize::from(index.0)].ok_or_else(invalid),
        RCIndex::Constant(index) => scan.constant(
            types::I64,
            source.constants[usize::from(index.0)].bits as i64,
        ),
    }
}

fn flush(
    scan: &mut Scan<'_>,
    pointer: Value,
    values: &[Option<Value>; MAX_REGISTERS],
    mask: u16,
) -> Result<(), JitError> {
    let tag = scan.constant(types::I64, abi::INTEGER as i64)?;
    for (index, value) in values.iter().enumerate() {
        if mask & (1 << index) != 0 {
            scan.store(pointer, index as i32 * 16, tag)?;
            scan.store(pointer, index as i32 * 16 + 8, value.ok_or_else(invalid)?)?;
        }
    }
    Ok(())
}

pub(super) fn check(
    before: &Function,
    after: &Function,
    source: &Snapshot,
    headers: &[Block],
    exhausted: Block,
    r: &Region,
    probe: bool,
) -> Result<(), JitError> {
    cranelift_codegen::verify_function(
        after,
        &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
    )
    .map_err(|_| invalid())?;
    let (used, written) = masks(source, r)?;
    require(headers.len() == source.operations.len())?;
    let length = r.plan.end - r.plan.start + 1;
    let new_blocks = [r.generic, r.guards, r.dispatch, r.done, r.budget_exit]
        .into_iter()
        .chain((0..length).flat_map(|i| [r.fast[i], r.bodies[i]]));
    require(after.dfg.num_blocks() == before.dfg.num_blocks() + 5 + length * 2)?;
    for (index, block) in new_blocks.clone().enumerate() {
        require(block.as_u32() as usize == before.dfg.num_blocks() + index)?;
    }
    require(
        after
            .layout
            .blocks()
            .skip(before.layout.blocks().count())
            .eq(new_blocks),
    )?;
    preserved(before, after, r.generic)?;
    for block in [r.generic, r.guards, r.dispatch]
        .into_iter()
        .chain(r.bodies[..length].iter().copied())
    {
        require(after.dfg.block_params(block).is_empty())?;
    }
    let entry = before.layout.entry_block().ok_or_else(invalid)?;
    let p: [Value; 5] = before
        .dfg
        .block_params(entry)
        .try_into()
        .map_err(|_| invalid())?;
    let first_new = before.dfg.num_insts();
    let scan = |block| Scan::new(after, block, first_new);
    let mut s = scan(entry);
    let lower = s.compare_imm(IntCC::UnsignedGreaterThanOrEqual, p[1], r.plan.start as i64)?;
    let upper = s.compare_imm(IntCC::UnsignedLessThanOrEqual, p[1], r.plan.end as i64)?;
    let within = s.binary(Opcode::Band, types::I8, lower, upper)?;
    let fuel = s.compare_imm(IntCC::NotEqual, p[2], 0)?;
    let ready = s.binary(Opcode::Band, types::I8, within, fuel)?;
    s.branch(ready, [(r.guards, &[]), (r.generic, &[])])?;
    s.finish()?;
    let mut s = scan(r.guards);
    let mut valid = s.constant(types::I8, 1)?;
    for index in 0..MAX_REGISTERS {
        if used & (1 << index) != 0 {
            let tag = s.load(p[0], index as i32 * 16)?;
            let integer = s.compare_imm(IntCC::Equal, tag, abi::INTEGER as i64)?;
            valid = s.binary(Opcode::Band, types::I8, valid, integer)?;
        }
    }
    s.branch(valid, [(r.dispatch, &[]), (r.generic, &[])])?;
    s.finish()?;
    let mut s = scan(r.dispatch);
    if probe {
        let dest = s.load(p[4], 0)?;
        let one = s.constant(types::I64, 1)?;
        s.store(dest, 0, one)?;
    }
    let mut initial = [None; MAX_REGISTERS];
    for (index, value) in initial.iter_mut().enumerate() {
        if used & (1 << index) != 0 {
            *value = Some(s.load(p[0], index as i32 * 16 + 8)?);
        }
    }
    let zero = s.constant(types::I32, 0)?;
    let negative_start = s.constant(types::I64, -(r.plan.start as i64))?;
    let offset = s.binary(Opcode::Iadd, types::I64, p[1], negative_start)?;
    let reduce = s.take(Opcode::Ireduce, &[types::I32])?;
    require(
        matches!(after.dfg.insts[reduce], InstructionData::Unary { arg, .. } if arg == offset),
    )?;
    let index = after.dfg.first_result(reduce);
    let branch = s.take(Opcode::BrTable, &[])?;
    let InstructionData::BranchTable { arg, table, .. } = after.dfg.insts[branch] else {
        return Err(invalid());
    };
    require(arg == index && table.as_u32() as usize == before.dfg.jump_tables.len())?;
    let table = &after.dfg.jump_tables[table];
    require(table.as_slice().len() == length)?;
    call(after, table.default_block(), r.generic, &[])?;
    let (args, len) = arguments(zero, &initial, used)?;
    for (index, &target) in table.as_slice().iter().enumerate() {
        call(after, target, r.fast[index], &args[..len])?;
    }
    s.finish()?;
    for index in 0..length {
        let pc = r.plan.start + index;
        let mut values = state(after, r.fast[index], &[types::I32], used)?;
        let count = after.dfg.block_params(r.fast[index])[0];
        let mut s = scan(r.fast[index]);
        let limit = s.compare(IntCC::UnsignedGreaterThanOrEqual, count, p[2])?;
        let source_pc = s.constant(types::I64, pc as i64)?;
        let (args, len) = arguments(count, &values, used)?;
        let mut exit_args = [source_pc; MAX_REGISTERS + 2];
        exit_args[1..len + 1].copy_from_slice(&args[..len]);
        s.branch(
            limit,
            [
                (r.budget_exit, &exit_args[..len + 1]),
                (r.bodies[index], &[]),
            ],
        )?;
        s.finish()?;
        let mut s = scan(r.bodies[index]);
        let one = s.constant(types::I32, 1)?;
        let next_count = s.binary(Opcode::Iadd, types::I32, count, one)?;
        match source.operations[pc] {
            op @ (Operation::Add { dest, left, right }
            | Operation::Sub { dest, left, right }
            | Operation::Mul { dest, left, right }) => {
                let left = operand(&mut s, source, &values, left)?;
                let right = operand(&mut s, source, &values, right)?;
                let opcode = match op {
                    Operation::Add { .. } => Opcode::Iadd,
                    Operation::Sub { .. } => Opcode::Isub,
                    _ => Opcode::Imul,
                };
                values[usize::from(dest.0)] = Some(s.binary(opcode, types::I64, left, right)?);
            }
            Operation::Move { dest, source } => {
                values[usize::from(dest.0)] = values[usize::from(source.0)];
            }
            Operation::LoadConstant { dest, constant } => {
                values[usize::from(dest.0)] = Some(s.constant(
                    types::I64,
                    source.constants[usize::from(constant.0)].bits as i64,
                )?);
            }
            Operation::NumericForLoop { base, jump: _ } => {
                require(pc == r.plan.end)?;
                let base = usize::from(base.0);
                let step = values[base + 2].ok_or_else(invalid)?;
                let add = s.take(Opcode::SaddOverflow, &[types::I64, types::I8])?;
                require(
                    matches!(after.dfg.insts[add], InstructionData::Binary { args, .. } if args == [values[base].ok_or_else(invalid)?, step]),
                )?;
                let next = after.dfg.inst_results(add)[0];
                let overflow = after.dfg.inst_results(add)[1];
                let negative = s.compare_imm(IntCC::SignedLessThan, step, 0)?;
                let ge = s.compare(
                    IntCC::SignedGreaterThanOrEqual,
                    next,
                    values[base + 1].ok_or_else(invalid)?,
                )?;
                let le = s.compare(
                    IntCC::SignedLessThanOrEqual,
                    next,
                    values[base + 1].ok_or_else(invalid)?,
                )?;
                let select = s.take(Opcode::Select, &[types::I8])?;
                require(
                    matches!(after.dfg.insts[select], InstructionData::Ternary { args, .. } if args == [negative, ge, le]),
                )?;
                let one = s.constant(types::I8, 1)?;
                let no_overflow = s.binary(Opcode::Bxor, types::I8, overflow, one)?;
                let taken = s.binary(
                    Opcode::Band,
                    types::I8,
                    after.dfg.first_result(select),
                    no_overflow,
                )?;
                values[base] = Some(next);
                let (done, done_len) = arguments(next_count, &values, used)?;
                values[base + 3] = Some(next);
                let (back, back_len) = arguments(next_count, &values, used)?;
                s.branch(
                    taken,
                    [(r.fast[0], &back[..back_len]), (r.done, &done[..done_len])],
                )?;
                s.finish()?;
                continue;
            }
            _ => return Err(invalid()),
        }
        let (args, len) = arguments(next_count, &values, used)?;
        s.jump(r.fast[index + 1], &args[..len])?;
        s.finish()?;
    }
    let values = state(after, r.done, &[types::I32], used)?;
    let mut s = scan(r.done);
    flush(&mut s, p[0], &values, written)?;
    s.jump(
        headers[r.plan.end + 1],
        &[after.dfg.block_params(r.done)[0]],
    )?;
    s.finish()?;
    let values = state(after, r.budget_exit, &[types::I64, types::I32], used)?;
    let mut s = scan(r.budget_exit);
    flush(&mut s, p[0], &values, written)?;
    s.jump(exhausted, &after.dfg.block_params(r.budget_exit)[..2])?;
    s.finish()?;
    let added_insts = after
        .layout
        .blocks()
        .map(|b| after.layout.block_insts(b).count())
        .sum::<usize>()
        - before
            .layout
            .blocks()
            .map(|b| before.layout.block_insts(b).count())
            .sum::<usize>();
    require(after.dfg.num_insts() == before.dfg.num_insts() + added_insts)?;
    let added_values: usize = after
        .layout
        .blocks()
        .flat_map(|b| after.layout.block_insts(b))
        .filter(|i| i.as_u32() as usize >= first_new)
        .map(|i| after.dfg.inst_results(i).len())
        .sum::<usize>()
        + after
            .dfg
            .blocks
            .iter()
            .skip(before.dfg.num_blocks())
            .map(|b| after.dfg.block_params(b).len())
            .sum::<usize>();
    require(after.dfg.num_values() == before.dfg.num_values() + added_values)
}
