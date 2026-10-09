use std::mem::offset_of;

use cranelift_codegen::{
    ir::{
        condcodes::IntCC, types, AbiParam, Block, BlockArg, BlockCall, Function, Inst,
        InstructionData, MemFlagsData, Opcode, Type, Value as IrValue, ValueDef,
    },
    isa::TargetIsa,
};

use super::{native::View, plan::Plan, *};
use crate::{
    jit::{ir::Snapshot, JitError},
    opcode::{Operation, RCIndex},
};

fn invalid() -> JitError {
    JitError::Compilation("array loop translation".into())
}

fn require(valid: bool) -> Result<(), JitError> {
    if valid {
        Ok(())
    } else {
        Err(invalid())
    }
}

struct Scan<'a> {
    function: &'a Function,
    block: Block,
    next: Option<Inst>,
}

impl<'a> Scan<'a> {
    fn new(function: &'a Function, block: Block) -> Self {
        Self {
            function,
            block,
            next: function.layout.first_inst(block),
        }
    }

    fn take(&mut self, opcode: Opcode, outputs: &[Type]) -> Result<Inst, JitError> {
        let inst = self.next.ok_or_else(invalid)?;
        require(
            self.function.dfg.insts[inst].opcode() == opcode
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
        self.next = self.function.layout.next_inst(inst);
        Ok(inst)
    }

    fn next_block(&self) -> Result<Block, JitError> {
        self.function
            .layout
            .next_block(self.block)
            .ok_or_else(invalid)
    }

    fn advance(&mut self, block: Block, types: &[Type]) -> Result<(), JitError> {
        require(self.next.is_none() && self.next_block()? == block)?;
        params(self.function, block, types)?;
        self.block = block;
        self.next = self.function.layout.first_inst(block);
        Ok(())
    }

    fn constant(&mut self, ty: Type, bits: i64) -> Result<IrValue, JitError> {
        let inst = self.take(Opcode::Iconst, &[ty])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::UnaryImm { imm, .. } if imm.bits() == bits),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn binary(
        &mut self,
        opcode: Opcode,
        ty: Type,
        a: IrValue,
        b: IrValue,
    ) -> Result<IrValue, JitError> {
        let inst = self.take(opcode, &[ty])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::Binary { args, .. } if args == [a, b]),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn immediate(
        &mut self,
        opcode: Opcode,
        value: IrValue,
        bits: i64,
    ) -> Result<IrValue, JitError> {
        let ty = self.function.dfg.value_type(value);
        let constant = self.constant(ty, bits)?;
        self.binary(opcode, ty, value, constant)
    }

    fn compare(&mut self, cond: IntCC, a: IrValue, b: IrValue) -> Result<IrValue, JitError> {
        let inst = self.take(Opcode::Icmp, &[types::I8])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::IntCompare { cond: actual, args, .. } if actual == cond && args == [a,b]),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn compare_imm(&mut self, cond: IntCC, value: IrValue, bits: i64) -> Result<IrValue, JitError> {
        let constant = self.constant(self.function.dfg.value_type(value), bits)?;
        self.compare(cond, value, constant)
    }

    fn load(&mut self, ty: Type, pointer: IrValue, offset: i32) -> Result<IrValue, JitError> {
        let inst = self.take(Opcode::Load, &[ty])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::Load { arg, offset: actual, flags, .. } if arg == pointer && i32::from(actual) == offset && self.function.dfg.mem_flags[flags] == MemFlagsData::new()),
        )?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn store(&mut self, pointer: IrValue, offset: i32, value: IrValue) -> Result<(), JitError> {
        let inst = self.take(Opcode::Store, &[])?;
        require(
            matches!(self.function.dfg.insts[inst], InstructionData::Store { args, offset: actual, flags, .. } if args == [value, pointer] && i32::from(actual) == offset && self.function.dfg.mem_flags[flags] == MemFlagsData::new()),
        )
    }

    fn unary(&mut self, opcode: Opcode, ty: Type, value: IrValue) -> Result<IrValue, JitError> {
        let inst = self.take(opcode, &[ty])?;
        require(match self.function.dfg.insts[inst] {
            InstructionData::Unary { arg, .. } => opcode != Opcode::Bitcast && arg == value,
            InstructionData::LoadNoOffset { arg, flags, .. } => {
                opcode == Opcode::Bitcast
                    && arg == value
                    && self.function.dfg.mem_flags[flags] == MemFlagsData::new()
            }
            _ => false,
        })?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn select(
        &mut self,
        condition: IrValue,
        yes: IrValue,
        no: IrValue,
    ) -> Result<IrValue, JitError> {
        let inst = self.take(Opcode::Select, &[self.function.dfg.value_type(yes)])?;
        require(self.function.dfg.inst_args(inst) == [condition, yes, no])?;
        Ok(self.function.dfg.first_result(inst))
    }

    fn edges(&mut self, condition: IrValue) -> Result<[BlockCall; 2], JitError> {
        let inst = self.take(Opcode::Brif, &[])?;
        let InstructionData::Brif { arg, blocks, .. } = self.function.dfg.insts[inst] else {
            return Err(invalid());
        };
        require(arg == condition && self.next.is_none())?;
        Ok(blocks)
    }

    fn edge(&self, edge: BlockCall, block: Block, values: &[IrValue]) -> Result<(), JitError> {
        require(
            edge.block(&self.function.dfg.value_lists) == block
                && edge
                    .args(&self.function.dfg.value_lists)
                    .eq(values.iter().copied().map(BlockArg::Value)),
        )
    }

    fn jump(&mut self, block: Block, args: &[IrValue]) -> Result<(), JitError> {
        let inst = self.take(Opcode::Jump, &[])?;
        let InstructionData::Jump { destination, .. } = self.function.dfg.insts[inst] else {
            return Err(invalid());
        };
        require(self.next.is_none())?;
        self.edge(destination, block, args)
    }

    fn guard(&mut self, condition: IrValue, decline: Block) -> Result<(), JitError> {
        let edges = self.edges(condition)?;
        let next = self.next_block()?;
        self.edge(edges[0], next, &[])?;
        self.edge(edges[1], decline, &[])?;
        self.advance(next, &[])
    }

    fn guarded_imm(
        &mut self,
        cond: IntCC,
        value: IrValue,
        bits: i64,
        decline: Block,
    ) -> Result<(), JitError> {
        let predicate = self.compare_imm(cond, value, bits)?;
        self.guard(predicate, decline)
    }

    fn register(&mut self, slots: IrValue, register: u8) -> Result<(IrValue, IrValue), JitError> {
        Ok((
            self.load(types::I64, slots, i32::from(register) * 16)?,
            self.load(types::I64, slots, i32::from(register) * 16 + 8)?,
        ))
    }

    fn literal(&mut self, slot: Slot) -> Result<(IrValue, IrValue), JitError> {
        Ok((
            self.constant(types::I64, slot.tag as i64)?,
            self.constant(types::I64, slot.bits as i64)?,
        ))
    }

    fn operand(
        &mut self,
        source: &Snapshot,
        slots: IrValue,
        operand: RCIndex,
    ) -> Result<(IrValue, IrValue), JitError> {
        match operand {
            RCIndex::Register(index) => self.register(slots, index.0),
            RCIndex::Constant(index) => self.literal(source.constants[usize::from(index.0)]),
        }
    }

    fn write_register(
        &mut self,
        slots: IrValue,
        register: u8,
        tag: IrValue,
        bits: IrValue,
    ) -> Result<(), JitError> {
        self.store(slots, i32::from(register) * 16, tag)?;
        self.store(slots, i32::from(register) * 16 + 8, bits)
    }
}

fn params(function: &Function, block: Block, expected: &[Type]) -> Result<(), JitError> {
    let actual = function.dfg.block_params(block);
    require(actual.len() == expected.len())?;
    for (index, (&value, &ty)) in actual.iter().zip(expected).enumerate() {
        require(
            function.dfg.value_is_real(value)
                && function.dfg.value_def(value) == ValueDef::Param(block, index)
                && function.dfg.value_type(value) == ty,
        )?;
    }
    Ok(())
}

fn binding(source: &Snapshot, plan: Plan) -> Result<(), JitError> {
    source.verify()?;
    require(
        plan.start < plan.end
            && plan.end < source.operations.len()
            && plan.end - plan.start < super::plan::MAX_OPERATIONS,
    )?;
    let Operation::NumericForLoop { base, jump } = source.operations[plan.end] else {
        return Err(invalid());
    };
    require(
        base.0 == plan.base
            && (plan.end + 1).checked_add_signed(isize::from(jump)) == Some(plan.start),
    )?;
    require(!(plan.base..=plan.base + 3).contains(&plan.table))?;
    let (mut found, mut writing) = (false, false);
    let valid_constant = |slot: Slot| match slot.tag {
        0 => slot.bits == 0,
        1 => slot.bits <= 1,
        2 | 3 => true,
        _ => false,
    };
    let valid_operand = |operand| match operand {
        RCIndex::Register(_) => true,
        RCIndex::Constant(index) => valid_constant(source.constants[usize::from(index.0)]),
    };
    for operation in source.operations[plan.start..plan.end].iter().copied() {
        if let Operation::GetTable { table, key, .. } | Operation::SetTable { table, key, .. } =
            operation
        {
            require(
                table.0 == plan.table
                    && matches!(key, RCIndex::Register(index) if index.0 == plan.base + 3),
            )?;
            found = true;
        }
        let dest = match operation {
            Operation::GetTable { dest, .. } | Operation::Move { dest, .. } => Some(dest.0),
            Operation::SetTable { value, .. } => {
                require(valid_operand(value))?;
                writing = true;
                None
            }
            Operation::LoadConstant { dest, constant } => {
                require(valid_constant(source.constants[usize::from(constant.0)]))?;
                Some(dest.0)
            }
            Operation::Add { dest, left, right }
            | Operation::Sub { dest, left, right }
            | Operation::Mul { dest, left, right } => {
                require(valid_operand(left) && valid_operand(right))?;
                Some(dest.0)
            }
            _ => return Err(invalid()),
        };
        if let Some(dest) = dest {
            require(dest != plan.table && !(plan.base..=plan.base + 3).contains(&dest))?;
        }
    }
    require(found && plan.access == if writing { Access::Write } else { Access::Read })
}

fn scalar_guard(
    s: &mut Scan<'_>,
    tag: IrValue,
    bits: IrValue,
    decline: Block,
) -> Result<(), JitError> {
    s.guarded_imm(IntCC::UnsignedLessThanOrEqual, tag, 3, decline)?;
    let numeric = s.compare_imm(IntCC::UnsignedGreaterThanOrEqual, tag, 2)?;
    let nil = s.compare_imm(IntCC::Equal, tag, 0)?;
    let empty = s.compare_imm(IntCC::Equal, bits, 0)?;
    let nil = s.binary(Opcode::Band, types::I8, nil, empty)?;
    let boolean = s.compare_imm(IntCC::Equal, tag, 1)?;
    let boolean_bits = s.compare_imm(IntCC::UnsignedLessThanOrEqual, bits, 1)?;
    let boolean = s.binary(Opcode::Band, types::I8, boolean, boolean_bits)?;
    let valid = s.binary(Opcode::Bor, types::I8, nil, boolean)?;
    let valid = s.binary(Opcode::Bor, types::I8, valid, numeric)?;
    s.guard(valid, decline)
}

fn table_access(
    s: &mut Scan<'_>,
    view: IrValue,
    key: IrValue,
    read: Option<IrValue>,
    write: Option<(IrValue, IrValue)>,
    decline: Block,
) -> Result<(), JitError> {
    s.guarded_imm(IntCC::NotEqual, view, 0, decline)?;
    if let Some(dest) = read {
        s.guarded_imm(IntCC::NotEqual, dest, 0, decline)?;
    }
    let version = s.load(types::I64, view, offset_of!(View, version) as i32)?;
    s.guarded_imm(
        IntCC::Equal,
        version,
        super::native::VERSION as i64,
        decline,
    )?;
    let length = s.load(types::I64, view, offset_of!(View, length) as i32)?;
    s.guarded_imm(IntCC::UnsignedLessThanOrEqual, length, 64, decline)?;
    s.guarded_imm(IntCC::NotEqual, length, 0, decline)?;
    let first = s.load(types::I64, view, offset_of!(View, first) as i32)?;
    s.guarded_imm(IntCC::SignedGreaterThan, first, 0, decline)?;
    let valid = s.compare(IntCC::SignedGreaterThanOrEqual, key, first)?;
    s.guard(valid, decline)?;
    let index = s.binary(Opcode::Isub, types::I64, key, first)?;
    let valid = s.compare(IntCC::UnsignedLessThan, index, length)?;
    s.guard(valid, decline)?;
    let writable = s.load(types::I64, view, offset_of!(View, writable) as i32)?;
    s.guarded_imm(
        if write.is_some() {
            IntCC::Equal
        } else {
            IntCC::UnsignedLessThanOrEqual
        },
        writable,
        1,
        decline,
    )?;
    let cells = s.load(types::I64, view, offset_of!(View, slots) as i32)?;
    let dirty = s.load(types::I64, view, offset_of!(View, dirty) as i32)?;
    let counts = s.load(types::I64, view, offset_of!(View, counts) as i32)?;
    for pointer in [cells, dirty, counts] {
        s.guarded_imm(IntCC::NotEqual, pointer, 0, decline)?;
    }
    let offset = s.immediate(Opcode::Ishl, index, 4)?;
    let cell = s.binary(Opcode::Iadd, types::I64, cells, offset)?;
    let (tag, bits, dest) = if let Some((tag, bits)) = write {
        (tag, bits, cell)
    } else {
        (
            s.load(types::I64, cell, 0)?,
            s.load(types::I64, cell, 8)?,
            read.ok_or_else(invalid)?,
        )
    };
    scalar_guard(s, tag, bits, decline)?;
    s.store(dest, 0, tag)?;
    s.store(dest, 8, bits)?;
    if write.is_some() {
        let previous = s.load(types::I64, dirty, 0)?;
        let one = s.constant(types::I64, 1)?;
        let bit = s.binary(Opcode::Ishl, types::I64, one, index)?;
        let updated = s.binary(Opcode::Bor, types::I64, previous, bit)?;
        s.store(dirty, 0, updated)?;
    }
    let offset = if write.is_some() {
        offset_of!(Counts, writes)
    } else {
        offset_of!(Counts, reads)
    } as i32;
    let previous = s.load(types::I32, counts, offset)?;
    let saturated = s.compare_imm(IntCC::Equal, previous, i64::from(u32::MAX))?;
    let incremented = s.immediate(Opcode::Iadd, previous, 1)?;
    let next = s.select(saturated, previous, incremented)?;
    s.store(counts, offset, next)
}

fn numeric(s: &mut Scan<'_>, tag: IrValue, decline: Block) -> Result<(), JitError> {
    let integer = s.compare_imm(IntCC::Equal, tag, 2)?;
    let number = s.compare_imm(IntCC::Equal, tag, 3)?;
    let valid = s.binary(Opcode::Bor, types::I8, integer, number)?;
    s.guard(valid, decline)
}

fn float(s: &mut Scan<'_>, tag: IrValue, bits: IrValue) -> Result<IrValue, JitError> {
    let integer = s.compare_imm(IntCC::Equal, tag, 2)?;
    let converted = s.unary(Opcode::FcvtFromSint, types::F64, bits)?;
    let original = s.unary(Opcode::Bitcast, types::F64, bits)?;
    s.select(integer, converted, original)
}

fn arithmetic(
    s: &mut Scan<'_>,
    source: &Snapshot,
    slots: IrValue,
    operation: Operation,
    decline: Block,
) -> Result<(), JitError> {
    let (dest, left, right, int_op, float_op) = match operation {
        Operation::Add { dest, left, right } => (dest, left, right, Opcode::Iadd, Opcode::Fadd),
        Operation::Sub { dest, left, right } => (dest, left, right, Opcode::Isub, Opcode::Fsub),
        Operation::Mul { dest, left, right } => (dest, left, right, Opcode::Imul, Opcode::Fmul),
        _ => return Err(invalid()),
    };
    let (lt, lb) = s.operand(source, slots, left)?;
    let (rt, rb) = s.operand(source, slots, right)?;
    numeric(s, lt, decline)?;
    numeric(s, rt, decline)?;
    let li = s.compare_imm(IntCC::Equal, lt, 2)?;
    let ri = s.compare_imm(IntCC::Equal, rt, 2)?;
    let both = s.binary(Opcode::Band, types::I8, li, ri)?;
    let edges = s.edges(both)?;
    let integer = edges[0].block(&s.function.dfg.value_lists);
    let floating = edges[1].block(&s.function.dfg.value_lists);
    s.edge(edges[0], integer, &[])?;
    s.edge(edges[1], floating, &[])?;
    s.advance(integer, &[])?;
    let bits = s.binary(int_op, types::I64, lb, rb)?;
    let tag = s.constant(types::I64, 2)?;
    let jump = s.next.ok_or_else(invalid)?;
    let InstructionData::Jump { destination, .. } = s.function.dfg.insts[jump] else {
        return Err(invalid());
    };
    let join = destination.block(&s.function.dfg.value_lists);
    s.jump(join, &[tag, bits])?;
    s.advance(floating, &[])?;
    let left = float(s, lt, lb)?;
    let right = float(s, rt, rb)?;
    let value = s.binary(float_op, types::F64, left, right)?;
    let bits = s.unary(Opcode::Bitcast, types::I64, value)?;
    let tag = s.constant(types::I64, 3)?;
    s.jump(join, &[tag, bits])?;
    s.advance(join, &[types::I64, types::I64])?;
    let values = s.function.dfg.block_params(join);
    s.write_register(slots, dest.0, values[0], values[1])
}

fn operation(
    s: &mut Scan<'_>,
    source: &Snapshot,
    slots: IrValue,
    view: IrValue,
    op: Operation,
    decline: Block,
) -> Result<(), JitError> {
    match op {
        Operation::GetTable { dest, key, .. } => {
            let (tag, key) = s.operand(source, slots, key)?;
            s.guarded_imm(IntCC::Equal, tag, 2, decline)?;
            let dest = s.immediate(Opcode::Iadd, slots, i64::from(dest.0) * 16)?;
            table_access(s, view, key, Some(dest), None, decline)
        }
        Operation::SetTable { key, value, .. } => {
            let (tag, key) = s.operand(source, slots, key)?;
            s.guarded_imm(IntCC::Equal, tag, 2, decline)?;
            let value = s.operand(source, slots, value)?;
            table_access(s, view, key, None, Some(value), decline)
        }
        Operation::Move { dest, source } => {
            let (tag, bits) = s.register(slots, source.0)?;
            scalar_guard(s, tag, bits, decline)?;
            s.write_register(slots, dest.0, tag, bits)
        }
        Operation::LoadConstant { dest, constant } => {
            let (tag, bits) = s.literal(source.constants[usize::from(constant.0)])?;
            s.write_register(slots, dest.0, tag, bits)
        }
        Operation::Add { .. } | Operation::Sub { .. } | Operation::Mul { .. } => {
            arithmetic(s, source, slots, op, decline)
        }
        _ => Err(invalid()),
    }
}

fn for_loop(
    s: &mut Scan<'_>,
    slots: IrValue,
    plan: Plan,
    decline: Block,
    header: Block,
    count: IrValue,
) -> Result<(), JitError> {
    let (it, index) = s.register(slots, plan.base)?;
    let (lt, limit) = s.register(slots, plan.base + 1)?;
    let (st, step) = s.register(slots, plan.base + 2)?;
    for tag in [it, lt, st] {
        s.guarded_imm(IntCC::Equal, tag, 2, decline)?;
    }
    let inst = s.take(Opcode::SaddOverflow, &[types::I64, types::I8])?;
    require(s.function.dfg.inst_args(inst) == [index, step])?;
    let results = s.function.dfg.inst_results(inst);
    let (next, overflow) = (results[0], results[1]);
    let negative = s.compare_imm(IntCC::SignedLessThan, step, 0)?;
    let ge = s.compare(IntCC::SignedGreaterThanOrEqual, next, limit)?;
    let le = s.compare(IntCC::SignedLessThanOrEqual, next, limit)?;
    let in_range = s.select(negative, ge, le)?;
    let no_overflow = s.immediate(Opcode::Bxor, overflow, 1)?;
    let continuing = s.binary(Opcode::Band, types::I8, in_range, no_overflow)?;
    s.write_register(slots, plan.base, it, next)?;
    let edges = s.edges(continuing)?;
    let update = edges[0].block(&s.function.dfg.value_lists);
    let done = edges[1].block(&s.function.dfg.value_lists);
    s.edge(edges[0], update, &[])?;
    s.edge(edges[1], done, &[])?;
    s.advance(update, &[])?;
    s.write_register(slots, plan.base + 3, it, next)?;
    s.jump(header, &[count])?;
    s.advance(done, &[])
}

pub(in crate::jit) fn verify(
    source: &Snapshot,
    plan: Plan,
    function: &Function,
    isa: &dyn TargetIsa,
) -> Result<(), JitError> {
    binding(source, plan)?;
    let count = plan.end - plan.start + 1;
    let blocks = function.layout.blocks().count();
    let instructions = function
        .layout
        .blocks()
        .map(|block| function.layout.block_insts(block).count())
        .sum::<usize>();
    require(
        blocks <= 64 + 32 * count
            && instructions <= 256 + 256 * count
            && function.dfg.blocks.len() == blocks
            && function.dfg.num_insts() == instructions,
    )?;
    cranelift_codegen::verify_function(function, isa).map_err(|_| invalid())?;
    require(
        function.signature.call_conv == isa.default_call_conv()
            && function.signature.params.iter().copied().eq([
                types::I64,
                types::I64,
                types::I32,
                types::I64,
                types::I64,
            ]
            .map(AbiParam::new))
            && function.signature.returns.is_empty()
            && function.sized_stack_slots.is_empty()
            && function.dynamic_stack_slots.is_empty()
            && function.global_values.is_empty()
            && function.stack_limit.is_none()
            && function.dfg.ext_funcs.is_empty()
            && function.dfg.signatures.is_empty()
            && function.dfg.exception_tables.is_empty()
            && function.dfg.jump_tables.is_empty()
            && function.dfg.dynamic_types.is_empty(),
    )?;
    let entry = function.layout.entry_block().ok_or_else(invalid)?;
    params(
        function,
        entry,
        &[types::I64, types::I64, types::I32, types::I64, types::I64],
    )?;
    let [slots, pc, budget, output, view]: [_; 5] = function
        .dfg
        .block_params(entry)
        .try_into()
        .map_err(|_| invalid())?;
    let mut s = Scan::new(function, entry);
    let zero = s.constant(types::I32, 0)?;
    let cap = s.constant(types::I32, 64)?;
    let budget = s.binary(Opcode::Umin, types::I32, budget, cap)?;
    let interpreter = s.constant(types::I32, 0)?;
    let guard = s.constant(types::I32, 1)?;
    let exhausted = s.constant(types::I32, 2)?;
    let mut headers = [entry; super::plan::MAX_OPERATIONS];
    for (index, header) in headers[..count].iter_mut().enumerate() {
        let matched = s.compare_imm(IntCC::Equal, pc, (plan.start + index) as i64)?;
        let edges = s.edges(matched)?;
        *header = edges[0].block(&function.dfg.value_lists);
        params(function, *header, &[types::I32])?;
        s.edge(edges[0], *header, &[zero])?;
        let next = s.next_block()?;
        s.edge(edges[1], next, &[])?;
        s.advance(next, &[])?;
    }
    let inst = s.next.ok_or_else(invalid)?;
    let InstructionData::Jump { destination, .. } = function.dfg.insts[inst] else {
        return Err(invalid());
    };
    let exit = destination.block(&function.dfg.value_lists);
    s.jump(exit, &[pc, zero, interpreter])?;
    for (index, &header) in headers[..count].iter().enumerate() {
        s.advance(header, &[types::I32])?;
        let completed = function.dfg.block_params(header)[0];
        let current = s.constant(types::I64, (plan.start + index) as i64)?;
        let within = s.compare(IntCC::UnsignedLessThan, completed, budget)?;
        let edges = s.edges(within)?;
        let body = edges[0].block(&function.dfg.value_lists);
        s.edge(edges[0], body, &[])?;
        s.edge(edges[1], exit, &[current, completed, exhausted])?;
        let decline = s.next_block()?;
        s.advance(decline, &[])?;
        s.jump(exit, &[current, completed, guard])?;
        s.advance(body, &[])?;
        let next_count = s.immediate(Opcode::Iadd, completed, 1)?;
        if index + 1 == count {
            for_loop(&mut s, slots, plan, decline, headers[0], next_count)?;
            let next_pc = s.constant(types::I64, (plan.end + 1) as i64)?;
            s.jump(exit, &[next_pc, next_count, interpreter])?;
        } else {
            operation(
                &mut s,
                source,
                slots,
                view,
                source.operations[plan.start + index],
                decline,
            )?;
            s.jump(headers[index + 1], &[next_count])?;
        }
    }
    s.advance(exit, &[types::I64, types::I32, types::I32])?;
    let args = function.dfg.block_params(exit);
    for (offset, value) in [(0, args[0]), (8, args[1]), (12, args[2])] {
        s.store(output, offset, value)?;
    }
    let returns = s.take(Opcode::Return, &[])?;
    require(
        function.dfg.inst_args(returns).is_empty()
            && s.next.is_none()
            && function.layout.next_block(exit).is_none(),
    )
}

#[cfg(test)]
mod tests;
