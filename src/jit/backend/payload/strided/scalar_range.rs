use super::*;

const _: () = assert!(abi::BOOLEAN == 1 && abi::INTEGER == 2 && abi::NUMBER == 3);

pub(super) fn emit(c: &mut FuncCursor<'_>, tag: IrValue) -> IrValue {
    let first = c.ins().iconst(types::I64, abi::BOOLEAN as i64);
    let offset = c.ins().isub(tag, first);
    let width = c
        .ins()
        .iconst(types::I64, (abi::NUMBER - abi::BOOLEAN) as i64);
    c.ins().icmp(IntCC::UnsignedLessThanOrEqual, offset, width)
}

pub(super) fn verify(
    f: &Function,
    instructions: [Inst; 4],
    tag: IrValue,
) -> Result<IrValue, JitError> {
    let [first, offset, width, scalar] = instructions;
    let first = reads::constant(f, first, abi::BOOLEAN as i64)?;
    let offset = reads::value(f, offset, Opcode::Isub, &[tag, first], types::I64)?;
    let width = reads::constant(f, width, (abi::NUMBER - abi::BOOLEAN) as i64)?;
    reads::compare(f, scalar, IntCC::UnsignedLessThanOrEqual, [offset, width])
}
