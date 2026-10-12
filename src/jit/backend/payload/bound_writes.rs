use super::*;

#[cfg(test)]
#[path = "bound_write_tests.rs"]
mod tests;

pub(super) struct Write {
    blocks: [Block; 2],
    slots: IrValue,
    index: u32,
    tag: IrValue,
    bits: IrValue,
    proof: same_tag::Proof,
    cast: Option<Inst>,
}

pub(super) fn numeric(f: &Function, tag: IrValue) -> bool {
    let Some(inst) = f.dfg.value_def(tag).inst() else {
        return false;
    };
    matches!(f.dfg.insts[inst], InstructionData::UnaryImm { opcode: Opcode::Iconst, imm }
        if matches!(i64::from(imm) as u64, abi::INTEGER | abi::NUMBER))
        && f.dfg.value_type(tag) == types::I64
}

pub(super) fn emit(
    f: &mut Function,
    tag_store: Inst,
    bits_store: Inst,
    slots: IrValue,
    index: u32,
    tag: IrValue,
    bits: IrValue,
    proof: same_tag::Proof,
) -> Write {
    let before = f.layout.inst_block(bits_store).unwrap();
    let after = f.dfg.make_block();
    f.layout.remove_inst(tag_store);
    f.layout.split_block(after, bits_store);
    f.layout.remove_inst(bits_store);
    let mut c = FuncCursor::new(f);
    c.goto_bottom(before);
    let (stored, cast) = if c.func.dfg.value_type(bits) == types::F64 {
        let stored = c.ins().bitcast(types::I64, MemFlagsData::new(), bits);
        (stored, c.func.dfg.value_def(stored).inst())
    } else {
        (bits, None)
    };
    let pointer = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
    );
    c.ins().store(MemFlagsData::new(), stored, pointer, 0);
    c.ins().jump(after, &[]);
    Write {
        blocks: [before, after],
        slots,
        index,
        tag,
        bits,
        proof,
        cast,
    }
}

pub(super) fn verify_source(
    f: &Function,
    write: &Write,
    analysis: &mut same_tag::Analysis,
) -> Result<(), JitError> {
    if !numeric(f, write.tag)
        || !write.proof.matches(write.index, write.tag)
        || !analysis.verify(f, write.slots, write.proof)
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify(
    f: &Function,
    write: &Write,
    predecessors: &crate::jit::preds::Predecessors,
    remaining: &mut usize,
) -> Result<(), JitError> {
    let [before, after] = write.blocks;
    if !numeric(f, write.tag)
        || !write.proof.matches(write.index, write.tag)
        || !f.dfg.block_params(after).is_empty()
    {
        return Err(invalid());
    }
    let [end, store, pointer] = shape(f.layout.block_insts(before).rev().take(3))?;
    let bits = match (f.dfg.value_type(write.bits), write.cast) {
        (types::I64, None) => write.bits,
        (types::F64, Some(cast)) if f.layout.prev_inst(pointer) == Some(cast) => {
            reads::bitcast(f, cast, write.bits, types::I64)?
        }
        _ => return Err(invalid()),
    };
    let address = reads::load(
        f,
        pointer,
        write.slots,
        write.index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
        types::I64,
    )?;
    writes::store(f, store, bits, address)?;
    writes::jump(f, end, after)?;
    if !same_tag::verify_bound_at(
        f,
        predecessors,
        write.slots,
        pointer,
        write.index,
        write.tag,
        remaining,
    ) {
        return Err(invalid());
    }
    Ok(())
}
