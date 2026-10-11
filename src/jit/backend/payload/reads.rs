use super::*;

#[cfg(test)]
#[path = "bound_scalar_read_tests.rs"]
mod bound_tests;

const _: () = assert!(abi::INTEGER == 2 && abi::NUMBER == 3);

pub(super) fn emit_wide(c: &mut FuncCursor<'_>, tag: IrValue) -> IrValue {
    let mask = c.ins().iconst(types::I64, -2);
    let masked = c.ins().band(tag, mask);
    let integer = c.ins().iconst(types::I64, abi::INTEGER as i64);
    c.ins().icmp(IntCC::Equal, masked, integer)
}

pub(super) fn verify_wide(
    f: &Function,
    instructions: [Inst; 4],
    tag: IrValue,
) -> Result<IrValue, JitError> {
    let [mask, masked, integer, predicate] = instructions;
    let mask = constant(f, mask, -2)?;
    let masked = value(f, masked, Opcode::Band, &[tag, mask], types::I64)?;
    let integer = constant(f, integer, abi::INTEGER as i64)?;
    compare(f, predicate, IntCC::Equal, [masked, integer])
}

pub(super) struct Read {
    contract: Contract,
    blocks: [Block; 6],
    result: IrValue,
    index: u32,
    cast: Option<(Inst, IrValue)>,
}

pub(super) fn block(function: &mut Function) -> Block {
    let block = function.dfg.make_block();
    function.layout.append_block(block);
    block
}

#[cfg(test)]
pub(super) fn emit(function: &mut Function, inst: Inst, slots: IrValue, index: u32) -> Read {
    emit_contract(function, inst, slots, index, Contract::Checked)
}

pub(super) fn emit_contract(
    function: &mut Function,
    inst: Inst,
    slots: IrValue,
    index: u32,
    contract: Contract,
) -> Read {
    let before = function.layout.inst_block(inst).unwrap();
    let after = function.dfg.make_block();
    function.layout.split_block(after, inst);
    let result = function.dfg.first_result(inst);
    let (result, cast) = if function.dfg.value_type(result) == types::F64 {
        let parameter = function.dfg.append_block_param(after, types::I64);
        function
            .replace(inst)
            .bitcast(types::F64, MemFlagsData::new(), parameter);
        (parameter, Some((inst, result)))
    } else {
        function.dfg.detach_inst_results(inst);
        function.dfg.attach_block_param(after, result);
        function.layout.remove_inst(inst);
        (result, None)
    };
    let wide = block(function);
    let other = block(function);
    let boolean = block(function);
    let zero = block(function);
    let mut c = FuncCursor::new(function);
    c.goto_bottom(before);
    let offset = index as i32 * 16;
    let tag = c.ins().load(types::I64, MemFlagsData::new(), slots, offset);
    let pointer_offset = offset + crate::jit::abi::payload::POINTER_OFFSET as i32;
    let pointer = (contract == Contract::Checked).then(|| {
        c.ins()
            .load(types::I64, MemFlagsData::new(), slots, pointer_offset)
    });
    let is_wide = emit_wide(&mut c, tag);
    let null = pointer.map(|_| c.ins().iconst(types::I64, 0));
    let nonnull = pointer.map(|pointer| c.ins().icmp(IntCC::NotEqual, pointer, null.unwrap()));
    let valid_wide = nonnull.map_or(is_wide, |nonnull| c.ins().band(is_wide, nonnull));
    c.ins().brif(valid_wide, wide, &[], other, &[]);
    c.goto_bottom(wide);
    let wide_pointer = pointer.unwrap_or_else(|| {
        c.ins()
            .load(types::I64, MemFlagsData::new(), slots, pointer_offset)
    });
    let bits = c
        .ins()
        .load(types::I64, MemFlagsData::new(), wide_pointer, 0);
    c.ins().jump(after, &[bits.into()]);
    c.goto_bottom(other);
    let boolean_tag = c.ins().iconst(types::I64, abi::BOOLEAN as i64);
    let is_boolean = c.ins().icmp(IntCC::Equal, tag, boolean_tag);
    let valid_boolean = nonnull.map_or(is_boolean, |nonnull| c.ins().band(is_boolean, nonnull));
    c.ins().brif(valid_boolean, boolean, &[], zero, &[]);
    c.goto_bottom(boolean);
    let boolean_pointer = pointer.unwrap_or_else(|| {
        c.ins()
            .load(types::I64, MemFlagsData::new(), slots, pointer_offset)
    });
    let byte = c
        .ins()
        .load(types::I8, MemFlagsData::new(), boolean_pointer, 0);
    let bits = c.ins().uextend(types::I64, byte);
    c.ins().jump(after, &[bits.into()]);
    c.goto_bottom(zero);
    let null = null.unwrap_or_else(|| c.ins().iconst(types::I64, 0));
    c.ins().jump(after, &[null.into()]);
    Read {
        contract,
        blocks: [before, after, wide, other, boolean, zero],
        result,
        index,
        cast,
    }
}

pub(super) fn bitcast(
    f: &Function,
    inst: Inst,
    input: IrValue,
    ty: cranelift_codegen::ir::Type,
) -> Result<IrValue, JitError> {
    let result = value(f, inst, Opcode::Bitcast, &[input], ty)?;
    let InstructionData::LoadNoOffset { flags, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if f.dfg.mem_flags[flags] != MemFlagsData::new() {
        return Err(invalid());
    }
    Ok(result)
}

pub(super) fn value(
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

pub(super) fn load(
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

pub(super) fn constant(f: &Function, inst: Inst, expected: i64) -> Result<IrValue, JitError> {
    let result = value(f, inst, Opcode::Iconst, &[], types::I64)?;
    let InstructionData::UnaryImm { imm, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if i64::from(imm) != expected {
        return Err(invalid());
    }
    Ok(result)
}

pub(super) fn compare(
    f: &Function,
    inst: Inst,
    cc: IntCC,
    args: [IrValue; 2],
) -> Result<IrValue, JitError> {
    let result = value(f, inst, Opcode::Icmp, &args, types::I8)?;
    let InstructionData::IntCompare { cond, .. } = f.dfg.insts[inst] else {
        return Err(invalid());
    };
    if cond != cc {
        return Err(invalid());
    }
    Ok(result)
}

pub(super) fn branch(
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

pub(super) fn jump(f: &Function, inst: Inst, target: Block, arg: IrValue) -> Result<(), JitError> {
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

#[cfg(test)]
pub(super) fn verify(f: &Function, read: &Read, slots: IrValue) -> Result<(), JitError> {
    verify_contract(f, read, slots, Contract::Checked, None)
}

pub(super) fn verify_contract(
    f: &Function,
    read: &Read,
    slots: IrValue,
    contract: Contract,
    predecessors: Option<&crate::jit::preds::Predecessors>,
) -> Result<(), JitError> {
    let [before, after, wide, other, boolean, zero] = read.blocks;
    if read.contract != contract
        || f.dfg.block_params(after) != [read.result]
        || f.dfg.value_type(read.result) != types::I64
    {
        return Err(invalid());
    }
    if let Some((inst, result)) = read.cast {
        if f.layout.first_inst(after) != Some(inst)
            || bitcast(f, inst, read.result, types::F64)? != result
        {
            return Err(invalid());
        }
    }
    for block in [wide, other, boolean, zero] {
        if !f.dfg.block_params(block).is_empty() {
            return Err(invalid());
        }
    }
    if contract == Contract::FrameBound {
        verify_bound_body(f, read, slots)?;
        return verify_bound_edges(f, read, predecessors.ok_or_else(invalid)?);
    }
    let mut head: [Inst; 10] = shape(f.layout.block_insts(before).rev().take(10))?;
    head.reverse();
    let [tag, pointer, mask, masked, integer, is_wide, null, nonnull, valid, split] = head;
    let tag = load(f, tag, slots, read.index as i32 * 16, types::I64)?;
    let pointer = load(
        f,
        pointer,
        slots,
        read.index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
        types::I64,
    )?;
    let is_wide = verify_wide(f, [mask, masked, integer, is_wide], tag)?;
    let null = constant(f, null, 0)?;
    let nonnull = compare(f, nonnull, IntCC::NotEqual, [pointer, null])?;
    let valid = value(f, valid, Opcode::Band, &[is_wide, nonnull], types::I8)?;
    branch(f, split, valid, wide, other)?;
    let [load_inst, end] = shape(f.layout.block_insts(wide))?;
    let bits = load(f, load_inst, pointer, 0, types::I64)?;
    jump(f, end, after, bits)?;
    let [tag_inst, cmp, valid, split] = shape(f.layout.block_insts(other))?;
    let boolean_tag = constant(f, tag_inst, abi::BOOLEAN as i64)?;
    let is_boolean = compare(f, cmp, IntCC::Equal, [tag, boolean_tag])?;
    let valid = value(f, valid, Opcode::Band, &[is_boolean, nonnull], types::I8)?;
    branch(f, split, valid, boolean, zero)?;
    let [load_inst, extend, end] = shape(f.layout.block_insts(boolean))?;
    let byte = load(f, load_inst, pointer, 0, types::I8)?;
    let bits = value(f, extend, Opcode::Uextend, &[byte], types::I64)?;
    jump(f, end, after, bits)?;
    let [end] = shape(f.layout.block_insts(zero))?;
    jump(f, end, after, null)?;
    Ok(())
}

fn verify_bound_body(f: &Function, read: &Read, slots: IrValue) -> Result<(), JitError> {
    let [before, after, wide, other, boolean, zero] = read.blocks;
    let mut head: [Inst; 6] = shape(f.layout.block_insts(before).rev().take(6))?;
    head.reverse();
    let [tag, mask, masked, integer, is_wide, split] = head;
    let tag = load(f, tag, slots, read.index as i32 * 16, types::I64)?;
    let is_wide = verify_wide(f, [mask, masked, integer, is_wide], tag)?;
    branch(f, split, is_wide, wide, other)?;
    let pointer_offset = read.index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32;
    let [pointer, bits, end] = shape(f.layout.block_insts(wide))?;
    let pointer = load(f, pointer, slots, pointer_offset, types::I64)?;
    let bits = load(f, bits, pointer, 0, types::I64)?;
    jump(f, end, after, bits)?;
    let [boolean_tag, is_boolean, split] = shape(f.layout.block_insts(other))?;
    let boolean_tag = constant(f, boolean_tag, abi::BOOLEAN as i64)?;
    let is_boolean = compare(f, is_boolean, IntCC::Equal, [tag, boolean_tag])?;
    branch(f, split, is_boolean, boolean, zero)?;
    let [pointer, byte, bits, end] = shape(f.layout.block_insts(boolean))?;
    let pointer = load(f, pointer, slots, pointer_offset, types::I64)?;
    let byte = load(f, byte, pointer, 0, types::I8)?;
    let bits = value(f, bits, Opcode::Uextend, &[byte], types::I64)?;
    jump(f, end, after, bits)?;
    let [null, end] = shape(f.layout.block_insts(zero))?;
    let null = constant(f, null, 0)?;
    jump(f, end, after, null)
}

fn verify_bound_edges(
    f: &Function,
    read: &Read,
    predecessors: &crate::jit::preds::Predecessors,
) -> Result<(), JitError> {
    let [before, after, wide, other, boolean, zero] = read.blocks;
    for (index, block) in read.blocks.iter().enumerate() {
        if read.blocks[..index].contains(block) {
            return Err(invalid());
        }
    }
    for (block, parent) in [
        (wide, before),
        (other, before),
        (boolean, other),
        (zero, other),
    ] {
        let mut incoming = predecessors.pred_iter(block);
        if incoming.next().map(|p| p.inst) != f.layout.last_inst(parent)
            || incoming.next().is_some()
            || f.layout.entry_block() == Some(block)
        {
            return Err(invalid());
        }
    }
    let parents = [wide, boolean, zero].map(|block| f.layout.last_inst(block).unwrap());
    let mut count = 0;
    for incoming in predecessors.pred_iter(after) {
        if !parents.contains(&incoming.inst) {
            return Err(invalid());
        }
        count += 1;
    }
    if count != 3 || f.layout.entry_block() == Some(after) {
        return Err(invalid());
    }
    Ok(())
}

fn fixture() -> (Function, Read, IrValue) {
    typed_fixture(types::I64)
}

fn typed_fixture(ty: cranelift_codegen::ir::Type) -> (Function, Read, IrValue) {
    let (mut function, inst, slots) = typed_source_fixture(ty);
    let read = emit(&mut function, inst, slots, 0);
    verify(&function, &read, slots).unwrap();
    (function, read, slots)
}

#[cfg(test)]
fn typed_source_fixture(ty: cranelift_codegen::ir::Type) -> (Function, Inst, IrValue) {
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
        let bits = b.ins().load(ty, MemFlagsData::new(), slots, 8);
        inst = b.func.dfg.value_def(bits).inst().unwrap();
        let bits = if ty == types::F64 {
            b.ins().bitcast(types::I64, MemFlagsData::new(), bits)
        } else {
            bits
        };
        b.ins().return_(&[bits]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    (function, inst, slots)
}

#[test]
fn float_read_checker_requires_bit_preserving_conversion() {
    let (function, read, slots) = typed_fixture(types::F64);
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&function, &flags).unwrap();
    let (cast, _) = read.cast.unwrap();
    let mut changed = function.clone();
    changed
        .replace(cast)
        .fcvt_from_sint(types::F64, read.result);
    cranelift_codegen::verify_function(&changed, &flags).unwrap();
    assert!(verify(&changed, &read, slots).is_err());
    let mut changed = function.clone();
    changed.dfg.inst_args_mut(cast)[0] = slots;
    cranelift_codegen::verify_function(&changed, &flags).unwrap();
    assert!(verify(&changed, &read, slots).is_err());
}

#[cfg(not(miri))]
#[test]
fn generated_float_read_preserves_payload_bits() {
    let (function, read, slots) = typed_fixture(types::F64);
    verify(&function, &read, slots).unwrap();
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

#[test]
fn inline_read_checker_rejects_well_formed_guard_width_and_result_mutations() {
    let (function, read, slots) = fixture();
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&function, &flags).unwrap();
    let before: Vec<_> = function.layout.block_insts(read.blocks[0]).collect();
    let null = function.dfg.first_result(before[6]);
    let integer = function.dfg.first_result(before[4]);
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
    let mut changed = function.clone();
    changed.replace(before[2]).iconst(types::I64, -6);
    reject(changed);
    let mut changed = function.clone();
    changed.dfg.inst_args_mut(before[3])[0] = slots;
    reject(changed);
    assert_eq!(rejected, 24);
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
