use super::*;

fn fixture(tag: u64, clobber: bool) -> (Function, IrValue, [Read; 2]) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.extend([AbiParam::new(types::I64); 2]);
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut context);
    let entry = b.create_block();
    let accepted = b.create_block();
    let rejected = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let slots = b.block_params(entry)[0];
    let helper = b.block_params(entry)[1];
    let actual = b.ins().load(types::I64, MemFlagsData::new(), slots, 16);
    let tag_inst = b.func.dfg.value_def(actual).inst().unwrap();
    let valid = b.ins().icmp_imm_s(IntCC::Equal, actual, tag as i64);
    b.ins().brif(valid, accepted, &[], rejected, &[]);
    b.switch_to_block(accepted);
    if clobber {
        let mut signature =
            cranelift_codegen::ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
        signature.params.extend([AbiParam::new(types::I64); 2]);
        let signature = b.import_signature(signature);
        let host = b.ins().iconst(types::I64, 0);
        b.ins().call_indirect(signature, helper, &[host, slots]);
    }
    let pointer = b.ins().load(types::I64, MemFlagsData::new(), slots, 24);
    let ptr_inst = b.func.dfg.value_def(pointer).inst().unwrap();
    b.ins().return_(&[pointer]);
    b.switch_to_block(rejected);
    let null = b.ins().iconst(types::I64, 0);
    b.ins().return_(&[null]);
    b.seal_all_blocks();
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    b.finalize(isa.frontend_config());
    (
        f,
        slots,
        [
            Read {
                inst: tag_inst,
                index: 1,
                pointer: false,
                result: actual,
                uniform: None,
                numeric: false,
            },
            Read {
                inst: ptr_inst,
                index: 1,
                pointer: true,
                result: pointer,
                uniform: Header::uniform_offset(),
                numeric: !clobber
                    && matches!(tag, abi::INTEGER | abi::NUMBER)
                    && Header::uniform_offset().is_some(),
            },
        ],
    )
}

fn memory(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn expansion(f: &Function) -> crate::jit::work::Expansion {
    crate::jit::work::Expansion {
        instructions: size(f).0 + 32,
        blocks: size(f).1,
    }
}

#[test]
fn strided_numeric_witness_preserves_guards_and_invalidates_on_helpers() {
    for tag in [
        abi::NIL,
        abi::BOOLEAN,
        abi::INTEGER,
        abi::NUMBER,
        abi::REFERENCE,
    ] {
        for clobber in [false, true] {
            let (source, slots, records) = fixture(tag, clobber);
            let quota = memory(1 << 20);
            let mut candidate = source.clone();
            lower(&mut candidate, slots, 2, quota.clone(), expansion(&source)).unwrap();
            let mut analysis = same_tag::Analysis::new(&source, quota.clone()).unwrap();
            guarded::verify_plan(&source, &candidate, slots, &records, &mut analysis).unwrap();
            assert_eq!(
                size(&candidate).0,
                size(&source).0 + records.iter().map(|r| r.extra()).sum::<usize>()
            );
            assert_eq!(
                candidate.dfg.insts[records[1].inst].opcode(),
                if records[1].numeric {
                    Opcode::Iadd
                } else {
                    Opcode::Select
                }
            );
            cranelift_codegen::verify_function(
                &candidate,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
            drop(analysis);
            assert_eq!(quota.0.current(), 0);
        }
    }
}

#[test]
fn strided_numeric_checker_rejects_forged_proofs_and_changed_source_control() {
    if Header::uniform_offset().is_none() {
        return;
    }
    let (source, slots, records) = fixture(abi::INTEGER, false);
    let mut candidate = source.clone();
    lower(
        &mut candidate,
        slots,
        2,
        memory(1 << 20),
        expansion(&source),
    )
    .unwrap();
    for inst in candidate
        .layout
        .blocks()
        .flat_map(|b| candidate.layout.block_insts(b))
    {
        let mut changed = candidate.clone();
        match &mut changed.dfg.insts[inst] {
            InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 1).into(),
            InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
            InstructionData::IntCompare { cond, .. } => *cond = IntCC::NotEqual,
            InstructionData::Unary { opcode, .. } => *opcode = Opcode::Sextend,
            InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bxor,
            InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
            _ => continue,
        }
        let mut analysis = same_tag::Analysis::new(&source, memory(1 << 20)).unwrap();
        assert!(guarded::verify_plan(&source, &changed, slots, &records, &mut analysis).is_err());
    }
    for (tag, clobber) in [
        (abi::REFERENCE, false),
        (abi::NIL, false),
        (abi::INTEGER, true),
    ] {
        let (source, slots, mut records) = fixture(tag, clobber);
        records[1].numeric = true;
        let mut forged = source.clone();
        for &read in &records {
            emit(&mut forged, slots, read);
        }
        let mut analysis = same_tag::Analysis::new(&source, memory(1 << 20)).unwrap();
        assert!(guarded::verify_plan(&source, &forged, slots, &records, &mut analysis).is_err());
    }
}

#[test]
fn strided_numeric_analysis_quota_refusal_keeps_source_unchanged() {
    let (source, slots, _) = fixture(abi::NUMBER, false);
    let quota = memory(1 << 20);
    let mut candidate = source.clone();
    lower(&mut candidate, slots, 2, quota.clone(), expansion(&source)).unwrap();
    let peak = quota.0.peak();
    assert_eq!(quota.0.current(), 0);
    for limit in [0, peak - 1, peak] {
        let quota = memory(limit);
        let mut candidate = source.clone();
        let result = lower(&mut candidate, slots, 2, quota.clone(), expansion(&source));
        assert_eq!(result.is_ok(), limit == peak);
        if result.is_err() {
            assert_eq!(candidate, source);
        }
        assert_eq!(quota.0.current(), 0);
    }
}
