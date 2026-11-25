use super::*;

fn fixture(
    left_write: bool,
    right_write: bool,
    cycle: bool,
    index: usize,
) -> (Function, IrValue, IrValue, [Block; 4]) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64, types::I64, types::I32].map(AbiParam::new));
    let mut frontend = FunctionBuilderContext::new();
    let (slots, host, headers);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut frontend);
        let entry = b.create_block();
        let left = b.create_block();
        let right = b.create_block();
        let join = b.create_block();
        let done = b.create_block();
        headers = [entry, left, right, join];
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let condition;
        [slots, host, condition] = b.block_params(entry).try_into().unwrap();
        let tag = b.ins().iconst(types::I64, abi::INTEGER as i64);
        let bits = b.ins().iconst(types::I64, 17);
        b.ins().brif(condition, left, &[], right, &[]);
        for (block, write) in [(left, left_write), (right, right_write)] {
            b.switch_to_block(block);
            if write {
                b.ins()
                    .store(MemFlagsData::new(), tag, slots, (index * 16) as i32);
                b.ins()
                    .store(MemFlagsData::new(), bits, slots, (index * 16 + 8) as i32);
            }
            b.ins().jump(join, &[]);
        }
        b.switch_to_block(join);
        b.ins()
            .load(types::I64, MemFlagsData::new(), slots, (index * 16) as i32);
        b.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots,
            (index * 16 + 8) as i32,
        );
        if cycle {
            b.ins().brif(condition, left, &[], done, &[]);
        } else {
            b.ins().jump(done, &[]);
        }
        b.switch_to_block(done);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(
            cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(settings::Flags::new(settings::builder()))
                .unwrap()
                .frontend_config(),
        );
    }
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    (f, slots, host, headers)
}

#[test]
fn entry_binding_masks_preserve_branch_loop_and_definition_paths() {
    for left in [false, true] {
        for right in [false, true] {
            for cycle in [false, true] {
                for index in [0, 63, 64, 127, 128, 255] {
                    let (f, slots, host, headers) = fixture(left, right, cycle, index);
                    let allocator =
                        BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
                    let plan = Requirements::new(
                        &f,
                        slots,
                        host,
                        index + 1,
                        &[],
                        &headers,
                        allocator.clone(),
                    )
                    .unwrap();
                    for (pc, expected) in [!(left && right), !left, !right, true]
                        .into_iter()
                        .enumerate()
                    {
                        let mask = plan.get(pc).unwrap();
                        assert_eq!(mask.contains(index), expected);
                        assert_eq!(
                            (0..256).filter(|&i| mask.contains(i)).count(),
                            usize::from(expected)
                        );
                    }
                    assert!(plan.get(headers.len()).is_none());
                    assert_eq!(
                        allocator.0.current(),
                        headers.len() * std::mem::size_of::<Mask>()
                    );
                    drop(plan);
                    assert_eq!(allocator.0.current(), 0);
                }
            }
        }
    }
}

#[test]
fn entry_binding_masks_cover_dispatch_prefix_reads_and_reject_missing_bits() {
    let (mut f, slots, host, headers) = fixture(true, true, true, 0);
    let first = f.layout.first_inst(headers[0]).unwrap();
    FuncCursor::new(&mut f)
        .at_inst(first)
        .ins()
        .load(types::I64, MemFlagsData::new(), slots, 16);
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
    let source_headers = &headers[1..];
    let mut plan =
        Requirements::new(&f, slots, host, 2, &[], source_headers, allocator.clone()).unwrap();
    for pc in 0..source_headers.len() {
        let mask = plan.entries[pc];
        assert!(mask.contains(1));
        assert_eq!(mask.contains(0), pc == 2);
        plan.entries[pc].0[0] &= !(1 << 1);
        assert!(plan
            .verify(&f, slots, host, 2, &[], source_headers, allocator.clone(),)
            .is_err());
        plan.entries[pc] = mask;
    }
    drop(plan);
    assert_eq!(allocator.0.current(), 0);
}

#[test]
fn entry_binding_prefix_dataflow_preserves_branch_definitions_and_cycles() {
    for left in [false, true] {
        for right in [false, true] {
            for cycle in [false, true] {
                let (f, slots, host, _) = fixture(left, right, cycle, 0);
                let headers = [f.layout.last_block().unwrap()];
                let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
                let mut plan =
                    Requirements::new(&f, slots, host, 1, &[], &headers, allocator.clone())
                        .unwrap();
                assert_eq!(plan.get(0).unwrap().contains(0), !(left && right));
                if !(left && right) {
                    plan.entries[0] = Mask::default();
                    assert!(plan
                        .verify(&f, slots, host, 1, &[], &headers, allocator.clone(),)
                        .is_err());
                }
                drop(plan);
                assert_eq!(allocator.0.current(), 0);
            }
        }
    }
}

#[test]
fn entry_write_binding_is_a_charged_superset_of_read_requirements() {
    for left in [false, true] {
        for right in [false, true] {
            for cycle in [false, true] {
                for index in [0, 63, 64, 127, 128, 255] {
                    let (f, slots, host, headers) = fixture(left, right, cycle, index);
                    let original = f.display().to_string();
                    let allocator =
                        BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
                    let reads = Requirements::new(
                        &f,
                        slots,
                        host,
                        index + 1,
                        &[],
                        &headers,
                        allocator.clone(),
                    )
                    .unwrap();
                    let writes = Requirements::with_writes(
                        &f,
                        slots,
                        host,
                        index + 1,
                        &[],
                        &headers,
                        allocator.clone(),
                    )
                    .unwrap();
                    for pc in 0..headers.len() {
                        let mask = writes.get(pc).unwrap();
                        assert!(mask.contains(index));
                        for bit in 0..256 {
                            assert!(!reads.get(pc).unwrap().contains(bit) || mask.contains(bit));
                            assert_eq!(mask.contains(bit), bit == index);
                        }
                    }
                    assert_eq!(
                        allocator.0.current(),
                        2 * headers.len() * std::mem::size_of::<Mask>()
                    );
                    assert_eq!(f.display().to_string(), original);
                    drop((reads, writes));
                    assert_eq!(allocator.0.current(), 0);
                    allocator.0.set_limit(0);
                    assert!(matches!(
                        Requirements::with_writes(
                            &f,
                            slots,
                            host,
                            index + 1,
                            &[],
                            &headers,
                            allocator.clone()
                        ),
                        Err(CompileError::ResourceLimit(_))
                    ));
                    assert_eq!(allocator.0.current(), 0);
                }
            }
        }
    }
}

#[test]
fn entry_binding_verifier_rejects_missing_uses_source_changes_and_excess_bits() {
    let (f, slots, host, headers) = fixture(true, false, true, 1);
    let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
    let mut plan = Requirements::new(&f, slots, host, 2, &[], &headers, allocator.clone()).unwrap();
    for pc in [0, 2, 3] {
        let saved = plan.entries[pc];
        plan.entries[pc] = Mask::default();
        assert!(plan
            .verify(&f, slots, host, 2, &[], &headers, allocator.clone())
            .is_err());
        plan.entries[pc] = saved;
    }
    plan.entries[0].insert(2);
    assert!(plan
        .verify(&f, slots, host, 2, &[], &headers, allocator.clone())
        .is_err());
    plan.entries[0].0[0] &= !(1 << 2);
    let mut changed = f.clone();
    let first = changed.layout.first_inst(headers[1]).unwrap();
    FuncCursor::new(&mut changed).at_inst(first).ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        16,
    );
    cranelift_codegen::verify_function(&changed, &settings::Flags::new(settings::builder()))
        .unwrap();
    assert!(plan
        .verify(&changed, slots, host, 2, &[], &headers, allocator.clone())
        .is_err());
    assert!(plan
        .verify(&f, slots, host, 2, &[], &headers[..3], allocator.clone())
        .is_err());
    drop(plan);
    assert_eq!(allocator.0.current(), 0);
}

#[test]
fn entry_binding_analysis_refuses_invalid_source_and_releases_workspace() {
    let (f, slots, host, headers) = fixture(true, true, false, 0);
    let original = f.display().to_string();
    for limit in [0, 1, f.dfg.num_blocks() * std::mem::size_of::<Facts>() - 1] {
        let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(limit));
        assert!(matches!(
            Requirements::new(&f, slots, host, 1, &[], &headers, allocator.clone()),
            Err(CompileError::ResourceLimit(_))
        ));
        assert_eq!(allocator.0.current(), 0);
        assert_eq!(f.display().to_string(), original);
    }
    let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
    assert!(Requirements::new(&f, slots, host, 0, &[], &headers, allocator.clone()).is_err());
    let mut changed = f.clone();
    let stale = changed.dfg.make_block();
    assert!(Requirements::new(&changed, slots, host, 1, &[], &[stale], allocator.clone()).is_err());
    let mut remaining = 0;
    assert!(matches!(
        charge(&mut remaining),
        Err(CompileError::ResourceLimit(_))
    ));
    assert_eq!(allocator.0.current(), 0);
}

#[test]
fn entry_binding_metadata_transfer_charges_exact_storage_and_rolls_back() {
    let (f, slots, host, headers) = fixture(true, false, true, 1);
    let bytes = headers.len() * std::mem::size_of::<Mask>();
    for limit in [0, bytes - 1, bytes] {
        let source = BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
        let metadata = BudgetAllocator(crate::jit::resources::Ledger::new(limit));
        let plan = Requirements::new(&f, slots, host, 2, &[], &headers, source.clone()).unwrap();
        assert_eq!(source.0.current(), bytes);
        let retained = plan.into_metadata(metadata.clone());
        assert_eq!(retained.is_ok(), limit == bytes);
        assert_eq!(source.0.current(), 0);
        assert_eq!(metadata.0.current(), if limit == bytes { bytes } else { 0 });
        if let Ok(ref plan) = retained {
            assert!(plan.get(0).unwrap().contains(1));
        }
        drop(retained);
        assert_eq!(metadata.0.current(), 0);
    }
}

#[test]
fn entry_binding_helpers_do_not_fabricate_descriptor_definitions() {
    use cranelift_codegen::ir::{ExtFuncData, ExternalName};
    for kind in 1..=10 {
        let (mut f, slots, host, headers) = fixture(false, false, true, 0);
        let mut signature = Signature::new(f.signature.call_conv);
        signature.params.extend(
            [
                types::I64,
                types::I64,
                types::I32,
                types::I32,
                types::I32,
                types::I32,
            ]
            .map(AbiParam::new),
        );
        signature.returns.push(AbiParam::new(types::I32));
        let signature = f.import_signature(signature);
        let reference = f.import_function(ExtFuncData {
            name: ExternalName::testcase("binding_helper"),
            signature,
            colocated: false,
            patchable: false,
        });
        let first = f.layout.first_inst(headers[3]).unwrap();
        let mut cursor = FuncCursor::new(&mut f);
        cursor.goto_inst(first);
        let zero = cursor.ins().iconst(types::I32, 0);
        cursor
            .ins()
            .call(reference, &[host, slots, zero, zero, zero, zero]);
        cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
        let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024));
        let mut plan = Requirements::new(
            &f,
            slots,
            host,
            1,
            &[(kind, reference)],
            &headers,
            allocator.clone(),
        )
        .unwrap();
        for pc in 0..headers.len() {
            assert!(plan.get(pc).unwrap().contains(0));
        }
        plan.entries[3] = Mask::default();
        assert!(plan
            .verify(
                &f,
                slots,
                host,
                1,
                &[(kind, reference)],
                &headers,
                allocator.clone()
            )
            .is_err());
        drop(plan);
        assert_eq!(allocator.0.current(), 0);
    }
}
