use super::*;

pub(super) struct Read {
    blocks: [Block; 6],
    result: IrValue,
    index: u32,
}

fn block(function: &mut Function) -> Block {
    let block = function.dfg.make_block();
    function.layout.append_block(block);
    block
}

pub(super) fn emit(function: &mut Function, inst: Inst, slots: IrValue, index: u32) -> Read {
    let before = function.layout.inst_block(inst).unwrap();
    let after = function.dfg.make_block();
    function.layout.split_block(after, inst);
    let result = function.dfg.first_result(inst);
    function.dfg.detach_inst_results(inst);
    function.dfg.attach_block_param(after, result);
    function.layout.remove_inst(inst);
    let wide = block(function);
    let other = block(function);
    let boolean = block(function);
    let zero = block(function);
    let mut c = FuncCursor::new(function);
    c.goto_bottom(before);
    let offset = index as i32 * 16;
    let tag = c.ins().load(types::I64, MemFlagsData::new(), slots, offset);
    let pointer = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        offset + crate::jit::abi::payload::POINTER_OFFSET as i32,
    );
    let integer = c.ins().iconst(types::I64, abi::INTEGER as i64);
    let is_integer = c.ins().icmp(IntCC::Equal, tag, integer);
    let number = c.ins().iconst(types::I64, abi::NUMBER as i64);
    let is_number = c.ins().icmp(IntCC::Equal, tag, number);
    let is_wide = c.ins().bor(is_integer, is_number);
    let null = c.ins().iconst(types::I64, 0);
    let nonnull = c.ins().icmp(IntCC::NotEqual, pointer, null);
    let valid_wide = c.ins().band(is_wide, nonnull);
    c.ins().brif(valid_wide, wide, &[], other, &[]);
    c.goto_bottom(wide);
    let bits = c.ins().load(types::I64, MemFlagsData::new(), pointer, 0);
    c.ins().jump(after, &[bits.into()]);
    c.goto_bottom(other);
    let boolean_tag = c.ins().iconst(types::I64, abi::BOOLEAN as i64);
    let is_boolean = c.ins().icmp(IntCC::Equal, tag, boolean_tag);
    let valid_boolean = c.ins().band(is_boolean, nonnull);
    c.ins().brif(valid_boolean, boolean, &[], zero, &[]);
    c.goto_bottom(boolean);
    let byte = c.ins().load(types::I8, MemFlagsData::new(), pointer, 0);
    let bits = c.ins().uextend(types::I64, byte);
    c.ins().jump(after, &[bits.into()]);
    c.goto_bottom(zero);
    c.ins().jump(after, &[null.into()]);
    Read {
        blocks: [before, after, wide, other, boolean, zero],
        result,
        index,
    }
}

fn value(
    f: &Function,
    inst: Inst,
    opcode: Opcode,
    args: &[IrValue],
    ty: cranelift_codegen::ir::Type,
) -> Result<IrValue, JitError> {
    if f.dfg.insts[inst].opcode() != opcode
        || f.dfg.inst_args(inst) != args
        || f.dfg.inst_results(inst).len() != 1
    {
        return Err(invalid());
    }
    let result = f.dfg.first_result(inst);
    if f.dfg.value_type(result) != ty {
        return Err(invalid());
    }
    Ok(result)
}

fn load(
    f: &Function,
    inst: Inst,
    base: IrValue,
    offset: i32,
    ty: cranelift_codegen::ir::Type,
) -> Result<IrValue, JitError> {
    let result = value(f, inst, Opcode::Load, &[base], ty)?;
    let InstructionData::Load {
        flags,
        offset: actual,
        ..
    } = f.dfg.insts[inst]
    else {
        return Err(invalid());
    };
    if i32::from(actual) != offset || f.dfg.mem_flags[flags] != MemFlagsData::new() {
        return Err(invalid());
    }
    Ok(result)
}

fn constant(f: &Function, inst: Inst, expected: i64) -> Result<IrValue, JitError> {
    let result = value(f, inst, Opcode::Iconst, &[], types::I64)?;
    let InstructionData::UnaryImm { imm, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if i64::from(imm) != expected {
        return Err(invalid());
    }
    Ok(result)
}

fn compare(f: &Function, inst: Inst, cc: IntCC, args: [IrValue; 2]) -> Result<IrValue, JitError> {
    let result = value(f, inst, Opcode::Icmp, &args, types::I8)?;
    let InstructionData::IntCompare { cond, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if cond != cc {
        return Err(invalid());
    }
    Ok(result)
}

fn branch(
    f: &Function,
    inst: Inst,
    condition: IrValue,
    yes: Block,
    no: Block,
) -> Result<(), JitError> {
    let InstructionData::Brif { arg, blocks, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if arg != condition
        || blocks[0].block(&f.dfg.value_lists) != yes
        || blocks[1].block(&f.dfg.value_lists) != no
        || blocks
            .iter()
            .any(|b| b.args(&f.dfg.value_lists).next().is_some())
    {
        return Err(invalid());
    }
    Ok(())
}

fn jump(f: &Function, inst: Inst, target: Block, arg: IrValue) -> Result<(), JitError> {
    let InstructionData::Jump { destination, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if destination.block(&f.dfg.value_lists) != target
        || !destination
            .args(&f.dfg.value_lists)
            .eq([BlockArg::Value(arg)])
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify(f: &Function, read: &Read, slots: IrValue) -> Result<(), JitError> {
    let [before, after, wide, other, boolean, zero] = read.blocks;
    if f.dfg.block_params(after) != [read.result] || f.dfg.value_type(read.result) != types::I64 {
        return Err(invalid());
    }
    for block in [wide, other, boolean, zero] {
        if !f.dfg.block_params(block).is_empty() {
            return Err(invalid());
        }
    }
    let mut head: Vec<_> = f.layout.block_insts(before).rev().take(11).collect();
    head.reverse();
    let [tag, pointer, integer, is_integer, number, is_number, is_wide, null, nonnull, valid, split]: [Inst; 11] = head.try_into().map_err(|_| invalid())?;
    let tag = load(f, tag, slots, read.index as i32 * 16, types::I64)?;
    let pointer = load(
        f,
        pointer,
        slots,
        read.index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
        types::I64,
    )?;
    let integer = constant(f, integer, abi::INTEGER as i64)?;
    let is_integer = compare(f, is_integer, IntCC::Equal, [tag, integer])?;
    let number = constant(f, number, abi::NUMBER as i64)?;
    let is_number = compare(f, is_number, IntCC::Equal, [tag, number])?;
    let is_wide = value(f, is_wide, Opcode::Bor, &[is_integer, is_number], types::I8)?;
    let null = constant(f, null, 0)?;
    let nonnull = compare(f, nonnull, IntCC::NotEqual, [pointer, null])?;
    let valid = value(f, valid, Opcode::Band, &[is_wide, nonnull], types::I8)?;
    branch(f, split, valid, wide, other)?;
    let [load_inst, end]: [Inst; 2] = f
        .layout
        .block_insts(wide)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    let bits = load(f, load_inst, pointer, 0, types::I64)?;
    jump(f, end, after, bits)?;
    let [tag_inst, cmp, valid, split]: [Inst; 4] = f
        .layout
        .block_insts(other)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    let boolean_tag = constant(f, tag_inst, abi::BOOLEAN as i64)?;
    let is_boolean = compare(f, cmp, IntCC::Equal, [tag, boolean_tag])?;
    let valid = value(f, valid, Opcode::Band, &[is_boolean, nonnull], types::I8)?;
    branch(f, split, valid, boolean, zero)?;
    let [load_inst, extend, end]: [Inst; 3] = f
        .layout
        .block_insts(boolean)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    let byte = load(f, load_inst, pointer, 0, types::I8)?;
    let bits = value(f, extend, Opcode::Uextend, &[byte], types::I64)?;
    jump(f, end, after, bits)?;
    let [end]: [Inst; 1] = f
        .layout
        .block_insts(zero)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid())?;
    jump(f, end, after, null)
}

fn fixture() -> (Function, Read, IrValue) {
    let mut function = Function::new();
    function.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    function.signature.params.push(AbiParam::new(types::I64));
    function.signature.returns.push(AbiParam::new(types::I64));
    let mut frontend = FunctionBuilderContext::new();
    let (slots, inst);
    {
        let mut b = FunctionBuilder::new(&mut function, &mut frontend);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        slots = b.block_params(entry)[0];
        let bits = b.ins().load(types::I64, MemFlagsData::new(), slots, 8);
        inst = b.func.dfg.value_def(bits).inst().unwrap();
        b.ins().return_(&[bits]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    let read = emit(&mut function, inst, slots, 0);
    verify(&function, &read, slots).unwrap();
    (function, read, slots)
}

#[test]
fn inline_read_checker_rejects_well_formed_guard_width_and_result_mutations() {
    let (function, read, slots) = fixture();
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&function, &flags).unwrap();
    let before: Vec<_> = function.layout.block_insts(read.blocks[0]).collect();
    let null = function.dfg.first_result(before[7]);
    let integer = function.dfg.first_result(before[2]);
    let mut rejected = 0;
    let mut reject = |changed: Function| {
        cranelift_codegen::verify_function(&changed, &flags).unwrap();
        assert!(verify(&changed, &read, slots).is_err());
        rejected += 1;
    };
    for block in [
        read.blocks[0],
        read.blocks[2],
        read.blocks[3],
        read.blocks[4],
        read.blocks[5],
    ] {
        for inst in function.layout.block_insts(block) {
            let mut changed = function.clone();
            let dfg = &mut changed.dfg;
            match &mut dfg.insts[inst] {
                InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 1).into(),
                InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                InstructionData::IntCompare { cond, .. } => {
                    *cond = if *cond == IntCC::Equal {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    }
                }
                InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bxor,
                InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                InstructionData::Unary { opcode, .. } => *opcode = Opcode::Sextend,
                InstructionData::Jump { destination, .. } => {
                    destination.update_args(&mut dfg.value_lists, |_| {
                        BlockArg::Value(if block == read.blocks[5] {
                            integer
                        } else {
                            null
                        })
                    })
                }
                _ => panic!("unhandled read instruction"),
            }
            reject(changed);
        }
    }
    let boolean: Vec<_> = function.layout.block_insts(read.blocks[4]).collect();
    let mut changed = function.clone();
    let byte = changed.dfg.first_result(boolean[0]);
    let wide = changed.dfg.replace_result(byte, types::I64);
    changed.replace(boolean[1]).iadd(wide, null);
    reject(changed);
    let mut changed = function.clone();
    let end = changed.layout.last_inst(read.blocks[2]).unwrap();
    let mut cursor = FuncCursor::new(&mut changed);
    cursor.goto_inst(end);
    cursor.ins().store(MemFlagsData::new(), null, slots, 0);
    reject(changed);
    assert_eq!(rejected, 23);
}

#[cfg(not(miri))]
#[test]
fn generated_inline_read_matches_scalar_bits_and_never_dereferences_references() {
    let (function, read, slots) = fixture();
    verify(&function, &read, slots).unwrap();
    assert!(!function
        .layout
        .blocks()
        .any(
            |block| function.layout.block_insts(block).any(|inst| matches!(
                function.dfg.insts[inst].opcode(),
                Opcode::Call | Opcode::CallIndirect
            ))
        ));
    crate::jit::backend::projection_probe::with_projection_probe(function, |pointer| {
        let entry = unsafe {
            std::mem::transmute::<
                *const u8,
                unsafe extern "C" fn(*const crate::jit::abi::payload::Payload) -> u64,
            >(pointer)
        };
        crate::jit::abi::payload::check_native_read(entry);
    })
    .unwrap();
}
