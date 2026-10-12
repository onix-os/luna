use super::*;
use crate::jit::abi::strided::frame::Header;

#[derive(Clone, Copy)]
struct Read {
    inst: Inst,
    index: usize,
    pointer: bool,
    result: IrValue,
}

pub(in crate::jit::backend) fn expansion(
    source: &Snapshot,
    limits: crate::jit::work::Limits,
) -> Result<crate::jit::work::Expansion, JitError> {
    let mut expansion = super::expansion(source, limits)?;
    for operation in &source.operations {
        if let Operation::LoadNil { count, .. } = operation {
            expansion.instructions = expansion
                .instructions
                .checked_add(17 * usize::from(*count))
                .ok_or(JitError::ResourceLimit("strided expansion overflow"))?;
        }
    }
    if expansion.instructions > limits.instructions {
        return Err(JitError::ResourceLimit("IR instructions"));
    }
    Ok(expansion)
}

fn emit(f: &mut Function, slots: IrValue, read: Read) {
    let mut c = FuncCursor::new(f);
    c.goto_inst(read.inst);
    let byte = c.ins().load(
        types::I8,
        MemFlagsData::new(),
        slots,
        (std::mem::offset_of!(Header, tags) + read.index) as i32,
    );
    if !read.pointer {
        c.func.replace(read.inst).uextend(types::I64, byte);
        return;
    }
    let tag = c.ins().uextend(types::I64, byte);
    let width = c.ins().iconst(types::I64, 8);
    let scaled = c.ins().imul(tag, width);
    let table = c.ins().iadd(slots, scaled);
    let offset = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        table,
        std::mem::offset_of!(Header, offsets) as i32,
    );
    let base = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        std::mem::offset_of!(Header, base) as i32,
    );
    let displacement = c.ins().iconst(
        types::I64,
        (read.index * std::mem::size_of::<crate::Value<'_>>()) as i64,
    );
    let cell = c.ins().iadd(base, displacement);
    let address = c.ins().iadd(cell, offset);
    let one = c.ins().iconst(types::I64, abi::BOOLEAN as i64);
    let three = c.ins().iconst(types::I64, abi::NUMBER as i64);
    let nonzero = c.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, tag, one);
    let nonref = c.ins().icmp(IntCC::UnsignedLessThanOrEqual, tag, three);
    let scalar = c.ins().band(nonzero, nonref);
    let null = c.ins().iconst(types::I64, 0);
    c.func.replace(read.inst).select(scalar, address, null);
}

fn verify(f: &Function, slots: IrValue, read: Read) -> Result<(), JitError> {
    use reads::{compare, constant, load, value};
    let mut previous = Some(read.inst);
    let mut backwards = std::iter::from_fn(|| {
        let inst = previous?;
        previous = f.layout.prev_inst(inst);
        Some(inst)
    });
    if !read.pointer {
        let byte = backwards.nth(1).ok_or_else(invalid)?;
        let byte = load(
            f,
            byte,
            slots,
            (std::mem::offset_of!(Header, tags) + read.index) as i32,
            types::I8,
        )?;
        if value(f, read.inst, Opcode::Uextend, &[byte], types::I64)? != read.result {
            return Err(invalid());
        }
        return Ok(());
    }
    let mut instructions = shape::<17>(backwards.take(17))?;
    instructions.reverse();
    let [byte, tag, width, scaled, table, offset, base, displacement, cell, address, one, three, nonzero, nonref, scalar, null, result] =
        instructions;
    let byte = load(
        f,
        byte,
        slots,
        (std::mem::offset_of!(Header, tags) + read.index) as i32,
        types::I8,
    )?;
    let tag = value(f, tag, Opcode::Uextend, &[byte], types::I64)?;
    let width = constant(f, width, 8)?;
    let scaled = value(f, scaled, Opcode::Imul, &[tag, width], types::I64)?;
    let table = value(f, table, Opcode::Iadd, &[slots, scaled], types::I64)?;
    let offset = load(
        f,
        offset,
        table,
        std::mem::offset_of!(Header, offsets) as i32,
        types::I64,
    )?;
    let base = load(
        f,
        base,
        slots,
        std::mem::offset_of!(Header, base) as i32,
        types::I64,
    )?;
    let displacement = constant(
        f,
        displacement,
        (read.index * std::mem::size_of::<crate::Value<'_>>()) as i64,
    )?;
    let cell = value(f, cell, Opcode::Iadd, &[base, displacement], types::I64)?;
    let address = value(f, address, Opcode::Iadd, &[cell, offset], types::I64)?;
    let one = constant(f, one, abi::BOOLEAN as i64)?;
    let three = constant(f, three, abi::NUMBER as i64)?;
    let nonzero = compare(f, nonzero, IntCC::UnsignedGreaterThanOrEqual, [tag, one])?;
    let nonref = compare(f, nonref, IntCC::UnsignedLessThanOrEqual, [tag, three])?;
    let scalar = value(f, scalar, Opcode::Band, &[nonzero, nonref], types::I8)?;
    let null = constant(f, null, 0)?;
    if value(
        f,
        result,
        Opcode::Select,
        &[scalar, address, null],
        types::I64,
    )? != read.result
    {
        return Err(invalid());
    }
    Ok(())
}

pub(in crate::jit::backend) fn lower(
    f: &mut Function,
    slots: IrValue,
    registers: usize,
    allocator: BudgetAllocator,
    expansion: crate::jit::work::Expansion,
) -> Result<(), JitError> {
    if registers == 0 || registers > 256 {
        return Err(invalid());
    }
    let mut records = BudgetVec::new_in(allocator);
    let (mut instructions, blocks) = size(f);
    for block in f.layout.blocks() {
        for inst in f.layout.block_insts(block) {
            let args = f.dfg.inst_args(inst);
            if !args.iter().any(|&v| f.dfg.resolve_aliases(v) == slots) {
                continue;
            }
            match f.dfg.insts[inst] {
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    flags,
                } if arg == slots => {
                    let offset = i32::from(offset);
                    if offset < 0
                        || offset % 8 != 0
                        || offset as usize / 16 >= registers
                        || f.dfg.mem_flags[flags] != MemFlagsData::new()
                        || f.dfg.value_type(f.dfg.first_result(inst)) != types::I64
                    {
                        return Err(invalid());
                    }
                    let pointer = offset % 16 == 8;
                    instructions = instructions
                        .checked_add(if pointer { 16 } else { 1 })
                        .ok_or(JitError::ResourceLimit("strided expansion overflow"))?;
                    records
                        .try_reserve(1)
                        .map_err(|_| JitError::ResourceLimit("strided read records"))?;
                    records.push(Read {
                        inst,
                        index: offset as usize / 16,
                        pointer,
                        result: f.dfg.first_result(inst),
                    });
                }
                InstructionData::CallIndirect {
                    opcode: Opcode::CallIndirect,
                    ..
                } if args.len() >= 3
                    && args[2] == slots
                    && args
                        .iter()
                        .filter(|&&v| f.dfg.resolve_aliases(v) == slots)
                        .count()
                        == 1 => {}
                _ => return Err(invalid()),
            }
        }
    }
    expansion.verify_actual(instructions, blocks)?;
    let mut candidate = f.clone();
    for &read in &records {
        emit(&mut candidate, slots, read);
    }
    for &read in &records {
        verify(&candidate, slots, read)?;
    }
    if size(&candidate) != (instructions, blocks) {
        return Err(invalid());
    }
    *f = candidate;
    Ok(())
}

#[cfg(test)]
mod tests;
