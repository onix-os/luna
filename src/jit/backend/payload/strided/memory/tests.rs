use super::*;

fn allocator(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn fixture(call: bool) -> (Function, IrValue, [Read; 2]) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.extend([AbiParam::new(types::I64); 4]);
    f.signature.returns.push(AbiParam::new(types::I64));
    let block = f.dfg.make_block();
    f.layout.append_block(block);
    let args: Vec<_> = (0..4)
        .map(|_| f.dfg.append_block_param(block, types::I64))
        .collect();
    let [slots, bits, other, helper]: [IrValue; 4] = args.try_into().unwrap();
    let call_conv = f.signature.call_conv;
    let signature = f.import_signature(Signature {
        call_conv,
        params: vec![AbiParam::new(types::I64); 2],
        returns: vec![],
    });
    let mut c = FuncCursor::new(&mut f);
    c.goto_bottom(block);
    let first = c.ins().load(types::I64, MemFlagsData::new(), slots, 8);
    let before = c.ins().load(types::I64, MemFlagsData::new(), first, 0);
    c.ins().store(MemFlagsData::new(), bits, first, 0);
    if call {
        c.ins().call_indirect(signature, helper, &[slots, other]);
    }
    let second = c.ins().load(types::I64, MemFlagsData::new(), slots, 8);
    let after = c.ins().load(types::I64, MemFlagsData::new(), second, 0);
    let sum = c.ins().iadd(before, after);
    c.ins().return_(&[sum]);
    let reads = [first, second].map(|result| Read {
        inst: f.dfg.value_def(result).inst().unwrap(),
        index: 0,
        pointer: true,
        result,
        uniform: Header::uniform_offset(),
        numeric: false,
    });
    for read in reads {
        super::super::emit(&mut f, slots, read);
    }
    (f, slots, reads)
}

#[test]
fn strided_memory_partitions_metadata_and_preserves_all_other_ir() {
    let (source, slots, reads) = fixture(true);
    let mut candidate = source.clone();
    let quota = allocator(1 << 20);
    let charge = lower(&mut candidate, slots, &reads, quota.clone()).unwrap();
    assert_eq!(quota.0.current(), TABLE_BYTES);
    verify(&source, &mut candidate, slots, &reads, quota.clone()).unwrap();
    let marked = metadata(&source, slots, &reads, quota.clone()).unwrap();
    assert!(marked.iter().any(|&v| v));
    for index in 0..source.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        if source.dfg.insts[inst].opcode().can_load() || source.dfg.insts[inst].opcode().can_store()
        {
            let expected = if marked[index] { HEADER } else { OTHER };
            assert_eq!(
                candidate.dfg.insts[inst].alias_region(&candidate.dfg),
                candidate.dfg.alias_regions.get(expected)
            );
            if marked[index] {
                assert!(!source.dfg.insts[inst].opcode().can_store());
            }
        }
    }
    drop(marked);
    drop(charge);
    assert_eq!(quota.0.current(), 0);
}

#[test]
fn strided_memory_checker_rejects_regions_flags_and_source_mutations() {
    let (source, slots, reads) = fixture(true);
    let mut candidate = source.clone();
    let quota = allocator(1 << 20);
    let _charge = lower(&mut candidate, slots, &reads, quota.clone()).unwrap();
    for index in 0..candidate.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let Some(flags) = candidate.dfg.insts[inst].memflags() else {
            continue;
        };
        for altered in [
            None,
            candidate.dfg.alias_regions.get(HEADER),
            candidate.dfg.alias_regions.get(OTHER),
        ] {
            if altered == candidate.dfg.mem_flags[flags].alias_region() {
                continue;
            }
            let mut changed = candidate.clone();
            let flags = changed
                .dfg
                .mem_flags
                .insert(candidate.dfg.mem_flags[flags].with_alias_region(altered))
                .unwrap();
            *changed.dfg.insts[inst].memflags_mut().unwrap() = flags;
            assert!(verify(&source, &mut changed, slots, &reads, quota.clone()).is_err());
        }
        let mut changed = candidate.clone();
        let mut flags = candidate.dfg.mem_flags[flags];
        flags.set_readonly();
        let flags = changed.dfg.mem_flags.insert(flags).unwrap();
        *changed.dfg.insts[inst].memflags_mut().unwrap() = flags;
        assert!(verify(&source, &mut changed, slots, &reads, quota.clone()).is_err());
    }
    let mut changed = candidate.clone();
    let block = changed.layout.entry_block().unwrap();
    let add = changed
        .layout
        .block_insts(block)
        .find(|&inst| changed.dfg.insts[inst].opcode() == Opcode::Iadd)
        .unwrap();
    let args = changed.dfg.inst_args(add).to_vec();
    changed.replace(add).isub(args[0], args[1]);
    cranelift_codegen::verify_function(&changed, &settings::Flags::new(settings::builder()))
        .unwrap();
    assert!(verify(&source, &mut changed, slots, &reads, quota.clone()).is_err());
    let mut changed = candidate.clone();
    changed.dfg.alias_regions = AliasRegionSet::new();
    changed.dfg.alias_regions.insert(AliasRegionData {
        user_id: HEADER,
        description: "altered metadata".into(),
    });
    changed.dfg.alias_regions.insert(AliasRegionData {
        user_id: OTHER,
        description: "strided other memory".into(),
    });
    assert!(verify(&source, &mut changed, slots, &reads, quota).is_err());
}

#[test]
fn strided_memory_quota_refusal_preserves_source_and_releases_tables() {
    let (source, slots, reads) = fixture(false);
    let quota = allocator(1 << 20);
    let mut candidate = source.clone();
    drop(lower(&mut candidate, slots, &reads, quota.clone()).unwrap());
    assert_eq!(quota.0.current(), 0);
    for limit in [0, TABLE_BYTES - 1, quota.0.peak() - 1, quota.0.peak()] {
        let memory = allocator(limit);
        let mut candidate = source.clone();
        let result = lower(&mut candidate, slots, &reads, memory.clone());
        if limit == quota.0.peak() {
            let charge = result.unwrap();
            assert_eq!(memory.0.current(), TABLE_BYTES);
            drop(charge);
        } else {
            assert!(matches!(result, Err(JitError::ResourceLimit(_))));
            assert_eq!(candidate, source);
        }
        assert_eq!(memory.0.current(), 0);
    }
}

#[cfg(not(miri))]
#[test]
fn strided_memory_native_loads_observe_stores_and_helper_rebinding() {
    unsafe extern "C" fn rebind(header: *mut Header, other: *mut u8) {
        unsafe {
            (*header).base = other;
        }
    }
    for call in [false, true] {
        let (mut f, slots, reads) = fixture(call);
        let quota = allocator(1 << 20);
        let charge = lower(&mut f, slots, &reads, quota.clone()).unwrap();
        crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
            let entry = unsafe {
                std::mem::transmute::<
                    *const u8,
                    unsafe extern "C" fn(*mut Header, u64, *mut u8, *const u8) -> u64,
                >(pointer)
            };
            let mut first = crate::Value::Integer(11);
            let mut second = crate::Value::Integer(22);
            let base = std::ptr::from_mut(&mut first).cast::<u8>();
            let crate::Value::Integer(ref mut value) = first else {
                unreachable!()
            };
            let offset = std::ptr::from_mut(value).addr() - base.addr();
            let mut header = Header {
                base,
                offsets: [0, 0, offset, 0, 0],
                tags: [abi::INTEGER as u8; 256],
            };
            let other = std::ptr::from_mut(&mut second).cast();
            assert_eq!(
                unsafe { entry(&mut header, 77, other, rebind as *const u8) },
                if call { 33 } else { 88 }
            );
            assert!(matches!(first, crate::Value::Integer(77)));
            assert!(matches!(second, crate::Value::Integer(22)));
        })
        .unwrap();
        drop(charge);
        assert_eq!(quota.0.current(), 0);
    }
}
