use super::*;
use crate::jit::abi::payload::packed::scoped::bridge::{entry, store_oracle};

fn fixture(float: bool, index: u32, live: bool) -> (Function, IrValue, IrValue, Inst) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64; 5].map(AbiParam::new));
    f.signature.returns.push(AbiParam::new(types::I32));
    let mut frontend = FunctionBuilderContext::new();
    let (slots, host, store);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut frontend);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let tag;
        let bits;
        let marker;
        [slots, host, tag, bits, marker] = b.block_params(entry).try_into().unwrap();
        let bits = if float {
            b.ins().bitcast(types::F64, MemFlagsData::new(), bits)
        } else {
            bits
        };
        store = b
            .ins()
            .store(MemFlagsData::new(), tag, slots, index as i32 * 16);
        b.ins()
            .store(MemFlagsData::new(), bits, slots, index as i32 * 16 + 8);
        if live {
            b.ins()
                .load(types::I64, MemFlagsData::new(), slots, index as i32 * 16);
        }
        let value = b.ins().iconst(types::I64, 999);
        b.ins().store(MemFlagsData::new(), value, marker, 0);
        let done = b.ins().iconst(types::I32, entry::RETURNED as i64);
        b.ins().return_(&[done]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    (f, slots, host, store)
}

fn allocator() -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(1024 * 1024))
}

#[test]
fn packed_stores_compose_with_scalar_reads_and_a_terminal_copy() {
    for float in [false, true] {
        let (mut source, slots, host, first) = fixture(float, 0, false);
        let bits_store = source.layout.next_inst(first).unwrap();
        let next = source.layout.next_inst(bits_store).unwrap();
        let (tag_read, bits_read, second);
        {
            let mut c = FuncCursor::new(&mut source);
            c.goto_inst(next);
            let tag = c.ins().load(types::I64, MemFlagsData::new(), slots, 0);
            tag_read = c.func.dfg.value_def(tag).inst().unwrap();
            let bits = c.ins().load(types::I64, MemFlagsData::new(), slots, 8);
            bits_read = c.func.dfg.value_def(bits).inst().unwrap();
            second = c.ins().store(MemFlagsData::new(), tag, slots, 16);
            c.ins().store(MemFlagsData::new(), bits, slots, 24);
        }
        let first_input = admit(&source, first, slots, host, 2, false).unwrap();
        let tag_input = packed_reads::admit(&source, tag_read, slots, 2).unwrap();
        let bits_input = packed_reads::admit(&source, bits_read, slots, 2).unwrap();
        let second_input = admit(&source, second, slots, host, 2, true).unwrap();
        let mut f = source.clone();
        let first = emit(&mut f, first_input);
        let tag = packed_reads::emit(&mut f, tag_input);
        let bits = packed_reads::emit(&mut f, bits_input);
        let second = emit(&mut f, second_input);
        let mut proof = terminal::Analysis::new(&source, slots, host, 2, &[], allocator()).unwrap();
        verify_source(&source, &first, slots, host, 2, &mut proof).unwrap();
        verify_source(&source, &second, slots, host, 2, &mut proof).unwrap();
        packed_reads::verify_source(&source, &tag, slots, 2).unwrap();
        packed_reads::verify_source(&source, &bits, slots, 2).unwrap();
        verify(&f, &first).unwrap();
        verify(&f, &second).unwrap();
        packed_reads::verify(&f, &tag).unwrap();
        packed_reads::verify(&f, &bits).unwrap();
        cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
        #[cfg(not(miri))]
        crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
            let entry = unsafe { std::mem::transmute::<*const u8, store_oracle::Entry>(pointer) };
            store_oracle::check_native_copy(entry);
        })
        .unwrap();
    }
}

#[test]
fn packed_store_lowering_preserves_pair_identity_and_exact_expansion() {
    let flags = settings::Flags::new(settings::builder());
    for float in [false, true] {
        for index in [0, 1, 255] {
            for terminal in [false, true] {
                let (source, slots, host, inst) = fixture(float, index, false);
                cranelift_codegen::verify_function(&source, &flags).unwrap();
                let input =
                    admit(&source, inst, slots, host, index as usize + 1, terminal).unwrap();
                let mut f = source.clone();
                let write = emit(&mut f, input);
                let mut proof = terminal::Analysis::new(
                    &source,
                    slots,
                    host,
                    index as usize + 1,
                    &[],
                    allocator(),
                )
                .unwrap();
                verify_source(&source, &write, slots, host, index as usize + 1, &mut proof)
                    .unwrap();
                verify(&f, &write).unwrap();
                cranelift_codegen::verify_function(&f, &flags).unwrap();
                let (instructions, blocks) = size(&source);
                assert_eq!(
                    size(&f),
                    (instructions + 24 + usize::from(float), blocks + 4)
                );
            }
        }
    }
}

#[test]
fn packed_terminal_store_requires_matching_source_liveness_proof() {
    for float in [false, true] {
        let (dead, slots, host, inst) = fixture(float, 0, false);
        let (live, _, _, _) = fixture(float, 0, true);
        let input = admit(&dead, inst, slots, host, 1, true).unwrap();
        let mut f = dead.clone();
        let write = emit(&mut f, input);
        let mut proof = terminal::Analysis::new(&dead, slots, host, 1, &[], allocator()).unwrap();
        assert!(verify_source(&live, &write, slots, host, 1, &mut proof).is_err());
        let mut proof = terminal::Analysis::new(&live, slots, host, 1, &[], allocator()).unwrap();
        assert!(verify_source(&live, &write, slots, host, 1, &mut proof).is_err());
        let input = admit(&live, inst, slots, host, 1, false).unwrap();
        let mut f = live.clone();
        let write = emit(&mut f, input);
        verify_source(&live, &write, slots, host, 1, &mut proof).unwrap();
        verify(&f, &write).unwrap();
    }
}

#[test]
fn packed_store_admission_rejects_bad_prefix_offsets_flags_and_nonadjacent_pairs() {
    for float in [false, true] {
        let (source, slots, host, inst) = fixture(float, 0, false);
        assert!(admit(&source, inst, slots, host, 0, false).is_err());
        assert!(admit(&source, inst, slots, host, 257, false).is_err());
        assert!(admit(&source, inst, host, slots, 1, false).is_err());
        for offset in [-16, -1, 1, 8, 16, i32::MAX] {
            let mut f = source.clone();
            let InstructionData::Store { offset: actual, .. } = &mut f.dfg.insts[inst] else {
                unreachable!()
            };
            *actual = offset.into();
            assert!(admit(&f, inst, slots, host, 1, false).is_err());
        }
        let second = source.layout.next_inst(inst).unwrap();
        for mode in 0..4 {
            let mut f = source.clone();
            let target = if mode == 0 { inst } else { second };
            let InstructionData::Store { args, offset, .. } = f.dfg.insts[target] else {
                unreachable!()
            };
            let mut flags = MemFlagsData::new();
            if mode < 2 {
                flags.set_notrap();
            }
            match mode {
                0 | 1 => {
                    f.replace(target).store(flags, args[0], args[1], offset);
                }
                2 => {
                    f.replace(target).store(flags, args[0], args[1], 0);
                }
                _ => {
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_inst(second);
                    c.ins().iconst(types::I64, 42);
                }
            }
            assert!(admit(&f, inst, slots, host, 1, false).is_err());
        }
    }
}

#[test]
fn packed_store_checker_rejects_well_formed_guard_pointer_gateway_and_failure_mutations() {
    let flags = settings::Flags::new(settings::builder());
    for float in [false, true] {
        for terminal in [false, true] {
            let (source, slots, host, inst) = fixture(float, 1, false);
            let input = admit(&source, inst, slots, host, 2, terminal).unwrap();
            let mut f = source.clone();
            let write = emit(&mut f, input);
            let [before, _, fast, slow, failed] = write.blocks;
            let instructions: Vec<_> = f
                .layout
                .block_insts(before)
                .rev()
                .take(13 + usize::from(float))
                .chain(
                    [fast, slow, failed]
                        .into_iter()
                        .flat_map(|block| f.layout.block_insts(block)),
                )
                .collect();
            let mut rejected = 0;
            for inst in instructions {
                let mut changed = f.clone();
                match &mut changed.dfg.insts[inst] {
                    InstructionData::Load { offset, .. }
                    | InstructionData::Store { offset, .. } => {
                        *offset = (i32::from(*offset) + 8).into()
                    }
                    InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                    InstructionData::IntCompare { cond, .. } => {
                        *cond = if *cond == IntCC::Equal {
                            IntCC::NotEqual
                        } else {
                            IntCC::Equal
                        }
                    }
                    InstructionData::Binary { opcode, .. } => {
                        *opcode = if *opcode == Opcode::Band {
                            Opcode::Bor
                        } else {
                            Opcode::Band
                        }
                    }
                    InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                    InstructionData::Jump { .. } => {
                        changed.replace(inst).jump(slow, &[]);
                    }
                    InstructionData::CallIndirect { .. } => {
                        changed.dfg.inst_args_mut(inst).swap(3, 4)
                    }
                    InstructionData::MultiAry {
                        opcode: Opcode::Return,
                        ..
                    } => {
                        let success = f.layout.block_insts(slow).nth(5).unwrap();
                        changed.dfg.inst_args_mut(inst)[0] = f.dfg.first_result(success);
                    }
                    InstructionData::LoadNoOffset {
                        opcode: Opcode::Bitcast,
                        ..
                    } => {
                        changed.replace(inst).fcvt_to_sint(types::I64, input.bits);
                    }
                    _ => panic!("unexpected packed store instruction"),
                }
                cranelift_codegen::verify_function(&changed, &flags).unwrap();
                assert!(verify(&changed, &write).is_err());
                rejected += 1;
            }
            assert_eq!(rejected, 26 + usize::from(float));
        }
    }
}

#[cfg(not(miri))]
#[test]
fn generated_packed_stores_match_canonical_values_retirement_and_failure_effects() {
    for float in [false, true] {
        for index in [0, 1, 255] {
            for terminal in [false, true] {
                let (source, slots, host, inst) = fixture(float, index, false);
                let input =
                    admit(&source, inst, slots, host, index as usize + 1, terminal).unwrap();
                let mut f = source.clone();
                let write = emit(&mut f, input);
                let mut proof = terminal::Analysis::new(
                    &source,
                    slots,
                    host,
                    index as usize + 1,
                    &[],
                    allocator(),
                )
                .unwrap();
                verify_source(&source, &write, slots, host, index as usize + 1, &mut proof)
                    .unwrap();
                verify(&f, &write).unwrap();
                crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
                    let entry =
                        unsafe { std::mem::transmute::<*const u8, store_oracle::Entry>(pointer) };
                    store_oracle::check_native(entry, index as usize, terminal);
                })
                .unwrap();
            }
        }
    }
}
