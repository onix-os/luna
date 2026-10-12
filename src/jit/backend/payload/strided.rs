use super::*;
use crate::jit::abi::strided::frame::Header;

mod guarded;
mod memory;
mod scalar_range;
mod uniform;

#[derive(Clone, Copy)]
struct Read {
    inst: Inst,
    index: usize,
    pointer: bool,
    result: IrValue,
    uniform: Option<usize>,
    numeric: bool,
}

impl Read {
    fn extra(self) -> usize {
        if !self.pointer {
            0
        } else if self.numeric {
            2
        } else if self.uniform.is_some() {
            9
        } else {
            14
        }
    }
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
    if read.numeric {
        guarded::emit(f, slots, read);
        return;
    }
    if read.pointer && read.uniform.is_some() {
        uniform::emit(f, slots, read);
        return;
    }
    let mut c = FuncCursor::new(f);
    c.goto_inst(read.inst);
    if !read.pointer {
        c.func.replace(read.inst).uload8(
            types::I64,
            MemFlagsData::new(),
            slots,
            (std::mem::offset_of!(Header, tags) + read.index) as i32,
        );
        return;
    }
    let tag = c.ins().uload8(
        types::I64,
        MemFlagsData::new(),
        slots,
        (std::mem::offset_of!(Header, tags) + read.index) as i32,
    );
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
    let scalar = scalar_range::emit(&mut c, tag);
    let null = c.ins().iconst(types::I64, 0);
    c.func.replace(read.inst).select(scalar, address, null);
}

fn verify_tag(f: &Function, inst: Inst, slots: IrValue, index: usize) -> Result<IrValue, JitError> {
    let InstructionData::Load { offset, flags, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if i32::from(offset) != (std::mem::offset_of!(Header, tags) + index) as i32
        || f.dfg.mem_flags[flags] != MemFlagsData::new()
    {
        return Err(invalid());
    }
    reads::value(f, inst, Opcode::Uload8, &[slots], types::I64)
}

fn verify(f: &Function, slots: IrValue, read: Read) -> Result<(), JitError> {
    if read.numeric {
        if !read.pointer || read.uniform.is_none() || read.uniform != Header::uniform_offset() {
            return Err(invalid());
        }
        return guarded::verify(f, slots, read);
    }
    if let Some(offset) = read.uniform {
        if !read.pointer || Header::uniform_offset() != Some(offset) {
            return Err(invalid());
        }
        return uniform::verify(f, slots, read);
    }
    use reads::{constant, load, value};
    let mut previous = Some(read.inst);
    let backwards = std::iter::from_fn(|| {
        let inst = previous?;
        previous = f.layout.prev_inst(inst);
        Some(inst)
    });
    if !read.pointer {
        if verify_tag(f, read.inst, slots, read.index)? != read.result {
            return Err(invalid());
        }
        return Ok(());
    }
    let mut instructions = shape::<15>(backwards.take(15))?;
    instructions.reverse();
    let [tag, width, scaled, table, offset, base, displacement, cell, address, first, adjusted, range, scalar, null, result] =
        instructions;
    let tag = verify_tag(f, tag, slots, read.index)?;
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
    let scalar = scalar_range::verify(f, [first, adjusted, range, scalar], tag)?;
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
    lower_impl(f, slots, registers, allocator, expansion, false).map(drop)
}

pub(in crate::jit::backend) fn lower_partitioned(
    f: &mut Function,
    slots: IrValue,
    registers: usize,
    allocator: BudgetAllocator,
    expansion: crate::jit::work::Expansion,
) -> Result<Reservation, JitError> {
    lower_impl(f, slots, registers, allocator, expansion, true)?.ok_or_else(invalid)
}

fn lower_impl(
    f: &mut Function,
    slots: IrValue,
    registers: usize,
    allocator: BudgetAllocator,
    expansion: crate::jit::work::Expansion,
    partition: bool,
) -> Result<Option<Reservation>, JitError> {
    if registers == 0 || registers > 256 {
        return Err(invalid());
    }
    let mut records = BudgetVec::new_in(allocator.clone());
    let mut analysis = same_tag::Analysis::with_bound_stores(f, allocator.clone())?;
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
                    let uniform = if pointer {
                        Header::uniform_offset()
                    } else {
                        None
                    };
                    let index = offset as usize / 16;
                    let read = Read {
                        inst,
                        index,
                        pointer,
                        result: f.dfg.first_result(inst),
                        uniform,
                        numeric: uniform.is_some()
                            && analysis.admit_numeric(f, slots, inst, index as u32),
                    };
                    instructions = instructions
                        .checked_add(read.extra())
                        .ok_or(JitError::ResourceLimit("strided expansion overflow"))?;
                    records
                        .try_reserve(1)
                        .map_err(|_| JitError::ResourceLimit("strided read records"))?;
                    records.push(read);
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
    guarded::verify_plan(f, &candidate, slots, &records, &mut analysis)?;
    if size(&candidate) != (instructions, blocks) {
        return Err(invalid());
    }
    let charge = if partition {
        Some(memory::lower(&mut candidate, slots, &records, allocator)?)
    } else {
        None
    };
    *f = candidate;
    Ok(charge)
}

#[cfg(test)]
mod guarded_tests;
#[cfg(test)]
mod tests;
