use super::*;

fn allocator(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn fixture(floating: bool, extra_read: bool) -> (Function, Inst, IrValue) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let (slots, inst);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut context);
        let entry = b.create_block();
        let yes = b.create_block();
        let no = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        slots = b.block_params(entry)[0];
        let tag = b.ins().load(types::I64, MemFlagsData::new(), slots, 0);
        let bits = b.ins().load(
            if floating { types::F64 } else { types::I64 },
            MemFlagsData::new(),
            slots,
            8,
        );
        inst = b.func.dfg.value_def(bits).inst().unwrap();
        let extra = if extra_read {
            Some(b.ins().load(types::I64, MemFlagsData::new(), slots, 24))
        } else {
            None
        };
        let mask = b.ins().iconst(types::I64, -2);
        let kind = b.ins().band(tag, mask);
        let integer = b.ins().iconst(types::I64, abi::INTEGER as i64);
        let valid = b.ins().icmp(IntCC::Equal, kind, integer);
        b.ins().brif(valid, yes, &[], no, &[]);
        b.switch_to_block(yes);
        let bits = if floating {
            b.ins().bitcast(types::I64, MemFlagsData::new(), bits)
        } else {
            bits
        };
        let bits = if let Some(extra) = extra {
            b.ins().bxor(bits, extra)
        } else {
            bits
        };
        b.ins().return_(&[bits]);
        b.switch_to_block(no);
        let zero = b.ins().iconst(types::I64, 0);
        b.ins().return_(&[zero]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    (f, inst, slots)
}

fn valid(f: &Function) {
    cranelift_codegen::verify_function(f, &settings::Flags::new(settings::builder())).unwrap();
}

#[test]
fn numeric_read_refuses_stale_tags_unguarded_uses_and_exhausted_work() {
    let (f, inst, slots) = fixture(false, false);
    let g = candidate(&f, inst, slots, 0).unwrap();
    let accepted = |f: &Function| {
        valid(f);
        Analysis::new(f, allocator(1 << 20))
            .unwrap()
            .admit(f, inst, slots, 0)
            .is_some()
    };
    assert!(accepted(&f));
    let mut limited = Analysis::new(&f, allocator(1 << 20)).unwrap();
    limited.remaining = 0;
    assert!(limited.admit(&f, inst, slots, 0).is_none());
    for effect in 0..3 {
        let mut changed = f.clone();
        let tag = changed.dfg.first_result(g.tag);
        let bits = changed.dfg.first_result(inst);
        let convention = changed.signature.call_conv;
        let signature = changed.import_signature(cranelift_codegen::ir::Signature::new(convention));
        let mut c = FuncCursor::new(&mut changed);
        c.goto_inst(g.branch);
        match effect {
            0 => {
                c.ins().iadd(bits, tag);
            }
            1 => {
                c.ins().store(MemFlagsData::new(), tag, slots, 0);
            }
            _ => {
                c.ins().call_indirect(signature, slots, &[]);
            }
        }
        assert!(!accepted(&changed));
    }
    let mut changed = f.clone();
    let end = changed.layout.last_inst(g.rejected).unwrap();
    changed.replace(end).jump(g.accepted, &[]);
    assert!(!accepted(&changed));
    let mut changed = f.clone();
    let bits = changed.dfg.first_result(inst);
    let end = changed.layout.last_inst(g.rejected).unwrap();
    changed.replace(end).return_(&[bits]);
    assert!(!accepted(&changed));
    let mut changed = f.clone();
    let mask = changed.layout.next_inst(inst).unwrap();
    changed.replace(mask).iconst(types::I64, -6);
    assert!(!accepted(&changed));
    let mut changed = f.clone();
    let mut c = FuncCursor::new(&mut changed);
    c.goto_inst(g.branch);
    for _ in 0..256 {
        c.ins().iconst(types::I64, 0);
    }
    assert!(!accepted(&changed));
}

#[test]
fn numeric_read_checker_rejects_guard_pointer_and_control_mutations() {
    for floating in [false, true] {
        let (mut f, inst, slots) = fixture(floating, false);
        let g = Analysis::new(&f, allocator(1 << 20))
            .unwrap()
            .admit(&f, inst, slots, 0)
            .unwrap();
        let read = emit(&mut f, inst, slots, 0, g);
        valid(&f);
        let check = |f: &Function| {
            verify(
                f,
                &read,
                slots,
                &Predecessors::new(f, allocator(1 << 20)).unwrap(),
            )
        };
        check(&f).unwrap();
        let mut count = 0;
        for block in [read.blocks[0], read.blocks[2], read.blocks[3]] {
            for inst in f.layout.block_insts(block) {
                let mut changed = f.clone();
                let dfg = &mut changed.dfg;
                match &mut dfg.insts[inst] {
                    InstructionData::Load { offset, .. } => {
                        *offset = (i32::from(*offset) + 1).into()
                    }
                    InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                    InstructionData::IntCompare { cond, .. } => *cond = IntCC::Equal,
                    InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                    InstructionData::Jump { destination, .. } => {
                        destination.update_args(&mut dfg.value_lists, |_| BlockArg::Value(slots));
                    }
                    _ => panic!("unhandled numeric read instruction"),
                }
                valid(&changed);
                assert!(check(&changed).is_err());
                count += 1;
            }
        }
        assert_eq!(count, 7);
        let mut changed = f.clone();
        let mask = changed.layout.next_inst(g.tag).unwrap();
        changed.replace(mask).iconst(types::I64, -6);
        valid(&changed);
        assert!(check(&changed).is_err());
        for bypass in [read.blocks[2], read.blocks[3], read.blocks[1]] {
            let mut changed = f.clone();
            let end = changed.layout.last_inst(g.rejected).unwrap();
            if bypass == read.blocks[1] {
                changed.replace(end).jump(bypass, &[slots.into()]);
            } else {
                changed.replace(end).jump(bypass, &[]);
            }
            assert!(check(&changed).is_err());
        }
        let mut changed = f.clone();
        let end = changed.layout.last_inst(g.rejected).unwrap();
        changed.replace(end).jump(read.blocks[0], &[]);
        valid(&changed);
        assert!(check(&changed).is_err());
        let mut changed = f.clone();
        let tag = changed.dfg.first_result(g.tag);
        let mut c = FuncCursor::new(&mut changed);
        c.goto_inst(g.branch);
        c.ins().store(MemFlagsData::new(), tag, slots, 0);
        valid(&changed);
        assert!(check(&changed).is_err());
        if let Some((cast, _)) = read.cast {
            let mut changed = f.clone();
            changed
                .replace(cast)
                .fcvt_from_sint(types::F64, read.result);
            valid(&changed);
            assert!(check(&changed).is_err());
        }
    }
}

#[test]
fn numeric_read_graph_quota_refusal_preserves_source_and_releases_storage() {
    let (source, _, slots) = fixture(false, true);
    let limits = crate::jit::work::Expansion {
        instructions: 4096,
        blocks: 4096,
    };
    let generous = allocator(1 << 20);
    let mut f = source.clone();
    lower(&mut f, slots, slots, 2, &[], generous.clone(), limits).unwrap();
    let peak = generous.0.peak();
    assert_eq!(generous.0.current(), 0);
    assert_ne!(f, source);
    valid(&f);
    for limit in [0, peak - 1, peak] {
        let ledger = allocator(limit);
        let mut f = source.clone();
        let result = lower(&mut f, slots, slots, 2, &[], ledger.clone(), limits);
        assert_eq!(ledger.0.current(), 0);
        if limit == peak {
            result.unwrap();
            valid(&f);
        } else {
            assert!(matches!(result, Err(JitError::ResourceLimit(_))));
            assert_eq!(f, source);
        }
    }
}

#[cfg(not(miri))]
#[test]
fn native_numeric_reads_preserve_bits_and_refuse_non_numeric_and_null_pointers() {
    for floating in [false, true] {
        for extra in [false, true] {
            let (mut f, inst, slots) = fixture(floating, extra);
            let g = Analysis::new(&f, allocator(1 << 20))
                .unwrap()
                .admit(&f, inst, slots, 0)
                .unwrap();
            assert_eq!(g.accepted, candidate(&f, inst, slots, 0).unwrap().accepted);
            lower(
                &mut f,
                slots,
                slots,
                2,
                &[],
                allocator(1 << 20),
                crate::jit::work::Expansion {
                    instructions: 4096,
                    blocks: 4096,
                },
            )
            .unwrap();
            valid(&f);
            crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
                let entry = unsafe {
                    std::mem::transmute::<
                        *const u8,
                        unsafe extern "C" fn(*const crate::jit::abi::payload::Payload) -> u64,
                    >(pointer)
                };
                crate::jit::abi::payload::check_native_numeric_read(entry);
            })
            .unwrap();
        }
    }
}
