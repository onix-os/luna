use super::*;

fn fixture(kind: Kind, index: u32) -> (Function, IrValue, Inst) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut frontend = FunctionBuilderContext::new();
    let (slots, load);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut frontend);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        slots = b.block_params(entry)[0];
        let (ty, offset) = match kind {
            Kind::Tag => (types::I64, index as i32 * 16),
            Kind::Bits(ty) => (ty, index as i32 * 16 + 8),
        };
        let bits = b.ins().load(ty, MemFlagsData::new(), slots, offset);
        load = b.func.dfg.value_def(bits).inst().unwrap();
        let bits = if ty == types::F64 {
            b.ins().bitcast(types::I64, MemFlagsData::new(), bits)
        } else if ty == types::I32 {
            b.ins().uextend(types::I64, bits)
        } else {
            bits
        };
        b.ins().return_(&[bits]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    (f, slots, load)
}

fn kinds() -> [Kind; 3] {
    [Kind::Tag, Kind::Bits(types::I64), Kind::Bits(types::F64)]
}

#[test]
fn packed_read_records_remain_valid_after_composed_block_splits() {
    let (mut source, slots, first) = fixture(Kind::Tag, 0);
    let block = source.layout.inst_block(first).unwrap();
    let end = source.layout.last_inst(block).unwrap();
    let initial = source.dfg.first_result(first);
    {
        let mut c = FuncCursor::new(&mut source);
        c.goto_inst(end);
        let mut result = initial;
        for (ty, offset) in [
            (types::I64, 8),
            (types::I64, 16),
            (types::F64, 24),
            (types::I64, 4080),
            (types::I64, 4088),
        ] {
            let value = c.ins().load(ty, MemFlagsData::new(), slots, offset);
            let value = if ty == types::F64 {
                c.ins().bitcast(types::I64, MemFlagsData::new(), value)
            } else {
                value
            };
            result = c.ins().bxor(result, value);
        }
        c.func.replace(end).return_(&[result]);
    }
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&source, &flags).unwrap();
    let inputs: Vec<_> = source
        .layout
        .block_insts(block)
        .filter(|&inst| source.dfg.insts[inst].opcode() == Opcode::Load)
        .map(|inst| admit(&source, inst, slots, 256).unwrap())
        .collect();
    assert_eq!(inputs.len(), 6);
    let mut f = source.clone();
    let reads: Vec<_> = inputs
        .into_iter()
        .map(|input| emit(&mut f, input))
        .collect();
    for read in &reads {
        verify_source(&source, read, slots, 256).unwrap();
        verify(&f, read).unwrap();
    }
    cranelift_codegen::verify_function(&f, &flags).unwrap();
}

#[test]
fn packed_reads_preserve_result_identity_and_exact_expansion_at_prefix_edges() {
    let flags = settings::Flags::new(settings::builder());
    for kind in kinds() {
        for index in [0, 1, 255] {
            let (source, slots, inst) = fixture(kind, index);
            cranelift_codegen::verify_function(&source, &flags).unwrap();
            let input = admit(&source, inst, slots, index as usize + 1).unwrap();
            let mut f = source.clone();
            let read = emit(&mut f, input);
            verify_source(&source, &read, slots, index as usize + 1).unwrap();
            verify(&f, &read).unwrap();
            cranelift_codegen::verify_function(&f, &flags).unwrap();
            let (instructions, blocks) = size(&source);
            let (added_i, added_b) = match kind {
                Kind::Tag => (20, 0),
                Kind::Bits(types::I64) => (16, 3),
                _ => (17, 3),
            };
            assert_eq!(size(&f), (instructions + added_i, blocks + added_b));
            assert!(admit(&source, inst, slots, index as usize).is_err());
            assert!(admit(&source, inst, slots, 257).is_err());
            let mut changed = source.clone();
            let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[inst] else {
                unreachable!()
            };
            *offset = (i32::from(*offset) + 16).into();
            cranelift_codegen::verify_function(&changed, &flags).unwrap();
            assert!(verify_source(&changed, &read, slots, 256).is_err());
        }
    }
}

#[test]
fn packed_read_admission_rejects_misaligned_negative_typed_or_flagged_loads() {
    for kind in kinds() {
        let (source, slots, inst) = fixture(kind, 0);
        for offset in [-16, -8, -1, 1, 7, 9, 4096, i32::MAX] {
            let mut f = source.clone();
            let InstructionData::Load { offset: actual, .. } = &mut f.dfg.insts[inst] else {
                unreachable!()
            };
            *actual = offset.into();
            assert!(admit(&f, inst, slots, 256).is_err());
        }
        let mut f = source.clone();
        let mut flags = MemFlagsData::new();
        flags.set_notrap();
        f.replace(inst).load(
            if kind == Kind::Bits(types::F64) {
                types::F64
            } else {
                types::I64
            },
            flags,
            slots,
            if kind == Kind::Tag { 0 } else { 8 },
        );
        assert!(admit(&f, inst, slots, 1).is_err());
        for (ty, offset) in [(types::I32, 8), (types::F64, 0)] {
            let (mut f, slots, inst) = fixture(Kind::Bits(ty), 0);
            let InstructionData::Load { offset: actual, .. } = &mut f.dfg.insts[inst] else {
                unreachable!()
            };
            *actual = offset.into();
            cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder()))
                .unwrap();
            assert!(admit(&f, inst, slots, 1).is_err());
        }
    }
}

fn instructions(f: &Function, read: &Read) -> Vec<Inst> {
    match read {
        Read::Tag(input) => {
            std::iter::successors(Some(input.inst), |&inst| f.layout.prev_inst(inst))
                .take(21)
                .collect()
        }
        Read::Bits {
            input,
            blocks: [before, _, numeric, immediate],
            ..
        } => {
            let mut result: Vec<_> = f
                .layout
                .block_insts(*before)
                .rev()
                .take(9)
                .chain(f.layout.block_insts(*numeric))
                .chain(f.layout.block_insts(*immediate))
                .collect();
            if input.kind == Kind::Bits(types::F64) {
                result.push(input.inst);
            }
            result
        }
    }
}

#[test]
fn packed_read_checker_rejects_well_formed_offset_mask_tag_branch_and_value_mutations() {
    let flags = settings::Flags::new(settings::builder());
    for kind in kinds() {
        let (source, slots, inst) = fixture(kind, 1);
        let input = admit(&source, inst, slots, 2).unwrap();
        let mut f = source.clone();
        let read = emit(&mut f, input);
        let mut rejected = 0;
        for inst in instructions(&f, &read) {
            let mut changed = f.clone();
            match &mut changed.dfg.insts[inst] {
                InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 8).into(),
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
                InstructionData::Ternary { .. } => changed.dfg.inst_args_mut(inst).swap(1, 2),
                InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                InstructionData::Jump { .. } => {
                    let InstructionData::Jump { destination, .. } = f.dfg.insts[inst] else {
                        unreachable!()
                    };
                    let block = destination.block(&f.dfg.value_lists);
                    changed.replace(inst).jump(block, &[slots.into()]);
                }
                InstructionData::Unary { opcode, .. } => *opcode = Opcode::Sextend,
                InstructionData::LoadNoOffset {
                    opcode: Opcode::Bitcast,
                    ..
                } => {
                    let value = changed.dfg.inst_args(inst)[0];
                    changed.replace(inst).fcvt_from_sint(types::F64, value);
                }
                _ => panic!(
                    "unexpected packed read instruction: {:?}",
                    changed.dfg.insts[inst]
                ),
            }
            cranelift_codegen::verify_function(&changed, &flags).unwrap();
            assert!(verify(&changed, &read).is_err());
            rejected += 1;
        }
        assert_eq!(
            rejected,
            match kind {
                Kind::Tag => 21,
                Kind::Bits(types::I64) => 17,
                _ => 18,
            }
        );
    }
}

#[cfg(not(miri))]
#[test]
fn generated_packed_reads_match_typed_values_mutations_and_invalid_encodings() {
    for kind in kinds() {
        for index in [0, 1, 255] {
            let (source, slots, inst) = fixture(kind, index);
            let input = admit(&source, inst, slots, index as usize + 1).unwrap();
            let mut f = source.clone();
            let read = emit(&mut f, input);
            verify_source(&source, &read, slots, index as usize + 1).unwrap();
            verify(&f, &read).unwrap();
            crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
                let entry = unsafe {
                    std::mem::transmute::<
                        *const u8,
                        unsafe extern "C" fn(
                            *const crate::jit::abi::payload::packed::Packed,
                        ) -> u64,
                    >(pointer)
                };
                crate::jit::abi::payload::packed::check_native_read(
                    entry,
                    index as usize,
                    kind == Kind::Tag,
                );
            })
            .unwrap();
        }
    }
}
