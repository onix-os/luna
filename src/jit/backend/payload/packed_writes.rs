use super::*;
use crate::jit::abi::payload::packed::scoped::bridge::{
    Bridge as PackedBridge, Functions as PackedFunctions,
};
use cranelift_codegen::ir::SigRef;

const _: () = {
    use crate::jit::abi::payload::packed::*;
    assert!(MASK == 3 && INT == 0 && FLOAT == 1 && OPAQUE == 5);
    assert!(abi::INTEGER == 2 && abi::NUMBER == 3 && std::mem::size_of::<Packed>() == 8);
};

#[derive(Clone, Copy)]
pub(super) struct Input {
    tag_store: Inst,
    bits_store: Inst,
    slots: IrValue,
    host: IrValue,
    index: u32,
    tag: IrValue,
    bits: IrValue,
    terminal: bool,
}

pub(super) struct Write {
    input: Input,
    blocks: [Block; 5],
    signature: SigRef,
    cast: Option<Inst>,
}

pub(super) fn admit(
    f: &Function,
    tag_store: Inst,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    terminal: bool,
) -> Result<Input, JitError> {
    let InstructionData::Store {
        opcode: Opcode::Store,
        args,
        offset,
        flags,
    } = f.dfg.insts[tag_store]
    else {
        return Err(invalid());
    };
    let offset = i32::from(offset);
    if registers > 256
        || offset < 0
        || offset % 16 != 0
        || offset as usize / 16 >= registers
        || args[1] != slots
        || f.dfg.value_type(slots) != types::I64
        || f.dfg.value_type(host) != types::I64
        || f.dfg.value_type(args[0]) != types::I64
        || f.dfg.mem_flags[flags] != MemFlagsData::new()
    {
        return Err(invalid());
    }
    let bits_store = f.layout.next_inst(tag_store).ok_or_else(invalid)?;
    let InstructionData::Store {
        opcode: Opcode::Store,
        args: second,
        offset: second_offset,
        flags: second_flags,
    } = f.dfg.insts[bits_store]
    else {
        return Err(invalid());
    };
    if second[1] != slots
        || i32::from(second_offset) != offset + 8
        || !matches!(f.dfg.value_type(second[0]), types::I64 | types::F64)
        || f.dfg.mem_flags[second_flags] != MemFlagsData::new()
        || f.layout.next_inst(bits_store).is_none()
    {
        return Err(invalid());
    }
    Ok(Input {
        tag_store,
        bits_store,
        slots,
        host,
        index: offset as u32 / 16,
        tag: args[0],
        bits: second[0],
        terminal,
    })
}

pub(super) fn emit(f: &mut Function, input: Input) -> Write {
    let mut signature = Signature::new(f.signature.call_conv);
    signature
        .params
        .extend([types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    signature.returns.push(AbiParam::new(types::I32));
    let signature = f.import_signature(signature);
    let before = f.layout.inst_block(input.bits_store).unwrap();
    let after = f.dfg.make_block();
    f.layout.remove_inst(input.tag_store);
    f.layout.split_block(after, input.bits_store);
    f.layout.remove_inst(input.bits_store);
    let fast = reads::block(f);
    let slow = reads::block(f);
    let failed = reads::block(f);
    let mut c = FuncCursor::new(f);
    c.goto_bottom(before);
    let (bits, cast) = if c.func.dfg.value_type(input.bits) == types::F64 {
        let bits = c.ins().bitcast(types::I64, MemFlagsData::new(), input.bits);
        (bits, c.func.dfg.value_def(bits).inst())
    } else {
        (input.bits, None)
    };
    let packed = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        input.slots,
        input.index as i32 * 8,
    );
    let mask = c.ins().iconst(types::I64, 3);
    let low = c.ins().band(packed, mask);
    let top = c.ins().iconst(types::I64, 5);
    let above = c.ins().icmp(IntCC::UnsignedGreaterThan, packed, top);
    let one = c.ins().iconst(types::I64, 1);
    let numeric = c.ins().icmp(IntCC::UnsignedLessThanOrEqual, low, one);
    let live = c.ins().band(above, numeric);
    let two = c.ins().iconst(types::I64, 2);
    let wanted = c.ins().isub(input.tag, two);
    let same = c.ins().icmp(IntCC::Equal, low, wanted);
    let valid = c.ins().band(live, same);
    c.ins().brif(valid, fast, &[], slow, &[]);
    c.goto_bottom(fast);
    let mask = c.ins().iconst(types::I64, !3);
    let pointer = c.ins().band(packed, mask);
    c.ins().store(MemFlagsData::new(), bits, pointer, 0);
    c.ins().jump(after, &[]);
    c.goto_bottom(slow);
    let control = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        input.host,
        std::mem::offset_of!(PackedBridge, control) as i32,
    );
    let table = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        input.host,
        std::mem::offset_of!(PackedBridge, functions) as i32,
    );
    let callee = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        table,
        if input.terminal {
            std::mem::offset_of!(PackedFunctions, terminal)
        } else {
            std::mem::offset_of!(PackedFunctions, write)
        } as i32,
    );
    let index = c.ins().iconst(types::I32, input.index as i64);
    let call = c
        .ins()
        .call_indirect(signature, callee, &[control, index, input.tag, bits]);
    let status = c.func.dfg.inst_results(call)[0];
    let completed = c.ins().iconst(types::I32, abi::HELPER_COMPLETED as i64);
    let success = c.ins().icmp(IntCC::Equal, status, completed);
    c.ins().brif(success, after, &[], failed, &[]);
    c.goto_bottom(failed);
    c.ins().return_(&[status]);
    Write {
        input,
        blocks: [before, after, fast, slow, failed],
        signature,
        cast,
    }
}

pub(super) fn verify_source(
    f: &Function,
    write: &Write,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    terminal: &mut terminal::Analysis<'_>,
) -> Result<(), JitError> {
    let input = write.input;
    let actual = admit(f, input.tag_store, slots, host, registers, input.terminal)?;
    if actual.bits_store != input.bits_store
        || actual.index != input.index
        || actual.tag != input.tag
        || actual.bits != input.bits
        || actual.slots != input.slots
        || actual.host != input.host
        || (input.terminal && !terminal.admits_source(f, input.bits_store, input.index))
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify(f: &Function, write: &Write) -> Result<(), JitError> {
    let input = write.input;
    let [before, after, fast, slow, failed] = write.blocks;
    if input.index >= 256
        || f.signature.returns != [AbiParam::new(types::I32)]
        || [after, fast, slow, failed]
            .iter()
            .any(|&b| !f.dfg.block_params(b).is_empty())
    {
        return Err(invalid());
    }
    let [branch, valid, same, wanted, two, live, numeric, one, above, top, low, mask, packed] =
        shape(f.layout.block_insts(before).rev().take(13))?;
    let bits = match (f.dfg.value_type(input.bits), write.cast) {
        (types::I64, None) => input.bits,
        (types::F64, Some(cast)) if f.layout.prev_inst(packed) == Some(cast) => {
            reads::bitcast(f, cast, input.bits, types::I64)?
        }
        _ => return Err(invalid()),
    };
    let packed = reads::load(f, packed, input.slots, input.index as i32 * 8, types::I64)?;
    let mask = reads::constant(f, mask, 3)?;
    let low = reads::value(f, low, Opcode::Band, &[packed, mask], types::I64)?;
    let top = reads::constant(f, top, 5)?;
    let above = reads::compare(f, above, IntCC::UnsignedGreaterThan, [packed, top])?;
    let one = reads::constant(f, one, 1)?;
    let numeric = reads::compare(f, numeric, IntCC::UnsignedLessThanOrEqual, [low, one])?;
    let live = reads::value(f, live, Opcode::Band, &[above, numeric], types::I8)?;
    let two = reads::constant(f, two, 2)?;
    let wanted = reads::value(f, wanted, Opcode::Isub, &[input.tag, two], types::I64)?;
    let same = reads::compare(f, same, IntCC::Equal, [low, wanted])?;
    let valid = reads::value(f, valid, Opcode::Band, &[live, same], types::I8)?;
    reads::branch(f, branch, valid, fast, slow)?;
    let [mask, pointer, store, jump] = shape(f.layout.block_insts(fast))?;
    let mask = reads::constant(f, mask, !3)?;
    let pointer = reads::value(f, pointer, Opcode::Band, &[packed, mask], types::I64)?;
    writes::store(f, store, bits, pointer)?;
    writes::jump(f, jump, after)?;
    let [control, table, callee, index, call, completed, success, branch] =
        shape(f.layout.block_insts(slow))?;
    let control = reads::load(
        f,
        control,
        input.host,
        std::mem::offset_of!(PackedBridge, control) as i32,
        types::I64,
    )?;
    let table = reads::load(
        f,
        table,
        input.host,
        std::mem::offset_of!(PackedBridge, functions) as i32,
        types::I64,
    )?;
    let callee = reads::load(
        f,
        callee,
        table,
        if input.terminal {
            std::mem::offset_of!(PackedFunctions, terminal)
        } else {
            std::mem::offset_of!(PackedFunctions, write)
        } as i32,
        types::I64,
    )?;
    let index_value = reads::value(f, index, Opcode::Iconst, &[], types::I32)?;
    if !matches!(f.dfg.insts[index], InstructionData::UnaryImm { imm, .. } if i64::from(imm) == input.index as i64)
    {
        return Err(invalid());
    }
    let status = reads::value(
        f,
        call,
        Opcode::CallIndirect,
        &[callee, control, index_value, input.tag, bits],
        types::I32,
    )?;
    let InstructionData::CallIndirect { sig_ref, .. } = f.dfg.insts[call] else {
        return Err(invalid());
    };
    let signature = &f.dfg.signatures[sig_ref];
    if sig_ref != write.signature
        || signature.call_conv != f.signature.call_conv
        || signature
            .params
            .iter()
            .copied()
            .ne([types::I64, types::I32, types::I64, types::I64].map(AbiParam::new))
        || signature.returns != [AbiParam::new(types::I32)]
    {
        return Err(invalid());
    }
    let completed_value = reads::value(f, completed, Opcode::Iconst, &[], types::I32)?;
    if !matches!(f.dfg.insts[completed], InstructionData::UnaryImm { imm, .. } if i64::from(imm) == abi::HELPER_COMPLETED as i64)
    {
        return Err(invalid());
    }
    let success = reads::compare(f, success, IntCC::Equal, [status, completed_value])?;
    reads::branch(f, branch, success, after, failed)?;
    let [returned] = shape(f.layout.block_insts(failed))?;
    if f.dfg.insts[returned].opcode() != Opcode::Return || f.dfg.inst_args(returned) != [status] {
        return Err(invalid());
    }
    Ok(())
}

#[path = "packed_write_tests.rs"]
mod tests;
