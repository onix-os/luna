use super::*;
use cranelift_codegen::{
    ir::{AbiParam, InstBuilder, MemFlagsData, Signature},
    settings,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

fn fixture() -> (Function, Roots) {
    fixture_with_resume(false)
}

fn fixture_with_resume(resumable: bool) -> (Function, Roots) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut context);
    let block = b.create_block();
    b.append_block_params_for_function_params(block);
    b.switch_to_block(block);
    let p = b.block_params(block).to_vec();
    let roots = Roots {
        slots: p[0],
        exit: p[3],
        registers: 1,
    };
    let body = b.create_block();
    let resume = b.create_block();
    if resumable {
        let active = b
            .ins()
            .icmp_imm_s(cranelift_codegen::ir::condcodes::IntCC::NotEqual, p[1], 0);
        b.ins().brif(active, resume, &[], body, &[]);
    } else {
        b.ins().jump(body, &[]);
    }
    b.switch_to_block(body);
    let a = b.ins().iconst(types::I64, 11);
    let v = b.ins().iconst(types::I64, 17);
    b.ins().store(MemFlagsData::new(), a, p[0], 0);
    b.ins().store(MemFlagsData::new(), v, p[0], 8);
    let mut sig = Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    sig.params.push(AbiParam::new(types::I64));
    let sig = b.import_signature(sig);
    b.ins().call_indirect(sig, p[4], &[p[0]]);
    let tag = b.ins().load(types::I64, MemFlagsData::new(), p[0], 0);
    let bits = b.ins().load(types::I64, MemFlagsData::new(), p[0], 8);
    let sum = b.ins().iadd(tag, bits);
    b.ins().store(MemFlagsData::new(), sum, p[0], 8);
    b.ins().jump(resume, &[]);
    b.switch_to_block(resume);
    let tag = b.ins().load(types::I64, MemFlagsData::new(), p[0], 0);
    let bits = b.ins().load(types::F64, MemFlagsData::new(), p[0], 8);
    let bits = b.ins().bitcast(types::I64, MemFlagsData::new(), bits);
    let result = b.ins().iadd(tag, bits);
    b.ins().store(MemFlagsData::new(), result, p[3], 0);
    let count = b.ins().iconst(types::I32, 2);
    let reason = b.ins().iconst(types::I32, 0);
    b.ins().store(MemFlagsData::new(), count, p[3], 8);
    b.ins().store(MemFlagsData::new(), reason, p[3], 12);
    b.ins().return_(&[result]);
    b.seal_all_blocks();
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    b.finalize(isa.frontend_config());
    (f, roots)
}

fn memory(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn bound(f: &Function) -> Expansion {
    Expansion {
        instructions: f.dfg.num_insts(),
        blocks: f.layout.blocks().count(),
    }
}

fn apply(
    f: &mut Function,
    roots: Roots,
    quota: BudgetAllocator,
) -> Result<Reservation, CompileError> {
    lower(f, roots.slots, roots.exit, roots.registers, quota, bound(f))
}

fn first_load(f: &Function) -> Inst {
    (0..f.dfg.num_insts())
        .map(|i| Inst::from_u32(i as u32))
        .find(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .unwrap()
}

#[test]
fn field_memory_classifies_exact_nonoverlapping_ranges() {
    let (mut source, mut roots) = fixture();
    let inst = first_load(&source);
    for registers in [0, 1, 7, 16, 64, 256] {
        roots.registers = registers;
        for offset in -8..=(registers as i32 * 16 + 8) {
            let InstructionData::Load { offset: actual, .. } = &mut source.dfg.insts[inst] else {
                unreachable!()
            };
            *actual = offset.into();
            let result = classify(&source, inst, roots);
            if offset >= 0 && offset % 8 == 0 && offset < registers as i32 * 16 {
                assert_eq!(result.unwrap(), Some(FIRST_FIELD + offset as u32 / 8));
            } else {
                assert!(result.is_err());
            }
        }
    }
    for (registers, same) in [(257, false), (1, true)] {
        let (mut source, roots) = fixture();
        let original = source.clone();
        assert!(apply(
            &mut source,
            Roots {
                registers,
                exit: if same { roots.slots } else { roots.exit },
                ..roots
            },
            memory(1 << 20)
        )
        .is_err());
        assert_eq!(source, original);
    }
}

#[test]
fn field_memory_verifier_rejects_address_region_and_flag_mutations() {
    let (source, roots) = fixture();
    let quota = memory(1 << 20);
    let mut lowered = source.clone();
    let _charge = apply(&mut lowered, roots, quota.clone()).unwrap();
    assert_eq!(lowered.dfg.alias_regions.len(), 3);
    cranelift_codegen::verify_function(&lowered, &settings::Flags::new(settings::builder()))
        .unwrap();
    let load = first_load(&source);
    for mutation in 0..7 {
        let mut candidate = lowered.clone();
        if mutation < 4 {
            let flags = candidate.dfg.insts[load].memflags().unwrap();
            let data = candidate.dfg.mem_flags[flags];
            let data = match mutation {
                0 => data.with_alias_region(None),
                1 => data.with_alias_region(candidate.dfg.alias_regions.get(FIRST_FIELD + 1)),
                2 => data.with_notrap(),
                _ => data.with_readonly(),
            };
            let flags = candidate.dfg.mem_flags.insert(data).unwrap();
            *candidate.dfg.insts[load].memflags_mut().unwrap() = flags;
        } else if mutation == 4 {
            let InstructionData::Load { offset, .. } = &mut candidate.dfg.insts[load] else {
                unreachable!()
            };
            *offset = 8.into();
        } else if mutation == 5 {
            let InstructionData::Load { arg, .. } = &mut candidate.dfg.insts[load] else {
                unreachable!()
            };
            *arg = roots.exit;
        } else {
            candidate.signature.returns.clear();
        }
        assert!(verify(&source, &mut candidate, roots, quota.clone()).is_err());
    }
    for pointer in [
        roots.exit,
        source
            .dfg
            .block_params(source.layout.entry_block().unwrap())[4],
    ] {
        let mut candidate = source.clone();
        let InstructionData::Load { arg, .. } = &mut candidate.dfg.insts[load] else {
            unreachable!()
        };
        *arg = pointer;
        let original = candidate.clone();
        assert!(apply(&mut candidate, roots, quota.clone()).is_err());
        assert_eq!(candidate, original);
    }
}

#[test]
fn field_memory_refusal_is_atomic_and_tables_follow_guard_lifetime() {
    let (source, roots) = fixture();
    let quota = memory(1 << 20);
    let mut candidate = source.clone();
    let charge = apply(&mut candidate, roots, quota.clone()).unwrap();
    let retained = quota.0.current();
    let peak = quota.0.peak();
    assert!(retained > 16 * 1024 && peak > retained);
    drop((candidate, charge));
    assert_eq!(quota.0.current(), 0);
    for limit in [0, retained - 1, retained, peak - 1, peak] {
        let quota = memory(limit);
        let mut candidate = source.clone();
        let result = apply(&mut candidate, roots, quota.clone());
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
        roots.slots,
        roots.exit,
        roots.registers,
        quota.clone(),
        Expansion {
            instructions: 1,
            blocks: 1
        }
    )
    .is_err());
    assert_eq!(candidate, source);
    let charge = apply(&mut candidate, roots, quota.clone()).unwrap();
    let original = candidate.clone();
    assert!(apply(&mut candidate, roots, quota.clone()).is_err());
    assert_eq!(candidate, original);
    drop((candidate, charge));
    assert_eq!(quota.0.current(), 0);
}

#[cfg(not(miri))]
#[test]
fn field_memory_native_preserves_float_bits_and_helper_clobbers() {
    use cranelift_codegen::settings::Configurable;
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{default_libcall_names, Linkage, Module};
    unsafe extern "C" fn change(pointer: *mut i64) {
        unsafe {
            *pointer += 5;
            *pointer.add(1) += 7;
        }
    }
    let mut optimized_loads = [[0usize; 2]; 2];
    for resumable in [false, true] {
        for selected in [false, true] {
            let mut flags = settings::builder();
            flags.set("opt_level", "speed").unwrap();
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(settings::Flags::new(flags))
                .unwrap();
            let _charge;
            let mut module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
            let mut context = module.make_context();
            let (source, roots) = fixture_with_resume(resumable);
            context.func = source;
            _charge = selected.then(|| apply(&mut context.func, roots, memory(1 << 20)).unwrap());
            let id = module
                .declare_function("field_probe", Linkage::Export, &context.func.signature)
                .unwrap();
            module.define_function(id, &mut context).unwrap();
            optimized_loads[usize::from(resumable)][usize::from(selected)] = context
                .func
                .layout
                .blocks()
                .flat_map(|block| context.func.layout.block_insts(block))
                .filter(|&inst| context.func.dfg.insts[inst].opcode().can_load())
                .count();
            module.finalize_definitions().unwrap();
            let call: unsafe extern "C" fn(
                *mut i64,
                u64,
                u32,
                *mut crate::jit::abi::Exit,
                usize,
            ) -> i64 = unsafe { std::mem::transmute(module.get_finalized_function(id)) };
            for _ in 0..100 {
                for pc in 0..=u64::from(resumable) {
                    let mut slots = [16i64, 40];
                    let mut exit = crate::jit::abi::Exit::default();
                    assert_eq!(
                        unsafe {
                            call(
                                slots.as_mut_ptr(),
                                pc,
                                64,
                                &mut exit,
                                change as *const () as usize,
                            )
                        },
                        56
                    );
                    assert_eq!(slots, [16, 40]);
                    assert_eq!((exit.pc, exit.instructions, exit.reason), (56, 2, 0));
                }
            }
            drop(context);
            unsafe {
                module.free_memory();
            }
        }
    }
    println!("field memory fixture optimized loads: {optimized_loads:?}");
    assert_eq!(optimized_loads, [[4, 3], [4, 4]]);
}
