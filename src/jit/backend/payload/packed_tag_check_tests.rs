use super::*;

fn fixture(kind: Kind, index: usize) -> (Function, IrValue, Inst) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut frontend = FunctionBuilderContext::new();
    let (slots, inst);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut frontend);
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        slots = b.block_params(block)[0];
        let tag = b
            .ins()
            .load(types::I64, MemFlagsData::new(), slots, (index * 16) as i32);
        let (selected, expected) = match kind {
            Kind::Numeric => {
                let mask = b.ins().iconst(types::I64, -2);
                (b.ins().band(tag, mask), abi::INTEGER)
            }
            Kind::Exact(kind) => (tag, kind),
        };
        let expected = b.ins().iconst(types::I64, expected as i64);
        let test = b.ins().icmp(IntCC::Equal, selected, expected);
        inst = b.func.dfg.value_def(test).inst().unwrap();
        let result = b.ins().uextend(types::I64, test);
        b.ins().return_(&[result]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    (f, slots, inst)
}

fn kinds() -> impl Iterator<Item = Kind> {
    (abi::NIL..=abi::REFERENCE)
        .map(Kind::Exact)
        .chain([Kind::Numeric])
}

fn lower(source: &Function, slots: IrValue, inst: Inst, count: usize) -> (Function, Check) {
    let test = super::source(source, inst, slots, count).unwrap();
    let input = packed_reads::admit(source, test.tag, slots, count).unwrap();
    let mut f = source.clone();
    let read = packed_reads::emit(&mut f, input);
    let snapshot = packed_reads::tag_snapshot(&f, &read).unwrap();
    let check = emit(&mut f, test, snapshot.1);
    packed_reads::verify(&f, &read).unwrap();
    check.verify_source(source, slots, count, snapshot).unwrap();
    check.fragment(&f).unwrap();
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    (f, check)
}

#[test]
fn packed_tag_checks_bind_source_and_exact_pure_fragments() {
    for kind in kinds() {
        for index in [0, 1, 255] {
            let (source, slots, inst) = fixture(kind, index);
            let test = super::source(&source, inst, slots, index + 1).unwrap();
            assert!(super::source(&source, inst, slots, index).is_none());
            let (f, check) = lower(&source, slots, inst, index + 1);
            assert_eq!(
                size(&f),
                (size(&source).0 + 12 + test.added(), size(&source).1)
            );
            let mut changed = source.clone();
            let InstructionData::IntCompare { cond, .. } = &mut changed.dfg.insts[inst] else {
                unreachable!()
            };
            *cond = IntCC::NotEqual;
            assert!(check
                .verify_source(&changed, slots, index + 1, (test.tag, check.packed))
                .is_err());
            assert!(check
                .verify_source(&source, slots, index + 1, (test.tag, slots))
                .is_err());
            let mut changed = source.clone();
            let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[test.tag] else {
                unreachable!()
            };
            *offset = (if index == 0 { 16 } else { 0 }).into();
            assert!(check
                .verify_source(&changed, slots, 256, (test.tag, check.packed))
                .is_err());
            let fragment = check.fragment(&f).unwrap();
            let mut at = fragment.first;
            let mut mutations = 0;
            loop {
                let mut changed = f.clone();
                match &mut changed.dfg.insts[at] {
                    InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                    InstructionData::IntCompare { cond, .. } => {
                        *cond = if *cond == IntCC::Equal {
                            IntCC::NotEqual
                        } else {
                            IntCC::Equal
                        }
                    }
                    InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bor,
                    _ => unreachable!(),
                }
                cranelift_codegen::verify_function(
                    &changed,
                    &settings::Flags::new(settings::builder()),
                )
                .unwrap();
                assert!(check.fragment(&changed).is_err());
                mutations += 1;
                if at == inst {
                    break;
                }
                at = f.layout.next_inst(at).unwrap();
            }
            assert_eq!(mutations, 1 + test.added());
        }
    }
}

#[cfg(not(miri))]
#[test]
fn packed_tag_checks_match_all_descriptor_classes_and_pointer_bits() {
    for kind in kinds() {
        for index in [0, 1, 255] {
            let (source, slots, inst) = fixture(kind, index);
            let (f, _) = lower(&source, slots, inst, index + 1);
            crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
                let entry: unsafe extern "C" fn(
                    *const crate::jit::abi::payload::packed::Packed,
                ) -> u64 = unsafe { std::mem::transmute(pointer) };
                crate::jit::abi::payload::packed::scoped::read_oracle::check_native_tag_test(
                    entry,
                    index,
                    match kind {
                        Kind::Exact(value) => Some(value),
                        Kind::Numeric => None,
                    },
                );
            })
            .unwrap();
        }
    }
}
