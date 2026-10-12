use super::*;
use cranelift_codegen::{
    ir::{types, AbiParam, InstBuilder, Signature},
    settings,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

fn fixture() -> Function {
    let mut function = Function::new();
    function.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    function
        .signature
        .params
        .extend([AbiParam::new(types::I64); 3]);
    function.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut function, &mut context);
    let block = b.create_block();
    b.append_block_params_for_function_params(block);
    b.switch_to_block(block);
    let args = b.block_params(block).to_vec();
    let eleven = b.ins().iconst(types::I64, 11);
    let seventeen = b.ins().iconst(types::I64, 17);
    b.ins().store(MemFlagsData::new(), eleven, args[0], 0);
    b.ins().store(MemFlagsData::new(), seventeen, args[1], 0);
    let first = b.ins().load(types::I64, MemFlagsData::new(), args[0], 0);
    let mut sig = Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    sig.params.push(AbiParam::new(types::I64));
    let sig = b.import_signature(sig);
    b.ins().call_indirect(sig, args[2], &[args[0]]);
    let second = b.ins().load(types::I64, MemFlagsData::new(), args[0], 0);
    let sum = b.ins().iadd(first, second);
    let bits = b.ins().bitcast(types::F64, MemFlagsData::new(), sum);
    let sum = b.ins().bitcast(types::I64, MemFlagsData::new(), bits);
    b.ins().store(MemFlagsData::new(), sum, args[1], 0);
    let final_value = b.ins().load(types::I64, MemFlagsData::new(), args[0], 0);
    b.ins().return_(&[final_value]);
    b.seal_all_blocks();
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    b.finalize(isa.frontend_config());
    function
}

fn memory(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn bound(function: &Function) -> Expansion {
    Expansion {
        instructions: function.dfg.num_insts(),
        blocks: function.layout.blocks().count(),
    }
}

#[test]
fn universal_memory_verifier_rejects_non_region_changes() {
    let source = fixture();
    let mut lowered = source.clone();
    let quota = memory(1 << 20);
    let _charge = lower(&mut lowered, quota.clone(), bound(&source)).unwrap();
    cranelift_codegen::verify_function(&lowered, &settings::Flags::new(settings::builder()))
        .unwrap();
    for index in 0..source.dfg.num_insts() {
        let inst = Inst::from_u32(index as u32);
        let data = &source.dfg.insts[inst];
        if !data.opcode().can_load() && !data.opcode().can_store() {
            continue;
        }
        for mutation in 0..5 {
            let mut changed = lowered.clone();
            let flags = changed.dfg.insts[inst].memflags().unwrap();
            let data = changed.dfg.mem_flags[flags];
            let data = match mutation {
                0 => data.with_alias_region(None),
                1 => data.with_notrap(),
                2 => data.with_aligned(),
                3 => data.with_readonly(),
                _ => data.with_endianness(cranelift_codegen::ir::Endianness::Big),
            };
            let flags = changed.dfg.mem_flags.insert(data).unwrap();
            *changed.dfg.insts[inst].memflags_mut().unwrap() = flags;
            assert!(verify(&source, &mut changed, quota.clone()).is_err());
        }
    }
    let mut changed = lowered.clone();
    changed.signature.returns.clear();
    assert!(verify(&source, &mut changed, quota.clone()).is_err());
    let mut changed = lowered.clone();
    changed.dfg.alias_regions.insert(AliasRegionData {
        user_id: 0,
        description: "other".into(),
    });
    assert!(verify(&source, &mut changed, quota.clone()).is_err());
    let mut changed = lowered.clone();
    let inst = (0..changed.dfg.num_insts())
        .map(|index| Inst::from_u32(index as u32))
        .find(|&inst| changed.dfg.insts[inst].opcode() == cranelift_codegen::ir::Opcode::Iadd)
        .unwrap();
    changed.dfg.inst_args_mut(inst).swap(0, 1);
    assert!(verify(&source, &mut changed, quota).is_err());
}

#[test]
fn universal_memory_rejects_uncovered_memory_and_large_tables() {
    use cranelift_codegen::ir::{GlobalValueData, InstructionData, Opcode, TrapCode};
    let source = fixture();
    let quota = memory(1 << 20);
    let mut full = source.clone();
    for code in 1..MAX_FLAGS as u8 {
        full.dfg
            .mem_flags
            .insert(MemFlagsData::new().with_trap_code(TrapCode::user(code)))
            .unwrap();
    }
    let mut admitted = full.clone();
    let charge = lower(&mut admitted, quota.clone(), bound(&full)).unwrap();
    drop((admitted, charge));
    full.dfg
        .mem_flags
        .insert(MemFlagsData::new().with_trap_code(TrapCode::user(MAX_FLAGS as u8)))
        .unwrap();
    let before = full.clone();
    assert!(matches!(
        lower(&mut full, quota.clone(), bound(&before)),
        Err(JitError::ResourceLimit("universal memory source flags"))
    ));
    assert_eq!(full, before);

    let mut global = source.clone();
    let base = global.global_values.push(GlobalValueData::VMContext);
    let flags = global
        .dfg
        .mem_flags
        .insert(MemFlagsData::trusted())
        .unwrap();
    global.global_values.push(GlobalValueData::Load {
        base,
        offset: 0.into(),
        global_type: types::I64,
        flags,
    });
    let before = global.clone();
    assert!(lower(&mut global, quota.clone(), bound(&before)).is_err());
    assert_eq!(global, before);

    let mut malformed = source.clone();
    let pointer = malformed
        .dfg
        .block_params(malformed.layout.entry_block().unwrap())[0];
    malformed.dfg.make_inst(InstructionData::Unary {
        opcode: Opcode::Load,
        arg: pointer,
    });
    let before = malformed.clone();
    assert!(lower(&mut malformed, quota.clone(), bound(&before)).is_err());
    assert_eq!(malformed, before);
    assert_eq!(quota.0.current(), 0);
}

#[test]
fn universal_memory_quota_and_expansion_refusals_are_atomic() {
    let source = fixture();
    let quota = memory(1 << 20);
    let mut candidate = source.clone();
    let charge = lower(&mut candidate, quota.clone(), bound(&source)).unwrap();
    assert_eq!(quota.0.current(), TABLE_BYTES);
    let peak = quota.0.peak();
    drop((candidate, charge));
    assert_eq!(quota.0.current(), 0);
    for limit in [0, TABLE_BYTES, peak - 1, peak] {
        let quota = memory(limit);
        let mut candidate = source.clone();
        let result = lower(&mut candidate, quota.clone(), bound(&source));
        assert_eq!(result.is_ok(), limit == peak);
        if result.is_err() {
            assert_eq!(candidate, source);
        }
        drop((candidate, result));
        assert_eq!(quota.0.current(), 0);
    }
    let mut candidate = source.clone();
    assert!(lower(
        &mut candidate,
        quota.clone(),
        Expansion {
            instructions: 1,
            blocks: 1
        }
    )
    .is_err());
    assert_eq!(candidate, source);
    let charge = lower(&mut candidate, quota.clone(), bound(&source)).unwrap();
    let prior = candidate.clone();
    assert!(lower(&mut candidate, quota.clone(), bound(&source)).is_err());
    assert_eq!(candidate, prior);
    drop((candidate, charge));
    assert_eq!(quota.0.current(), 0);
}

#[cfg(not(miri))]
#[test]
fn universal_memory_native_preserves_aliases_and_helper_clobbers() {
    use cranelift_codegen::settings::Configurable;
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{default_libcall_names, Linkage, Module};
    unsafe extern "C" fn increment(pointer: *mut i64) {
        unsafe {
            *pointer += 5;
        }
    }
    for selected in [false, true] {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").unwrap();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        let mut module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
        let mut context = module.make_context();
        context.func = fixture();
        let expansion = bound(&context.func);
        let _charge =
            selected.then(|| lower(&mut context.func, memory(1 << 20), expansion).unwrap());
        let id = module
            .declare_function("memory_probe", Linkage::Export, &context.func.signature)
            .unwrap();
        module.define_function(id, &mut context).unwrap();
        module.finalize_definitions().unwrap();
        let call: unsafe extern "C" fn(*mut i64, *mut i64, usize) -> i64 =
            unsafe { std::mem::transmute(module.get_finalized_function(id)) };
        for aliased in [false, true] {
            let mut values = [0i64; 2];
            let x = values.as_mut_ptr();
            let y = unsafe { x.add(usize::from(!aliased)) };
            let result = unsafe { call(x, y, increment as *const () as usize) };
            assert_eq!(result, if aliased { 39 } else { 16 });
            assert_eq!(values, if aliased { [39, 0] } else { [16, 27] });
        }
        unsafe {
            module.free_memory();
        }
    }
}
