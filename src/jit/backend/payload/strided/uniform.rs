use super::*;

pub(super) fn emit(f: &mut Function, slots: IrValue, read: Read) {
    let mut c = FuncCursor::new(f);
    c.goto_inst(read.inst);
    let byte = c.ins().load(
        types::I8,
        MemFlagsData::new(),
        slots,
        (std::mem::offset_of!(Header, tags) + read.index) as i32,
    );
    let tag = c.ins().uextend(types::I64, byte);
    let base = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        std::mem::offset_of!(Header, base) as i32,
    );
    let displacement = c.ins().iconst(
        types::I64,
        (read.index * std::mem::size_of::<crate::Value<'_>>() + read.uniform.unwrap()) as i64,
    );
    let address = c.ins().iadd(base, displacement);
    let one = c.ins().iconst(types::I64, abi::BOOLEAN as i64);
    let three = c.ins().iconst(types::I64, abi::NUMBER as i64);
    let nonzero = c.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, tag, one);
    let nonref = c.ins().icmp(IntCC::UnsignedLessThanOrEqual, tag, three);
    let scalar = c.ins().band(nonzero, nonref);
    let null = c.ins().iconst(types::I64, 0);
    c.func.replace(read.inst).select(scalar, address, null);
}

pub(super) fn verify(f: &Function, slots: IrValue, read: Read) -> Result<(), JitError> {
    use reads::{compare, constant, load, value};
    let mut previous = Some(read.inst);
    let backwards = std::iter::from_fn(|| {
        let inst = previous?;
        previous = f.layout.prev_inst(inst);
        Some(inst)
    });
    let mut instructions = shape::<12>(backwards.take(12))?;
    instructions.reverse();
    let [byte, tag, base, displacement, address, one, three, nonzero, nonref, scalar, null, result] =
        instructions;
    let byte = load(
        f,
        byte,
        slots,
        (std::mem::offset_of!(Header, tags) + read.index) as i32,
        types::I8,
    )?;
    let tag = value(f, tag, Opcode::Uextend, &[byte], types::I64)?;
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
        (read.index * std::mem::size_of::<crate::Value<'_>>() + read.uniform.ok_or_else(invalid)?)
            as i64,
    )?;
    let address = value(f, address, Opcode::Iadd, &[base, displacement], types::I64)?;
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
