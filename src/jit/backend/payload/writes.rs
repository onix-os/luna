use super::reads::{
    bitcast, block, branch, compare, constant, emit_wide, load, value, verify_wide,
};
use super::*;

#[derive(Clone, Copy)]
pub(super) struct Input {
    pub slots: IrValue,
    pub host: IrValue,
    pub index: u32,
    pub tag: IrValue,
    pub bits: IrValue,
    pub signature: cranelift_codegen::ir::SigRef,
    pub terminal: bool,
    pub known_tag: Option<same_tag::Proof>,
    pub copy: Option<copy::Proof>,
}

pub(super) struct Write {
    contract: Contract,
    blocks: [Block; 8],
    input: Input,
    cast: Option<Inst>,
}

pub(super) fn copy_site(write: &Write) -> Option<(copy::Proof, Block)> {
    write.input.copy.map(|proof| (proof, write.blocks[7]))
}

pub(super) fn copy_predecessors(f: &Function, write: &Write) -> Option<[Inst; 2]> {
    Some([
        f.layout.last_inst(write.blocks[0])?,
        f.layout.last_inst(write.blocks[6])?,
    ])
}

pub(super) fn emit(f: &mut Function, tag_store: Inst, bits_store: Inst, input: Input) -> Write {
    emit_contract(f, tag_store, bits_store, input, Contract::Checked)
}

pub(super) fn emit_contract(
    f: &mut Function,
    tag_store: Inst,
    bits_store: Inst,
    input: Input,
    contract: Contract,
) -> Write {
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
    if input.known_tag.is_some() {
        f.layout.set_cold(slow);
    }
    let Input {
        slots,
        host,
        index,
        tag,
        bits,
        signature,
        terminal,
        known_tag,
        copy,
    } = input;
    let mut c = FuncCursor::new(f);
    c.goto_bottom(before);
    let (bits, cast) = if c.func.dfg.value_type(bits) == types::F64 {
        let bits = c.ins().bitcast(types::I64, MemFlagsData::new(), bits);
        (bits, c.func.dfg.value_def(bits).inst())
    } else {
        (bits, None)
    };
    let actual = if known_tag.is_some() {
        c.ins().bor(tag, tag)
    } else {
        c.ins()
            .load(types::I64, MemFlagsData::new(), slots, index as i32 * 16)
    };
    let pointer = (contract == Contract::Checked).then(|| load_pointer(&mut c, slots, index));
    let same = c.ins().icmp(IntCC::Equal, actual, tag);
    c.ins().brif(same, kind, &[], slow, &[]);
    c.goto_bottom(kind);
    let guarded_tag = if contract == Contract::FrameBound {
        actual
    } else {
        tag
    };
    let is_wide = emit_wide(&mut c, guarded_tag);
    let null = c.ins().iconst(types::I64, 0);
    let nonnull = pointer.map(|pointer| c.ins().icmp(IntCC::NotEqual, pointer, null));
    let valid = nonnull.map_or(is_wide, |nonnull| c.ins().band(is_wide, nonnull));
    c.ins().brif(valid, wide, &[], other, &[]);
    c.goto_bottom(wide);
    let wide_pointer = pointer.unwrap_or_else(|| load_pointer(&mut c, slots, index));
    c.ins().store(MemFlagsData::new(), bits, wide_pointer, 0);
    c.ins().jump(after, &[]);
    c.goto_bottom(other);
    let boolean_tag = c.ins().iconst(types::I64, abi::BOOLEAN as i64);
    let is_boolean = c.ins().icmp(IntCC::Equal, guarded_tag, boolean_tag);
    let valid_bits = c
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bits, boolean_tag);
    let valid = nonnull.map_or(is_boolean, |nonnull| c.ins().band(is_boolean, nonnull));
    let valid = c.ins().band(valid, valid_bits);
    c.ins().brif(valid, boolean, &[], nil, &[]);
    c.goto_bottom(boolean);
    let boolean_pointer = pointer.unwrap_or_else(|| load_pointer(&mut c, slots, index));
    let byte = c.ins().ireduce(types::I8, bits);
    c.ins().store(MemFlagsData::new(), byte, boolean_pointer, 0);
    c.ins().jump(after, &[]);
    c.goto_bottom(nil);
    let is_nil = c.ins().icmp(IntCC::Equal, tag, null);
    let valid = if let Some(pointer) = pointer {
        let is_null = c.ins().icmp(IntCC::Equal, pointer, null);
        c.ins().band(is_nil, is_null)
    } else {
        is_nil
    };
    c.ins().brif(valid, after, &[], slow, &[]);
    c.goto_bottom(slow);
    let functions = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        host,
        std::mem::offset_of!(Bridge, functions) as i32,
    );
    let target = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        functions,
        if copy.is_some() {
            std::mem::offset_of!(Functions, copy)
        } else if terminal {
            std::mem::offset_of!(Functions, terminal)
        } else {
            std::mem::offset_of!(Functions, write)
        } as i32,
    );
    let index = c.ins().iconst(types::I32, i64::from(index));
    let (tag, bits) = if let Some(copy) = copy {
        (
            c.ins().iconst(types::I64, i64::from(copy.source())),
            c.ins().iconst(types::I64, 0),
        )
    } else {
        (tag, bits)
    };
    c.ins()
        .call_indirect(signature, target, &[host, slots, index, tag, bits]);
    c.ins().jump(after, &[]);
    Write {
        contract,
        blocks: [before, after, kind, wide, other, boolean, nil, slow],
        input,
        cast,
    }
}

fn load_pointer(c: &mut FuncCursor<'_>, slots: IrValue, index: u32) -> IrValue {
    c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
    )
}

pub(super) fn jump(f: &Function, inst: Inst, after: Block) -> Result<(), JitError> {
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

pub(super) fn store(
    f: &Function,
    inst: Inst,
    bits: IrValue,
    pointer: IrValue,
) -> Result<(), JitError> {
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

pub(super) fn verify_source(
    f: &Function,
    write: &Write,
    analysis: &mut same_tag::Analysis,
) -> Result<(), JitError> {
    if let Some(proof) = write.input.known_tag {
        if !proof.matches(write.input.index, write.input.tag)
            || !analysis.verify(f, write.input.slots, proof)
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn verify_guard(
    f: &Function,
    write: &Write,
    predecessors: Option<&crate::jit::preds::Predecessors>,
    remaining: &mut usize,
    contract: Contract,
) -> Result<(), JitError> {
    if write.contract != contract {
        return Err(invalid());
    }
    if write.input.known_tag.is_some() {
        let at = f
            .layout
            .block_insts(write.blocks[0])
            .rev()
            .nth(if contract == Contract::Checked { 3 } else { 2 })
            .ok_or_else(invalid)?;
        let verify = if contract == Contract::FrameBound {
            same_tag::verify_bound_at
        } else {
            same_tag::verify_at
        };
        if !verify(
            f,
            predecessors.ok_or_else(invalid)?,
            write.input.slots,
            at,
            write.input.index,
            write.input.tag,
            remaining,
        ) {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn verify_copy_source(
    f: &Function,
    write: &Write,
    analysis: &mut same_tag::Analysis,
    terminal: &mut terminal::Analysis<'_>,
) -> Result<(), JitError> {
    if let Some(proof) = write.input.copy {
        if !write.input.terminal
            || !proof.matches(write.input.index, write.input.tag, write.input.bits)
            || !copy::verify(f, write.input.slots, proof, analysis, terminal, true)
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn verify(f: &Function, write: &Write) -> Result<(), JitError> {
    verify_contract(f, write, Contract::Checked)
}

pub(super) fn verify_contract(
    f: &Function,
    write: &Write,
    contract: Contract,
) -> Result<(), JitError> {
    if write.contract != contract {
        return Err(invalid());
    }
    let [before, after, kind, wide, other, boolean, nil, slow] = write.blocks;
    let Input {
        slots,
        host,
        index,
        tag,
        bits,
        signature,
        terminal,
        known_tag,
        copy,
    } = write.input;
    if copy.is_some_and(|proof| !terminal || !proof.matches(index, tag, bits)) {
        return Err(invalid());
    }
    if f.layout.is_cold(slow) != known_tag.is_some() {
        return Err(invalid());
    }
    for block in [after, kind, wide, other, boolean, nil, slow] {
        if !f.dfg.block_params(block).is_empty() {
            return Err(invalid());
        }
    }
    let (actual, pointer, same, split) = if contract == Contract::Checked {
        let [split, same, pointer, actual] = shape(f.layout.block_insts(before).rev().take(4))?;
        (actual, Some(pointer), same, split)
    } else {
        let [split, same, actual] = shape(f.layout.block_insts(before).rev().take(3))?;
        (actual, None, same, split)
    };
    let bits = match (f.dfg.value_type(bits), write.cast) {
        (types::I64, None) => bits,
        (types::F64, Some(cast)) if f.layout.prev_inst(actual) == Some(cast) => {
            bitcast(f, cast, bits, types::I64)?
        }
        _ => return Err(invalid()),
    };
    let actual = if let Some(proof) = known_tag {
        if !proof.matches(index, tag) {
            return Err(invalid());
        }
        value(f, actual, Opcode::Bor, &[tag, tag], types::I64)?
    } else {
        load(f, actual, slots, index as i32 * 16, types::I64)?
    };
    let address = |inst| {
        load(
            f,
            inst,
            slots,
            index as i32 * 16 + crate::jit::abi::payload::POINTER_OFFSET as i32,
            types::I64,
        )
    };
    let pointer = pointer.map(address).transpose()?;
    let same = compare(f, same, IntCC::Equal, [actual, tag])?;
    branch(f, split, same, kind, slow)?;
    let (wide_prefix, null_inst, guards, split) = if contract == Contract::Checked {
        let [mask, masked, integer, is_wide, null, nonnull, valid, split] =
            shape(f.layout.block_insts(kind))?;
        (
            [mask, masked, integer, is_wide],
            null,
            Some((nonnull, valid)),
            split,
        )
    } else {
        let [mask, masked, integer, is_wide, null, split] = shape(f.layout.block_insts(kind))?;
        ([mask, masked, integer, is_wide], null, None, split)
    };
    let guarded_tag = if contract == Contract::FrameBound {
        actual
    } else {
        tag
    };
    let is_wide = verify_wide(f, wide_prefix, guarded_tag)?;
    let null = constant(f, null_inst, 0)?;
    let (nonnull, valid) = if let Some((nonnull, valid)) = guards {
        let nonnull = compare(
            f,
            nonnull,
            IntCC::NotEqual,
            [pointer.ok_or_else(invalid)?, null],
        )?;
        (
            Some(nonnull),
            value(f, valid, Opcode::Band, &[is_wide, nonnull], types::I8)?,
        )
    } else {
        (None, is_wide)
    };
    branch(f, split, valid, wide, other)?;
    let (wide_pointer, write_inst, end) = if let Some(pointer) = pointer {
        let [write_inst, end] = shape(f.layout.block_insts(wide))?;
        (pointer, write_inst, end)
    } else {
        let [pointer, write_inst, end] = shape(f.layout.block_insts(wide))?;
        (address(pointer)?, write_inst, end)
    };
    store(f, write_inst, bits, wide_pointer)?;
    jump(f, end, after)?;
    let (boolean_tag, is_boolean, valid_bits, nonnull_boolean, valid, split) =
        if contract == Contract::Checked {
            let [tag, boolean, bits, nonnull, valid, split] = shape(f.layout.block_insts(other))?;
            (tag, boolean, bits, Some(nonnull), valid, split)
        } else {
            let [tag, boolean, bits, valid, split] = shape(f.layout.block_insts(other))?;
            (tag, boolean, bits, None, valid, split)
        };
    let boolean_tag = constant(f, boolean_tag, abi::BOOLEAN as i64)?;
    let is_boolean = compare(f, is_boolean, IntCC::Equal, [guarded_tag, boolean_tag])?;
    let valid_bits = compare(
        f,
        valid_bits,
        IntCC::UnsignedLessThanOrEqual,
        [bits, boolean_tag],
    )?;
    let nonnull_boolean = match (nonnull_boolean, nonnull) {
        (Some(inst), Some(nonnull)) => {
            value(f, inst, Opcode::Band, &[is_boolean, nonnull], types::I8)?
        }
        (None, None) => is_boolean,
        _ => return Err(invalid()),
    };
    let valid = value(
        f,
        valid,
        Opcode::Band,
        &[nonnull_boolean, valid_bits],
        types::I8,
    )?;
    branch(f, split, valid, boolean, nil)?;
    let (boolean_pointer, reduce, write_inst, end) = if let Some(pointer) = pointer {
        let [reduce, write_inst, end] = shape(f.layout.block_insts(boolean))?;
        (pointer, reduce, write_inst, end)
    } else {
        let [pointer, reduce, write_inst, end] = shape(f.layout.block_insts(boolean))?;
        (address(pointer)?, reduce, write_inst, end)
    };
    let byte = value(f, reduce, Opcode::Ireduce, &[bits], types::I8)?;
    store(f, write_inst, byte, boolean_pointer)?;
    jump(f, end, after)?;
    let (is_nil, guards, split) = if contract == Contract::Checked {
        let [is_nil, is_null, valid, split] = shape(f.layout.block_insts(nil))?;
        (is_nil, Some((is_null, valid)), split)
    } else {
        let [is_nil, split] = shape(f.layout.block_insts(nil))?;
        (is_nil, None, split)
    };
    let is_nil = compare(f, is_nil, IntCC::Equal, [tag, null])?;
    let valid = if let Some((is_null, valid)) = guards {
        let is_null = compare(
            f,
            is_null,
            IntCC::Equal,
            [pointer.ok_or_else(invalid)?, null],
        )?;
        value(f, valid, Opcode::Band, &[is_nil, is_null], types::I8)?
    } else {
        is_nil
    };
    branch(f, split, valid, after, slow)?;
    let (functions, target, register, call, end, tag, bits) = if let Some(proof) = copy {
        let [functions, target, register, source, zero, call, end] =
            shape(f.layout.block_insts(slow))?;
        (
            functions,
            target,
            register,
            call,
            end,
            constant(f, source, i64::from(proof.source()))?,
            constant(f, zero, 0)?,
        )
    } else {
        let [functions, target, register, call, end] = shape(f.layout.block_insts(slow))?;
        (functions, target, register, call, end, tag, bits)
    };
    let functions = load(
        f,
        functions,
        host,
        std::mem::offset_of!(Bridge, functions) as i32,
        types::I64,
    )?;
    let target = load(
        f,
        target,
        functions,
        if copy.is_some() {
            std::mem::offset_of!(Functions, copy)
        } else if terminal {
            std::mem::offset_of!(Functions, terminal)
        } else {
            std::mem::offset_of!(Functions, write)
        } as i32,
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
    typed_fixture(false)
}

#[test]
fn terminal_write_checker_binds_the_retiring_gateway() {
    for contract in [Contract::Checked, Contract::FrameBound] {
        check_terminal_gateway(contract);
    }
}

fn check_terminal_gateway(contract: Contract) {
    let (mut function, mut write) = typed_fixture_contract(false, contract);
    let slow = write.blocks[7];
    let table = function.layout.first_inst(slow).unwrap();
    let target = function.layout.next_inst(table).unwrap();
    write.input.terminal = true;
    assert!(verify_contract(&function, &write, contract).is_err());
    let InstructionData::Load { offset, .. } = &mut function.dfg.insts[target] else {
        unreachable!()
    };
    *offset = (std::mem::offset_of!(Functions, terminal) as i32).into();
    cranelift_codegen::verify_function(&function, &settings::Flags::new(settings::builder()))
        .unwrap();
    verify_contract(&function, &write, contract).unwrap();
    write.input.terminal = false;
    assert!(verify_contract(&function, &write, contract).is_err());
}

fn typed_fixture(floating: bool) -> (Function, Write) {
    typed_fixture_contract(floating, Contract::Checked)
}

fn typed_fixture_contract(floating: bool, contract: Contract) -> (Function, Write) {
    let (mut function, tag_store, bits_store, input) = source_fixture(floating);
    let write = emit_contract(&mut function, tag_store, bits_store, input, contract);
    verify_contract(&function, &write, contract).unwrap();
    (function, write)
}

fn source_fixture(floating: bool) -> (Function, Inst, Inst, Input) {
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
    let bits = if floating {
        c.ins().bitcast(types::F64, MemFlagsData::new(), bits)
    } else {
        bits
    };
    let tag_store = c.ins().store(MemFlagsData::new(), tag, slots, 0);
    let bits_store = c.ins().store(MemFlagsData::new(), bits, slots, 8);
    c.ins().return_(&[]);
    (
        function,
        tag_store,
        bits_store,
        Input {
            slots,
            host,
            index: 0,
            tag,
            bits,
            signature,
            terminal: false,
            known_tag: None,
            copy: None,
        },
    )
}

#[test]
fn frame_bound_generic_write_quota_refusal_preserves_source_and_ledger() {
    let flags = settings::Flags::new(settings::builder());
    let expansion = crate::jit::work::Expansion {
        instructions: 4096,
        blocks: 4096,
    };
    for floating in [false, true] {
        let (source, _, _, input) = source_fixture(floating);
        for contract in [Contract::Checked, Contract::FrameBound] {
            let ledger = crate::jit::resources::Ledger::new(1 << 20);
            let mut lowered = source.clone();
            assert_eq!(
                lower_contract(
                    &mut lowered,
                    input.slots,
                    input.host,
                    1,
                    &[],
                    BudgetAllocator(ledger.clone()),
                    expansion,
                    contract,
                )
                .unwrap(),
                0
            );
            assert_eq!(ledger.current(), 0);
            cranelift_codegen::verify_function(&lowered, &flags).unwrap();
            for limit in [0, ledger.peak() - 1, ledger.peak()] {
                let quota = crate::jit::resources::Ledger::new(limit);
                let mut candidate = source.clone();
                let result = lower_contract(
                    &mut candidate,
                    input.slots,
                    input.host,
                    1,
                    &[],
                    BudgetAllocator(quota.clone()),
                    expansion,
                    contract,
                );
                assert_eq!(quota.current(), 0);
                if limit == ledger.peak() {
                    result.unwrap();
                    assert_eq!(candidate, lowered);
                } else {
                    assert!(matches!(result, Err(JitError::ResourceLimit(_))));
                    assert_eq!(candidate, source);
                }
            }
        }
    }
}

#[test]
fn float_write_checker_requires_bit_preserving_conversion() {
    for contract in [Contract::Checked, Contract::FrameBound] {
        let (function, write) = typed_fixture_contract(true, contract);
        let flags = settings::Flags::new(settings::builder());
        cranelift_codegen::verify_function(&function, &flags).unwrap();
        let mut changed = function.clone();
        changed
            .replace(write.cast.unwrap())
            .fcvt_to_sint_sat(types::I64, write.input.bits);
        cranelift_codegen::verify_function(&changed, &flags).unwrap();
        assert!(verify_contract(&changed, &write, contract).is_err());
    }
}

#[cfg(not(miri))]
#[test]
fn generated_float_write_preserves_payload_bits() {
    let (function, write) = typed_fixture(true);
    verify(&function, &write).unwrap();
    crate::jit::backend::projection_probe::with_projection_probe(function, |pointer| {
        let entry = unsafe {
            std::mem::transmute::<
                *const u8,
                unsafe extern "C" fn(*mut Payload, *mut Bridge, u64, u64),
            >(pointer)
        };
        crate::jit::abi::payload::runtime::check_native_write(entry);
        crate::jit::abi::payload::runtime::check_native_write_guards(entry);
    })
    .unwrap();
}

#[test]
fn inline_write_checker_rejects_well_formed_guard_store_and_gateway_mutations() {
    check_write_mutations(Contract::Checked, 39);
}

#[test]
fn frame_bound_write_checker_rejects_guard_store_and_gateway_mutations() {
    check_write_mutations(Contract::FrameBound, 35);
}

fn check_write_mutations(contract: Contract, expected: usize) {
    let (function, write) = typed_fixture_contract(false, contract);
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&function, &flags).unwrap();
    let mut rejected = 0;
    let mut reject = |changed: Function| {
        cranelift_codegen::verify_function(&changed, &flags).unwrap();
        assert!(verify_contract(&changed, &write, contract).is_err());
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
    let store = boolean
        .iter()
        .copied()
        .find(|&i| function.dfg.insts[i].opcode() == Opcode::Store)
        .unwrap();
    changed.dfg.inst_args_mut(store)[0] = write.input.bits;
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
    let call = slow
        .into_iter()
        .find(|&inst| function.dfg.insts[inst].opcode() == Opcode::CallIndirect)
        .unwrap();
    changed.dfg.inst_args_mut(call).swap(4, 5);
    reject(changed);
    let kind: Vec<_> = function.layout.block_insts(write.blocks[2]).collect();
    let mut changed = function.clone();
    changed.replace(kind[0]).iconst(types::I64, -6);
    reject(changed);
    let mut changed = function.clone();
    changed.dfg.inst_args_mut(kind[1])[0] = write.input.bits;
    reject(changed);
    let mut changed = function.clone();
    changed.layout.set_cold(write.blocks[7]);
    reject(changed);
    assert_eq!(rejected, expected);
}

#[test]
fn frame_bound_write_has_exact_savings_and_rejects_contract_substitution() {
    for floating in [false, true] {
        let (checked, checked_write) = typed_fixture_contract(floating, Contract::Checked);
        let (bound, bound_write) = typed_fixture_contract(floating, Contract::FrameBound);
        assert_eq!(size(&checked), (size(&bound).0 + 4, size(&bound).1));
        for (function, mut write, contract, other) in [
            (
                checked,
                checked_write,
                Contract::Checked,
                Contract::FrameBound,
            ),
            (bound, bound_write, Contract::FrameBound, Contract::Checked),
        ] {
            cranelift_codegen::verify_function(
                &function,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
            verify_contract(&function, &write, contract).unwrap();
            assert!(verify_contract(&function, &write, other).is_err());
            assert!(verify_guard(&function, &write, None, &mut 100, other).is_err());
            write.contract = other;
            assert!(verify_contract(&function, &write, other).is_err());
        }
    }
}

#[cfg(not(miri))]
#[test]
fn generated_frame_bound_write_preserves_bits_and_delegates_invalid_values() {
    for floating in [false, true] {
        let (function, write) = typed_fixture_contract(floating, Contract::FrameBound);
        verify_contract(&function, &write, Contract::FrameBound).unwrap();
        crate::jit::backend::projection_probe::with_projection_probe(function, |pointer| {
            let entry = unsafe {
                std::mem::transmute::<
                    *const u8,
                    unsafe extern "C" fn(*mut Payload, *mut Bridge, u64, u64),
                >(pointer)
            };
            crate::jit::abi::payload::runtime::check_native_write(entry);
            crate::jit::abi::payload::runtime::check_native_bound_write_guards(entry);
        })
        .unwrap();
    }
}

#[test]
fn frame_bound_write_exposes_guarded_numeric_pointer_to_strided_lowering() {
    for floating in [false, true] {
        let (function, write) = typed_fixture_contract(floating, Contract::FrameBound);
        let [before, _, kind, wide, _, boolean, nil, _] = write.blocks;
        let pointer = function.layout.first_inst(wide).unwrap();
        let ledger = crate::jit::resources::Ledger::new(1 << 20);
        let allocator = BudgetAllocator(ledger.clone());
        let mut proof =
            same_tag::Analysis::with_bound_stores(&function, allocator.clone()).unwrap();
        assert!(proof.admit_numeric(&function, write.input.slots, pointer, 0));
        assert!(proof.verify_numeric(&function, write.input.slots, pointer, 0));
        for block in [before, kind, nil] {
            assert!(!function.layout.block_insts(block).any(|inst| {
                matches!(function.dfg.insts[inst], InstructionData::Load { offset, .. }
                    if i32::from(offset) == crate::jit::abi::payload::POINTER_OFFSET as i32)
            }));
        }
        let boolean_pointer = function.layout.first_inst(boolean).unwrap();
        assert!(!proof.admit_numeric(&function, write.input.slots, boolean_pointer, 0));
        let mut changed = function.clone();
        let masked = changed.layout.block_insts(kind).nth(1).unwrap();
        changed.dfg.inst_args_mut(masked)[0] = write.input.tag;
        let mut checker =
            same_tag::Analysis::with_bound_stores(&changed, allocator.clone()).unwrap();
        assert!(!checker.verify_numeric(&changed, write.input.slots, pointer, 0));
        assert!(verify_contract(&changed, &write, Contract::FrameBound).is_err());
        drop((checker, proof));
        let mut lowered = function.clone();
        super::strided::lower(
            &mut lowered,
            write.input.slots,
            1,
            allocator,
            crate::jit::work::Expansion {
                instructions: 4096,
                blocks: 4096,
            },
        )
        .unwrap();
        cranelift_codegen::verify_function(&lowered, &settings::Flags::new(settings::builder()))
            .unwrap();
        if crate::jit::abi::strided::frame::Header::uniform_offset().is_some() {
            assert_eq!(lowered.layout.block_insts(wide).count(), 5);
            assert!(!lowered
                .layout
                .block_insts(wide)
                .any(|inst| lowered.dfg.insts[inst].opcode() == Opcode::Select));
        }
        assert_eq!(ledger.current(), 0);
    }
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
        crate::jit::abi::payload::runtime::check_native_write_guards(entry);
    })
    .unwrap();
}
