use super::*;

fn fixture(pointer: bool, index: usize) -> (Function, IrValue, Read) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut context);
    let block = b.create_block();
    b.append_block_params_for_function_params(block);
    b.switch_to_block(block);
    let slots = b.block_params(block)[0];
    let result = b.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        (16 * index + if pointer { 8 } else { 0 }) as i32,
    );
    let inst = b.func.dfg.value_def(result).inst().unwrap();
    b.ins().return_(&[result]);
    b.seal_all_blocks();
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    b.finalize(isa.frontend_config());
    (
        f,
        slots,
        Read {
            inst,
            index,
            pointer,
            result,
        },
    )
}

fn memory(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

#[test]
fn strided_expansion_accounts_nil_fanout_without_changing_limits() {
    crate::Lua::empty().enter(|ctx| {
        let closure =
            crate::Closure::load(ctx, None, b"local a,b,c,d,e,f,g return 'anchored'").unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let limits = crate::jit::work::Limits::from(&crate::jit::JitConfig::default());
        let original = super::super::expansion(&source, limits).unwrap();
        let actual = expansion(&source, limits).unwrap();
        assert_eq!(actual.instructions, original.instructions + 17 * 7);
        assert_eq!(actual.blocks, original.blocks);
        assert!(matches!(
            expansion(
                &source,
                crate::jit::work::Limits {
                    instructions: actual.instructions - 1,
                    ..limits
                }
            ),
            Err(JitError::ResourceLimit("IR instructions"))
        ));
        assert_eq!(
            expansion(
                &source,
                crate::jit::work::Limits {
                    instructions: actual.instructions,
                    ..limits
                }
            )
            .unwrap(),
            actual
        );
    });
}

#[test]
fn strided_lowering_preserves_result_and_checks_quota_atomically() {
    for pointer in [false, true] {
        for index in [0, 6, 63, 64, 255] {
            let (source, slots, read) = fixture(pointer, index);
            let expansion = crate::jit::work::Expansion {
                instructions: 32,
                blocks: 1,
            };
            let quota = memory(1 << 20);
            let mut lowered = source.clone();
            lower(&mut lowered, slots, index + 1, quota.clone(), expansion).unwrap();
            verify(&lowered, slots, read).unwrap();
            cranelift_codegen::verify_function(
                &lowered,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
            assert_eq!(
                size(&lowered),
                (size(&source).0 + if pointer { 16 } else { 1 }, 1)
            );
            assert_eq!(quota.0.current(), 0);
            let peak = quota.0.peak();
            assert!(peak > 0);
            for limit in [0, peak - 1, peak] {
                let quota = memory(limit);
                let mut candidate = source.clone();
                assert_eq!(
                    lower(&mut candidate, slots, index + 1, quota.clone(), expansion).is_ok(),
                    limit == peak
                );
                assert_eq!(quota.0.current(), 0);
                if limit < peak {
                    assert_eq!(candidate, source);
                }
            }
            let mut candidate = source.clone();
            assert!(lower(
                &mut candidate,
                slots,
                index + 1,
                memory(1 << 20),
                crate::jit::work::Expansion {
                    instructions: size(&lowered).0 - 1,
                    blocks: 1
                }
            )
            .is_err());
            assert_eq!(candidate, source);
        }
    }
}

#[test]
fn strided_checker_rejects_changed_addresses_tags_widths_and_guards() {
    for pointer in [false, true] {
        let (mut f, slots, read) = fixture(pointer, 255);
        emit(&mut f, slots, read);
        verify(&f, slots, read).unwrap();
        let block = f.layout.entry_block().unwrap();
        let mut rejected = 0;
        for inst in f
            .layout
            .block_insts(block)
            .filter(|&i| f.dfg.insts[i].opcode() != Opcode::Return)
        {
            let mut changed = f.clone();
            match &mut changed.dfg.insts[inst] {
                InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 1).into(),
                InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bxor,
                InstructionData::Unary { opcode, .. } => *opcode = Opcode::Sextend,
                InstructionData::IntCompare { cond, .. } => *cond = IntCC::Equal,
                InstructionData::Ternary { args, .. } => args.swap(1, 2),
                other => panic!("unhandled instruction {other:?}"),
            }
            assert!(verify(&changed, slots, read).is_err());
            rejected += 1;
        }
        assert_eq!(rejected, if pointer { 17 } else { 2 });
    }
}

#[test]
fn strided_lowering_refuses_non_descriptor_accesses() {
    for fault in 0..5 {
        let (mut f, slots, read) = fixture(true, 0);
        match fault {
            0 => {
                f.replace(read.inst)
                    .load(types::I64, MemFlagsData::new(), slots, 7);
            }
            1 => {
                f.replace(read.inst)
                    .load(types::I64, MemFlagsData::new(), slots, 16);
            }
            2 => {
                let mut flags = MemFlagsData::new();
                flags.set_readonly();
                f.replace(read.inst).load(types::I64, flags, slots, 8);
            }
            3 => {
                f.replace(read.inst).iadd(slots, slots);
            }
            _ => {
                f.replace(read.inst)
                    .bitcast(types::F64, MemFlagsData::new(), slots);
            }
        }
        let source = f.clone();
        let quota = memory(1 << 20);
        assert!(lower(
            &mut f,
            slots,
            1,
            quota.clone(),
            crate::jit::work::Expansion {
                instructions: 4096,
                blocks: 32
            }
        )
        .is_err());
        assert_eq!(f, source);
        assert_eq!(quota.0.current(), 0);
    }
}
