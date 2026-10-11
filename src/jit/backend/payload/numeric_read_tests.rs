use super::*;

fn allocator(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

#[test]
fn bound_numeric_reads_have_exact_expansion_and_workspace_limits() {
    for floating in [false, true] {
        for paired in [false, true] {
            let (source, slots) = if paired {
                paired_fixture(floating)
            } else {
                let (f, _, slots) = fixture(floating, false);
                (f, slots)
            };
            let original = size(&source);
            let mut f = source.clone();
            let memory = allocator(1 << 20);
            let lower = |f: &mut Function, memory| {
                lower_contract(
                    f,
                    slots,
                    slots,
                    2,
                    &[],
                    memory,
                    crate::jit::work::Expansion {
                        instructions: 4096,
                        blocks: 4096,
                    },
                    Contract::FrameBound,
                )
            };
            lower(&mut f, memory.clone()).unwrap();
            valid(&f);
            assert_eq!(size(&f), (original.0 + 1 + usize::from(paired), original.1));
            assert_eq!(memory.0.current(), 0);
            let peak = memory.0.peak();
            for limit in [0, peak - 1, peak] {
                let ledger = allocator(limit);
                let mut candidate = source.clone();
                let result = lower(&mut candidate, ledger.clone());
                assert_eq!(result.is_ok(), limit == peak);
                assert_eq!(ledger.0.current(), 0);
                if result.is_err() {
                    assert_eq!(candidate, source);
                }
            }
        }
    }
}

#[test]
fn bound_numeric_reads_recheck_source_and_emitted_guards_independently() {
    for floating in [false, true] {
        let (source, inst, slots) = fixture(floating, false);
        let mut analysis = Analysis::new(&source, allocator(1 << 20)).unwrap();
        let proof = analysis.admit(&source, inst, slots, 0).unwrap();
        let mut f = source.clone();
        let read = emit_bound(&mut f, inst, slots, 0, proof);
        analysis.verify_bound_source(&source, &read, slots).unwrap();
        let check = |f: &Function| {
            valid(f);
            verify_bound(
                f,
                &read,
                slots,
                &Predecessors::new(f, allocator(1 << 20)).unwrap(),
            )
        };
        check(&f).unwrap();
        for mutation in 0..8 {
            let mut changed = f.clone();
            match mutation {
                0 => {
                    let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[read.pointer]
                    else {
                        unreachable!()
                    };
                    *offset = 0.into();
                }
                1 => {
                    changed.dfg.inst_args_mut(read.inst)[0] = slots;
                }
                2 => {
                    let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[read.inst]
                    else {
                        unreachable!()
                    };
                    *offset = 8.into();
                }
                3 => {
                    let InstructionData::Brif { blocks, .. } = &mut changed.dfg.insts[proof.branch]
                    else {
                        unreachable!()
                    };
                    blocks.swap(0, 1);
                }
                4 => {
                    let end = changed.layout.last_inst(proof.rejected).unwrap();
                    changed.replace(end).jump(proof.accepted, &[]);
                }
                5 | 6 => {
                    let tag = changed.dfg.first_result(proof.tag);
                    let mut c = FuncCursor::new(&mut changed);
                    c.goto_inst(if mutation == 5 {
                        read.pointer
                    } else {
                        proof.branch
                    });
                    c.ins().store(MemFlagsData::new(), tag, slots, 0);
                }
                _ => {
                    changed.replace(read.pointer).iconst(types::I64, 1);
                }
            }
            assert!(
                check(&changed).is_err(),
                "floating={floating} mutation={mutation}"
            );
        }
        for mutation in 0..3 {
            let mut changed = source.clone();
            match mutation {
                0 => {
                    let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[inst] else {
                        unreachable!()
                    };
                    *offset = 24.into();
                }
                1 => {
                    let tag = changed.dfg.first_result(proof.tag);
                    let mut c = FuncCursor::new(&mut changed);
                    c.goto_inst(proof.branch);
                    c.ins().store(MemFlagsData::new(), tag, slots, 0);
                }
                _ => {
                    let mut c = FuncCursor::new(&mut changed);
                    c.goto_inst(proof.branch);
                    if floating {
                        c.ins().fneg(read.result);
                    } else {
                        c.ins().bnot(read.result);
                    }
                }
            }
            valid(&changed);
            assert!(Analysis::new(&changed, allocator(1 << 20))
                .unwrap()
                .verify_bound_source(&changed, &read, slots)
                .is_err());
        }
        analysis.verification_remaining = 0;
        assert!(analysis.verify_bound_source(&source, &read, slots).is_err());
    }
}

#[cfg(not(miri))]
#[test]
fn bound_numeric_native_reads_preserve_scalar_bits_and_guard_reference_cells() {
    for floating in [false, true] {
        let (mut f, _, slots) = fixture(floating, false);
        lower_contract(
            &mut f,
            slots,
            slots,
            1,
            &[],
            allocator(1 << 20),
            crate::jit::work::Expansion {
                instructions: 4096,
                blocks: 4096,
            },
            Contract::FrameBound,
        )
        .unwrap();
        crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
            let entry = unsafe {
                std::mem::transmute::<
                    *const u8,
                    unsafe extern "C" fn(*const crate::jit::abi::payload::Payload) -> u64,
                >(pointer)
            };
            crate::jit::abi::payload::runtime::check_native_bound_numeric_read(entry);
        })
        .unwrap();
    }
}

fn fixture(floating: bool, extra_read: bool) -> (Function, Inst, IrValue) {
    guarded_fixture(floating, extra_read, 0)
}

fn guarded_fixture(
    floating: bool,
    extra_read: bool,
    preceding: usize,
) -> (Function, Inst, IrValue) {
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
        for _ in 0..preceding {
            let other = b.ins().load(types::I64, MemFlagsData::new(), slots, 16);
            let mask = b.ins().iconst(types::I64, -2);
            let kind = b.ins().band(other, mask);
            let integer = b.ins().iconst(types::I64, abi::INTEGER as i64);
            let valid = b.ins().icmp(IntCC::Equal, kind, integer);
            let next = b.create_block();
            b.ins().brif(valid, next, &[], no, &[]);
            b.switch_to_block(next);
        }
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

fn paired_fixture(floating: bool) -> (Function, IrValue) {
    let (mut f, _, slots) = guarded_fixture(floating, false, 1);
    let entry = f.layout.entry_block().unwrap();
    let tag = f.layout.block_insts(entry).find(|&i|
        matches!(f.dfg.insts[i], InstructionData::Load { offset, .. } if i32::from(offset) == 16)
    ).unwrap();
    let first = f.layout.next_inst(tag).unwrap();
    let mut c = FuncCursor::new(&mut f);
    c.goto_inst(first);
    let other = c.ins().load(types::I64, MemFlagsData::new(), slots, 24);
    let end = f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).find(|&i| {
        f.dfg.insts[i].opcode() == Opcode::Return
            && !matches!(f.dfg.value_def(f.dfg.inst_args(i)[0]),
                cranelift_codegen::ir::ValueDef::Result(v, _) if f.dfg.insts[v].opcode() == Opcode::Iconst)
    }).unwrap();
    let bits = f.dfg.inst_args(end)[0];
    let mut c = FuncCursor::new(&mut f);
    c.goto_inst(end);
    let result = c.ins().bxor(bits, other);
    c.func.replace(end).return_(&[result]);
    (f, slots)
}

#[test]
fn numeric_read_chains_lower_both_operands_with_exact_savings() {
    for floating in [false, true] {
        let (mut f, slots) = paired_fixture(floating);
        valid(&f);
        let original = size(&f);
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
        assert_eq!(
            size(&f),
            (original.0 + 12 + usize::from(floating), original.1 + 6)
        );
    }
}

#[test]
fn numeric_read_searches_bounded_guard_chains_with_freshness_and_dominance() {
    for preceding in [1, 7, 8] {
        let (f, inst, slots) = guarded_fixture(false, false, preceding);
        valid(&f);
        let g = Analysis::new(&f, allocator(1 << 20))
            .unwrap()
            .admit(&f, inst, slots, 0);
        assert_eq!(g.is_some(), preceding < 8);
        let Some(g) = g else { continue };
        for effect in 0..4 {
            let mut changed = f.clone();
            let bits = changed.dfg.first_result(inst);
            let tag = changed.dfg.first_result(g.tag);
            let convention = changed.signature.call_conv;
            let sig = changed.import_signature(Signature::new(convention));
            let mut c = FuncCursor::new(&mut changed);
            c.goto_inst(g.branch);
            match effect {
                0 => {
                    c.ins().store(MemFlagsData::new(), tag, slots, 0);
                }
                1 => {
                    c.ins().call_indirect(sig, slots, &[]);
                }
                2 => {
                    c.ins().iadd(bits, bits);
                }
                _ => {
                    c.ins().store(MemFlagsData::new(), bits, slots, 8);
                }
            }
            valid(&changed);
            assert!(Analysis::new(&changed, allocator(1 << 20))
                .unwrap()
                .admit(&changed, inst, slots, 0)
                .is_none());
        }
        let mut changed = f.clone();
        let first = changed
            .layout
            .last_inst(changed.layout.entry_block().unwrap())
            .unwrap();
        if let InstructionData::Brif { blocks, .. } = &mut changed.dfg.insts[first] {
            blocks.swap(0, 1);
        } else {
            unreachable!()
        }
        assert!(Analysis::new(&changed, allocator(1 << 20))
            .unwrap()
            .admit(&changed, inst, slots, 0)
            .is_none());
        let mut changed = f.clone();
        let original = size(&changed);
        lower(
            &mut changed,
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
        valid(&changed);
        assert_eq!(size(&changed), (original.0 + 6, original.1 + 3));
    }
}

#[test]
fn numeric_read_chain_verifier_rejects_intervening_effects() {
    let (mut f, inst, slots) = guarded_fixture(false, false, 2);
    let g = Analysis::new(&f, allocator(1 << 20))
        .unwrap()
        .admit(&f, inst, slots, 0)
        .unwrap();
    let read = emit(&mut f, inst, slots, 0, g);
    let check = |f: &Function| {
        verify(
            f,
            &read,
            slots,
            &Predecessors::new(f, allocator(1 << 20)).unwrap(),
        )
    };
    check(&f).unwrap();
    for effect in 0..2 {
        let mut changed = f.clone();
        let tag = changed.dfg.first_result(g.tag);
        let convention = changed.signature.call_conv;
        let sig = changed.import_signature(Signature::new(convention));
        let mut c = FuncCursor::new(&mut changed);
        c.goto_inst(g.branch);
        if effect == 0 {
            c.ins().store(MemFlagsData::new(), tag, slots, 0);
        } else {
            c.ins().call_indirect(sig, slots, &[]);
        }
        valid(&changed);
        assert!(check(&changed).is_err());
    }
}

#[cfg(not(miri))]
#[test]
fn native_numeric_read_chains_preserve_bits_and_both_operand_guards() {
    for floating in [false, true] {
        let (mut f, slots) = paired_fixture(floating);
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
            crate::jit::abi::payload::check_native_numeric_pair(entry);
        })
        .unwrap();
    }
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
