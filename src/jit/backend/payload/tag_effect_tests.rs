use super::*;

fn memory(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn finish(builder: FunctionBuilder<'_>) {
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    builder.finalize(isa.frontend_config());
}

fn store_fixture(
    ty: cranelift_codegen::ir::Type,
    offset: i32,
    field: Option<i32>,
) -> (Function, IrValue, IrValue, Inst) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64, types::I64, ty].map(AbiParam::new));
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut context);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let [slots, other, bits]: [_; 3] = b.block_params(entry).try_into().unwrap();
    let address = field.map_or(slots, |offset| {
        b.ins().load(types::I64, MemFlagsData::new(), slots, offset)
    });
    let store = b.ins().store(MemFlagsData::new(), bits, address, offset);
    b.ins().return_(&[]);
    b.seal_all_blocks();
    finish(b);
    (f, slots, other, store)
}

#[test]
fn tag_source_store_effects_require_registered_disjoint_eight_byte_fields() {
    for ty in [
        types::I8,
        types::I32,
        types::I64,
        types::I128,
        types::F32,
        types::F64,
    ] {
        for offset in [-8, 0, 7, 8, 15, 16, 17, 24, 4088, 4095, 4096] {
            let (f, slots, other, store) = store_fixture(ty, offset, None);
            let mut records = vec![false; f.dfg.num_insts()];
            assert!(!Effects::SourceSlots.preserves(&f, store, slots, 1, Some(&records)));
            records[store.as_u32() as usize] = true;
            let valid = matches!(ty, types::I64 | types::F64)
                && (0..4096).contains(&offset)
                && offset % 8 == 0
                && offset != 16;
            assert_eq!(
                Effects::SourceSlots.preserves(&f, store, slots, 1, Some(&records)),
                valid
            );
            assert!(!Effects::SourceSlots.preserves(&f, store, other, 1, Some(&records)));
            assert!(!Effects::SourceSlots.preserves(&f, store, slots, 256, Some(&records)));
            assert!(!Effects::Opaque.preserves(&f, store, slots, 1, Some(&records)));
            let InstructionData::Store { args, offset, .. } = f.dfg.insts[store] else {
                unreachable!()
            };
            let mut changed = f.clone();
            let mut flags = MemFlagsData::new();
            flags.set_notrap();
            changed
                .replace(store)
                .store(flags, args[0], args[1], offset);
            assert!(!Effects::SourceSlots.preserves(&changed, store, slots, 1, Some(&records)));
        }
    }
}

#[test]
fn bound_tag_metadata_effects_require_adjacent_descriptor_pointer_stores() {
    for ty in [
        types::I8,
        types::I32,
        types::I64,
        types::I128,
        types::F32,
        types::F64,
    ] {
        for field in [-8, 0, 7, 8, 16, 24, 4088, 4096] {
            for offset in [0, 1, 8] {
                let (f, slots, _, store) = store_fixture(ty, offset, Some(field));
                let valid = matches!(ty, types::I64 | types::F64)
                    && (8..4096).contains(&field)
                    && field % 16 == 8
                    && offset == 0;
                assert_eq!(
                    Effects::BoundMetadata.preserves(&f, store, slots, 0, None),
                    valid
                );
                assert!(!Effects::Opaque.preserves(&f, store, slots, 0, None));
            }
        }
    }
    let (source, slots, other, store) = store_fixture(types::I64, 0, Some(8));
    let pointer = source.layout.prev_inst(store).unwrap();
    for fault in 0..6 {
        let mut f = source.clone();
        let InstructionData::Store { args, .. } = f.dfg.insts[store] else {
            unreachable!()
        };
        match fault {
            0 => {
                f.replace(store)
                    .store(MemFlagsData::new(), args[0], other, 0);
            }
            1 => {
                f.replace(pointer)
                    .load(types::I64, MemFlagsData::new(), other, 8);
            }
            2 | 3 => {
                let mut flags = MemFlagsData::new();
                flags.set_notrap();
                if fault == 2 {
                    f.replace(pointer).load(types::I64, flags, slots, 8);
                } else {
                    f.replace(store).store(flags, args[0], args[1], 0);
                }
            }
            4 => {
                let mut c = FuncCursor::new(&mut f);
                c.goto_inst(store);
                c.ins().iconst(types::I64, 0);
            }
            _ => {
                let convention = f.signature.call_conv;
                let signature = f.import_signature(Signature::new(convention));
                let mut c = FuncCursor::new(&mut f);
                c.goto_inst(store);
                c.ins().call_indirect(signature, other, &[]);
            }
        }
        assert!(
            !Effects::BoundMetadata.preserves(&f, store, slots, 0, None),
            "fault={fault}"
        );
    }
}

fn pair() -> (Function, IrValue, IrValue, IrValue, [Inst; 4], Inst) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.extend([AbiParam::new(types::I64); 3]);
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut context);
    let entry = b.create_block();
    let yes = b.create_block();
    let no = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let [slots, host, bits]: [_; 3] = b.block_params(entry).try_into().unwrap();
    let tag = b.ins().iconst(types::I64, abi::INTEGER as i64);
    let first = b.ins().load(types::I64, MemFlagsData::new(), slots, 0);
    let second = b.ins().load(types::I64, MemFlagsData::new(), slots, 16);
    let first = b.ins().icmp(IntCC::Equal, first, tag);
    let second = b.ins().icmp(IntCC::Equal, second, tag);
    let valid = b.ins().band(first, second);
    b.ins().brif(valid, yes, &[], no, &[]);
    b.switch_to_block(yes);
    let old = b.ins().load(types::I64, MemFlagsData::new(), slots, 8);
    let old = b.func.dfg.value_def(old).inst().unwrap();
    let stores = [
        b.ins().store(MemFlagsData::new(), tag, slots, 0),
        b.ins().store(MemFlagsData::new(), bits, slots, 8),
        b.ins().store(MemFlagsData::new(), tag, slots, 16),
        b.ins().store(MemFlagsData::new(), bits, slots, 24),
    ];
    b.ins().return_(&[]);
    b.switch_to_block(no);
    b.ins().return_(&[]);
    b.seal_all_blocks();
    finish(b);
    (f, slots, host, tag, stores, old)
}

#[test]
fn registered_source_effects_preserve_tags_but_not_copied_payloads() {
    let (f, slots, host, tag, stores, old) = pair();
    let mut analysis = same_tag::Analysis::with_source_stores(&f, memory(1 << 20)).unwrap();
    assert!(analysis.admit(&f, slots, stores[2], 1, tag).is_none());
    analysis.record_bound_write(stores[0], stores[1]).unwrap();
    let proof = analysis.admit(&f, slots, stores[2], 1, tag).unwrap();
    assert!(analysis.verify(&f, slots, proof));
    assert!(!analysis.fresh_before(&f, old, stores[2], false));
    assert!(!analysis.fresh_before(&f, old, stores[2], true));
    for fault in 0..3 {
        let mut changed = f.clone();
        match fault {
            0 => {
                changed
                    .replace(stores[0])
                    .store(MemFlagsData::new(), tag, slots, 16);
            }
            1 => {
                changed
                    .replace(stores[1])
                    .store(MemFlagsData::new(), tag, host, 8);
            }
            _ => {
                let convention = changed.signature.call_conv;
                let signature = changed.import_signature(Signature::new(convention));
                let mut c = FuncCursor::new(&mut changed);
                c.goto_inst(stores[2]);
                c.ins().call_indirect(signature, host, &[]);
            }
        }
        let mut check = same_tag::Analysis::with_source_stores(&changed, memory(1 << 20)).unwrap();
        check.record_bound_write(stores[0], stores[1]).unwrap();
        assert!(check.admit(&changed, slots, stores[2], 1, tag).is_none());
        assert!(!check.verify(&changed, slots, proof));
    }
}

#[test]
fn bound_store_composition_verifies_block_start_queries_and_atomic_quota() {
    let (source, slots, host, _, _, _) = pair();
    let lower = |f: &mut Function, allocator| {
        lower_contract(
            f,
            slots,
            host,
            2,
            &[],
            allocator,
            crate::jit::work::Expansion {
                instructions: 4096,
                blocks: 4096,
            },
            Contract::FrameBound,
        )
    };
    let quota = memory(1 << 20);
    let mut f = source.clone();
    assert_eq!(lower(&mut f, quota.clone()).unwrap(), 2);
    assert_eq!(quota.0.current(), 0);
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    assert!(!f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .any(|i| f.dfg.insts[i].opcode().is_call()));
    strided::lower(
        &mut f,
        slots,
        2,
        memory(1 << 20),
        crate::jit::work::Expansion {
            instructions: 4096,
            blocks: 4096,
        },
    )
    .unwrap();
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    for limit in [0, quota.0.peak() - 1, quota.0.peak()] {
        let memory = memory(limit);
        let mut f = source.clone();
        let result = lower(&mut f, memory.clone());
        assert_eq!(result.is_ok(), limit == quota.0.peak());
        if result.is_err() {
            assert_eq!(f, source);
        }
        assert_eq!(memory.0.current(), 0);
    }
}
