use super::*;

#[cfg(test)]
#[path = "bound_read_tests.rs"]
mod tests;

pub(super) struct Read {
    inst: Inst,
    pointer: Inst,
    result: IrValue,
    ty: cranelift_codegen::ir::Type,
    index: u32,
}

pub(super) fn emit(f: &mut Function, inst: Inst, slots: IrValue, index: u32) -> Read {
    let result = f.dfg.first_result(inst);
    let ty = f.dfg.value_type(result);
    let mut c = FuncCursor::new(f);
    c.goto_inst(inst);
    let pointer = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
    );
    let pointer = c.func.dfg.value_def(pointer).inst().unwrap();
    let address = c.func.dfg.first_result(pointer);
    c.func
        .replace(inst)
        .load(ty, MemFlagsData::new(), address, 0);
    Read {
        inst,
        pointer,
        result,
        ty,
        index,
    }
}

pub(super) fn verify_source(
    f: &Function,
    read: &Read,
    slots: IrValue,
    analysis: &mut same_tag::Analysis,
) -> Result<(), JitError> {
    if !matches!(read.ty, types::I64 | types::F64)
        || reads::load(f, read.inst, slots, read.index as i32 * 16 + 8, read.ty)? != read.result
        || !analysis.verify_numeric(f, slots, read.inst, read.index)
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify(
    f: &Function,
    read: &Read,
    slots: IrValue,
    predecessors: &crate::jit::preds::Predecessors,
    remaining: &mut usize,
) -> Result<(), JitError> {
    if !matches!(read.ty, types::I64 | types::F64)
        || f.layout.next_inst(read.pointer) != Some(read.inst)
        || !same_tag::numeric_at(f, predecessors, slots, read.pointer, read.index, remaining)
    {
        return Err(invalid());
    }
    let pointer = reads::load(
        f,
        read.pointer,
        slots,
        read.index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
        types::I64,
    )?;
    if reads::load(f, read.inst, pointer, 0, read.ty)? != read.result {
        return Err(invalid());
    }
    Ok(())
}
