use super::reads::{block, branch, compare, constant, load, value};
use super::*;

#[derive(Clone, Copy)]
pub(super) struct Input {
    pub slots: IrValue,
    pub host: IrValue,
    pub index: u32,
    pub tag: IrValue,
    pub bits: IrValue,
    pub signature: cranelift_codegen::ir::SigRef,
}

pub(super) struct Write {
    blocks: [Block; 8],
    input: Input,
}

pub(super) fn emit(f: &mut Function, tag_store: Inst, bits_store: Inst, input: Input) -> Write {
    let before = f.layout.inst_block(bits_store).unwrap();
    let after = f.dfg.make_block();
    f.layout.remove_inst(tag_store);
    f.layout.split_block(after, bits_store);
    f.layout.remove_inst(bits_store);
    let kind = block(f);
    let wide = block(f);
    let other = block(f);
    let boolean = block(f);
    let nil = block(f);
    let slow = block(f);
    let Input {
        slots,
        host,
        index,
        tag,
        bits,
        signature,
    } = input;
    let mut c = FuncCursor::new(f);
    c.goto_bottom(before);
    let actual = c
        .ins()
        .load(types::I64, MemFlagsData::new(), slots, index as i32 * 16);
    let pointer = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
    );
    let same = c.ins().icmp(IntCC::Equal, actual, tag);
    c.ins().brif(same, kind, &[], slow, &[]);
    c.goto_bottom(kind);
    let integer = c.ins().iconst(types::I64, abi::INTEGER as i64);
    let is_integer = c.ins().icmp(IntCC::Equal, tag, integer);
    let number = c.ins().iconst(types::I64, abi::NUMBER as i64);
    let is_number = c.ins().icmp(IntCC::Equal, tag, number);
    let is_wide = c.ins().bor(is_integer, is_number);
    let null = c.ins().iconst(types::I64, 0);
    let nonnull = c.ins().icmp(IntCC::NotEqual, pointer, null);
    let valid = c.ins().band(is_wide, nonnull);
    c.ins().brif(valid, wide, &[], other, &[]);
    c.goto_bottom(wide);
    c.ins().store(MemFlagsData::new(), bits, pointer, 0);
    c.ins().jump(after, &[]);
    c.goto_bottom(other);
    let boolean_tag = c.ins().iconst(types::I64, abi::BOOLEAN as i64);
    let is_boolean = c.ins().icmp(IntCC::Equal, tag, boolean_tag);
    let valid_bits = c
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bits, boolean_tag);
    let valid = c.ins().band(is_boolean, nonnull);
    let valid = c.ins().band(valid, valid_bits);
    c.ins().brif(valid, boolean, &[], nil, &[]);
    c.goto_bottom(boolean);
    let byte = c.ins().ireduce(types::I8, bits);
    c.ins().store(MemFlagsData::new(), byte, pointer, 0);
    c.ins().jump(after, &[]);
    c.goto_bottom(nil);
    let is_nil = c.ins().icmp(IntCC::Equal, tag, null);
    let is_null = c.ins().icmp(IntCC::Equal, pointer, null);
    let valid = c.ins().band(is_nil, is_null);
    c.ins().brif(valid, after, &[], slow, &[]);
    c.goto_bottom(slow);
    let target = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        host,
        std::mem::offset_of!(Bridge, write) as i32,
    );
    let index = c.ins().iconst(types::I32, i64::from(index));
    c.ins()
        .call_indirect(signature, target, &[host, slots, index, tag, bits]);
    c.ins().jump(after, &[]);
    Write {
        blocks: [before, after, kind, wide, other, boolean, nil, slow],
        input,
    }
}

fn jump(f: &Function, inst: Inst, after: Block) -> Result<(), JitError> {
    let InstructionData::Jump { destination, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if destination.block(&f.dfg.value_lists) != after
        || destination.args(&f.dfg.value_lists).next().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

fn store(f: &Function, inst: Inst, bits: IrValue, pointer: IrValue) -> Result<(), JitError> {
    let InstructionData::Store {
        opcode: Opcode::Store,
        args,
        offset,
        flags,
    } = f.dfg.insts[inst]
    else {
        return Err(invalid());
    };
    if args != [bits, pointer]
        || i32::from(offset) != 0
        || f.dfg.mem_flags[flags] != MemFlagsData::new()
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify(f: &Function, write: &Write) -> Result<(), JitError> {
    let [before, after, kind, wide, other, boolean, nil, slow] = write.blocks;
    let Input {
        slots,
        host,
        index,
        tag,
        bits,
        signature,
    } = write.input;
    for block in [after, kind, wide, other, boolean, nil, slow] {
        if !f.dfg.block_params(block).is_empty() {
            return Err(invalid());
        }
    }
    let mut head: Vec<_> = f.layout.block_insts(before).rev().take(4).collect();
    head.reverse();
    let [actual, pointer, same, split]: [Inst; 4] = head.try_into().map_err(|_| invalid())?;
    let actual = load(f, actual, slots, index as i32 * 16, types::I64)?;
    let pointer = load(
        f,
        pointer,
        slots,
        index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
        types::I64,
    )?;
    let same = compare(f, same, IntCC::Equal, [actual, tag])?;
    branch(f, split, same, kind, slow)?;
    let [integer, is_integer, number, is_number, is_wide, null, nonnull, valid, split]: [Inst; 9] =
        f.layout
            .block_insts(kind)
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| invalid())?;
    let integer = constant(f, integer, abi::INTEGER as i64)?;
    let is_integer = compare(f, is_integer, IntCC::Equal, [tag, integer])?;
    let number = constant(f, number, abi::NUMBER as i64)?;
    let is_number = compare(f, is_number, IntCC::Equal, [tag, number])?;
    let is_wide = value(f, is_wide, Opcode::Bor, &[is_integer, is_number], types::I8)?;
    let null = constant(f, null, 0)?;
    let nonnull = compare(f, nonnull, IntCC::NotEqual, [pointer, null])?;
    let valid = value(f, valid, Opcode::Band, &[is_wide, nonnull], types::I8)?;
    branch(f, split, valid, wide, other)?;
    let [write_inst, end]: [Inst; 2] = f
        .layout
        .block_insts(wide)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    store(f, write_inst, bits, pointer)?;
    jump(f, end, after)?;
    let [boolean_tag, is_boolean, valid_bits, nonnull_boolean, valid, split]: [Inst; 6] = f
        .layout
        .block_insts(other)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    let boolean_tag = constant(f, boolean_tag, abi::BOOLEAN as i64)?;
    let is_boolean = compare(f, is_boolean, IntCC::Equal, [tag, boolean_tag])?;
    let valid_bits = compare(
        f,
        valid_bits,
        IntCC::UnsignedLessThanOrEqual,
        [bits, boolean_tag],
    )?;
    let nonnull_boolean = value(
        f,
        nonnull_boolean,
        Opcode::Band,
        &[is_boolean, nonnull],
        types::I8,
    )?;
    let valid = value(
        f,
        valid,
        Opcode::Band,
        &[nonnull_boolean, valid_bits],
        types::I8,
    )?;
    branch(f, split, valid, boolean, nil)?;
    let [reduce, write_inst, end]: [Inst; 3] = f
        .layout
        .block_insts(boolean)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    let byte = value(f, reduce, Opcode::Ireduce, &[bits], types::I8)?;
    store(f, write_inst, byte, pointer)?;
    jump(f, end, after)?;
    let [is_nil, is_null, valid, split]: [Inst; 4] = f
        .layout
        .block_insts(nil)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    let is_nil = compare(f, is_nil, IntCC::Equal, [tag, null])?;
    let is_null = compare(f, is_null, IntCC::Equal, [pointer, null])?;
    let valid = value(f, valid, Opcode::Band, &[is_nil, is_null], types::I8)?;
    branch(f, split, valid, after, slow)?;
    let [target, register, call, end]: [Inst; 4] = f
        .layout
        .block_insts(slow)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    let target = load(
        f,
        target,
        host,
        std::mem::offset_of!(Bridge, write) as i32,
        types::I64,
    )?;
    let register_value = value(f, register, Opcode::Iconst, &[], types::I32)?;
    let InstructionData::UnaryImm { imm, .. } = f.dfg.insts[register] else {
        return Err(invalid());
    };
    if i64::from(imm) != i64::from(index) {
        return Err(invalid());
    }
    let InstructionData::CallIndirect {
        opcode: Opcode::CallIndirect,
        sig_ref,
        ..
    } = f.dfg.insts[call]
    else {
        return Err(invalid());
    };
    if sig_ref != signature
        || f.dfg.signatures[sig_ref].call_conv != f.signature.call_conv
        || f.dfg.signatures[sig_ref].params
            != [types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new)
        || !f.dfg.signatures[sig_ref].returns.is_empty()
        || f.dfg.inst_args(call) != [target, host, slots, register_value, tag, bits]
        || !f.dfg.inst_results(call).is_empty()
    {
        return Err(invalid());
    }
    jump(f, end, after)
}

fn fixture() -> (Function, Write) {
    let mut function = Function::new();
    function.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    function.signature.params = vec![AbiParam::new(types::I64); 4];
    let call_conv = function.signature.call_conv;
    let signature = function.import_signature(Signature {
        params: [types::I64, types::I64, types::I32, types::I64, types::I64]
            .map(AbiParam::new)
            .to_vec(),
        returns: vec![],
        call_conv,
    });
    let entry = block(&mut function);
    let args: Vec<_> = (0..4)
        .map(|_| function.dfg.append_block_param(entry, types::I64))
        .collect();
    let [slots, host, tag, bits]: [IrValue; 4] = args.try_into().unwrap();
    let mut c = FuncCursor::new(&mut function);
    c.goto_bottom(entry);
    let tag_store = c.ins().store(MemFlagsData::new(), tag, slots, 0);
    let bits_store = c.ins().store(MemFlagsData::new(), bits, slots, 8);
    c.ins().return_(&[]);
    let write = emit(
        &mut function,
        tag_store,
        bits_store,
        Input {
            slots,
            host,
            index: 0,
            tag,
            bits,
            signature,
        },
    );
    verify(&function, &write).unwrap();
    (function, write)
}

#[test]
fn inline_write_checker_rejects_well_formed_guard_store_and_gateway_mutations() {
    let (function, write) = fixture();
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&function, &flags).unwrap();
    let mut rejected = 0;
    let mut reject = |changed: Function| {
        cranelift_codegen::verify_function(&changed, &flags).unwrap();
        assert!(verify(&changed, &write).is_err());
        rejected += 1;
    };
    for block in write
        .blocks
        .into_iter()
        .filter(|block| *block != write.blocks[1])
    {
        for inst in function.layout.block_insts(block) {
            let mut changed = function.clone();
            let dfg = &mut changed.dfg;
            match &mut dfg.insts[inst] {
                InstructionData::Load { offset, .. } | InstructionData::Store { offset, .. } => {
                    *offset = (i32::from(*offset) + 1).into()
                }
                InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                InstructionData::IntCompare { cond, .. } => {
                    *cond = if *cond == IntCC::Equal {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    };
                }
                InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bxor,
                InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                InstructionData::Unary { arg, .. } => *arg = write.input.tag,
                InstructionData::Jump { destination, .. } => {
                    *destination = cranelift_codegen::ir::BlockCall::new(
                        write.blocks[7],
                        [],
                        &mut dfg.value_lists,
                    );
                }
                InstructionData::CallIndirect { .. } => dfg.inst_args_mut(inst).swap(1, 2),
                _ => panic!("unhandled write instruction"),
            }
            reject(changed);
        }
    }
    let boolean: Vec<_> = function.layout.block_insts(write.blocks[5]).collect();
    let mut changed = function.clone();
    changed.dfg.inst_args_mut(boolean[1])[0] = write.input.bits;
    reject(changed);
    let mut changed = function.clone();
    let end = changed.layout.last_inst(write.blocks[6]).unwrap();
    let mut c = FuncCursor::new(&mut changed);
    c.goto_inst(end);
    c.ins()
        .store(MemFlagsData::new(), write.input.bits, write.input.slots, 0);
    reject(changed);
    let mut changed = function.clone();
    changed.dfg.signatures[write.input.signature].call_conv =
        cranelift_codegen::isa::CallConv::Fast;
    reject(changed);
    let slow: Vec<_> = function.layout.block_insts(write.blocks[7]).collect();
    let mut changed = function.clone();
    changed.dfg.inst_args_mut(slow[2]).swap(4, 5);
    reject(changed);
    assert_eq!(rejected, 36);
}

#[cfg(not(miri))]
#[test]
fn generated_inline_write_preserves_values_and_only_materializes_type_changes() {
    let (function, write) = fixture();
    verify(&function, &write).unwrap();
    crate::jit::backend::projection_probe::with_projection_probe(function, |pointer| {
        let entry = unsafe {
            std::mem::transmute::<
                *const u8,
                unsafe extern "C" fn(*mut Payload, *mut Bridge, u64, u64),
            >(pointer)
        };
        crate::jit::abi::payload::runtime::check_native_write(entry);
    })
    .unwrap();
}
