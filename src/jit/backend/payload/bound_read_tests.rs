use super::*;

fn allocator(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn valid(f: &Function) {
    cranelift_codegen::verify_function(f, &settings::Flags::new(settings::builder())).unwrap();
}

fn fixture(count: usize, tag: u64, floating: bool) -> (Function, IrValue, Vec<Inst>) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let slots;
    let mut reads = Vec::new();
    {
        let mut b = FunctionBuilder::new(&mut f, &mut context);
        let entry = b.create_block();
        let yes = b.create_block();
        let no = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        slots = b.block_params(entry)[0];
        let mut condition = b.ins().iconst(types::I8, 1);
        for index in 0..count {
            let actual = b
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, index as i32 * 16);
            let expected = b.ins().iconst(types::I64, tag as i64);
            let same = b.ins().icmp(IntCC::Equal, actual, expected);
            condition = b.ins().band(condition, same);
        }
        b.ins().brif(condition, yes, &[], no, &[]);
        b.switch_to_block(yes);
        let mut values = Vec::new();
        for index in 0..count {
            let bits = b.ins().load(
                if floating { types::F64 } else { types::I64 },
                MemFlagsData::new(),
                slots,
                index as i32 * 16 + 8,
            );
            reads.push(b.func.dfg.value_def(bits).inst().unwrap());
            values.push(bits);
        }
        let mut result = b.ins().iconst(types::I64, 0);
        for bits in values {
            let bits = if floating {
                b.ins().bitcast(types::I64, MemFlagsData::new(), bits)
            } else {
                bits
            };
            result = b.ins().bxor(result, bits);
        }
        b.ins().return_(&[result]);
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
    valid(&f);
    (f, slots, reads)
}

#[test]
fn exact_numeric_reads_preserve_expansion_and_workspace_refusal() {
    for count in [1, 5] {
        for tag in [abi::INTEGER, abi::NUMBER] {
            for floating in [false, true] {
                let (source, slots, _) = fixture(count, tag, floating);
                let original = size(&source);
                let lower = |f: &mut Function, allocator| {
                    lower_contract(
                        f,
                        slots,
                        slots,
                        count,
                        &[],
                        allocator,
                        crate::jit::work::Expansion {
                            instructions: 4096,
                            blocks: 4096,
                        },
                        Contract::FrameBound,
                    )
                };
                let mut f = source.clone();
                let memory = allocator(1 << 20);
                lower(&mut f, memory.clone()).unwrap();
                valid(&f);
                assert_eq!(size(&f), (original.0 + count, original.1));
                assert_eq!(memory.0.current(), 0);
                let peak = memory.0.peak();
                for limit in [0, peak - 1, peak] {
                    let ledger = allocator(limit);
                    let mut candidate = source.clone();
                    assert_eq!(lower(&mut candidate, ledger.clone()).is_ok(), limit == peak);
                    assert_eq!(ledger.0.current(), 0);
                    if limit < peak {
                        assert_eq!(candidate, source);
                    }
                }
            }
        }
    }
}

fn corrupt_guard(f: &mut Function, slots: IrValue, at: Inst, mutation: usize) {
    let entry = f.layout.entry_block().unwrap();
    let branch = f.layout.last_inst(entry).unwrap();
    let compare = f
        .layout
        .block_insts(entry)
        .find(|&i| f.dfg.insts[i].opcode() == Opcode::Icmp)
        .unwrap();
    match mutation {
        0 => {
            let InstructionData::IntCompare { cond, .. } = &mut f.dfg.insts[compare] else {
                unreachable!()
            };
            *cond = IntCC::NotEqual;
        }
        1 => {
            let InstructionData::Brif { blocks, .. } = &mut f.dfg.insts[branch] else {
                unreachable!()
            };
            blocks.swap(0, 1);
        }
        2 => {
            let inst = f
                .layout
                .block_insts(entry)
                .find(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
                .unwrap();
            let InstructionData::Load { offset, .. } = &mut f.dfg.insts[inst] else {
                unreachable!()
            };
            *offset = 16.into();
        }
        3 => {
            let inst = f.layout.prev_inst(branch).unwrap();
            let InstructionData::Binary { opcode, .. } = &mut f.dfg.insts[inst] else {
                unreachable!()
            };
            *opcode = Opcode::Bor;
        }
        4 => {
            let value = f.dfg.inst_args(compare)[1];
            let inst = f.dfg.value_def(value).inst().unwrap();
            f.replace(inst).iconst(types::I64, abi::NIL as i64);
        }
        5 | 6 => {
            let mut c = FuncCursor::new(f);
            c.goto_inst(if mutation == 5 { branch } else { at });
            let value = c.ins().iconst(types::I64, 0);
            c.ins().store(MemFlagsData::new(), value, slots, 0);
        }
        _ => {
            let InstructionData::Brif { blocks, .. } = f.dfg.insts[branch] else {
                unreachable!()
            };
            let yes = blocks[0].block(&f.dfg.value_lists);
            let no = blocks[1].block(&f.dfg.value_lists);
            let end = f.layout.last_inst(no).unwrap();
            f.replace(end).jump(yes, &[]);
        }
    }
}

#[test]
fn exact_numeric_reads_recheck_source_and_emitted_corruptions() {
    for tag in [abi::INTEGER, abi::NUMBER] {
        for floating in [false, true] {
            let (source, slots, insts) = fixture(1, tag, floating);
            let inst = insts[0];
            let mut analysis = same_tag::Analysis::new(&source, allocator(1 << 20)).unwrap();
            assert!(analysis.admit_numeric(&source, slots, inst, 0));
            let mut f = source.clone();
            let read = emit(&mut f, inst, slots, 0);
            verify_source(&source, &read, slots, &mut analysis).unwrap();
            let check = |f: &Function, budget: usize| {
                valid(f);
                verify(
                    f,
                    &read,
                    slots,
                    &crate::jit::preds::Predecessors::new(f, allocator(1 << 20)).unwrap(),
                    &mut { budget },
                )
            };
            check(&f, 4096).unwrap();
            assert!(check(&f, 0).is_err());
            let mut changed = source.clone();
            let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[inst] else {
                unreachable!()
            };
            *offset = 24.into();
            valid(&changed);
            assert!(verify_source(&changed, &read, slots, &mut analysis).is_err());
            for mutation in 0..8 {
                let mut changed = source.clone();
                corrupt_guard(&mut changed, slots, inst, mutation);
                valid(&changed);
                let mut analysis = same_tag::Analysis::new(&changed, allocator(1 << 20)).unwrap();
                assert!(!analysis.admit_numeric(&changed, slots, inst, 0));
                assert!(verify_source(&changed, &read, slots, &mut analysis).is_err());
                let mut changed = f.clone();
                corrupt_guard(&mut changed, slots, read.pointer, mutation);
                assert!(check(&changed, 4096).is_err(), "guard mutation={mutation}");
            }
            for mutation in 0..5 {
                let mut changed = f.clone();
                match mutation {
                    0 => {
                        let InstructionData::Load { offset, .. } =
                            &mut changed.dfg.insts[read.pointer]
                        else {
                            unreachable!()
                        };
                        *offset = 0.into();
                    }
                    1 => changed.dfg.inst_args_mut(read.inst)[0] = slots,
                    2 => {
                        let InstructionData::Load { offset, .. } =
                            &mut changed.dfg.insts[read.inst]
                        else {
                            unreachable!()
                        };
                        *offset = 8.into();
                    }
                    3 => {
                        changed.replace(read.pointer).iconst(types::I64, 1);
                    }
                    _ => {
                        let mut c = FuncCursor::new(&mut changed);
                        c.goto_inst(read.inst);
                        c.ins().iconst(types::I64, 0);
                    }
                }
                assert!(check(&changed, 4096).is_err(), "read mutation={mutation}");
            }
        }
    }
}

#[cfg(not(miri))]
#[test]
fn exact_numeric_native_reads_preserve_bits_and_reject_other_tags() {
    for tag in [abi::INTEGER, abi::NUMBER] {
        for floating in [false, true] {
            let (mut f, slots, _) = fixture(1, tag, floating);
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
                crate::jit::abi::payload::runtime::check_native_bound_numeric_read(
                    entry,
                    Some(tag),
                );
            })
            .unwrap();
        }
    }
}

#[cfg(not(miri))]
#[test]
fn bound_numeric_union_native_reads_preserve_bits_and_reject_other_tags() {
    for floating in [false, true] {
        let (mut f, slots, _) = fixture(1, abi::INTEGER, floating);
        let entry = f.layout.entry_block().unwrap();
        let compare = f
            .layout
            .block_insts(entry)
            .find(|&i| f.dfg.insts[i].opcode() == Opcode::Icmp)
            .unwrap();
        let InstructionData::IntCompare { args, .. } = f.dfg.insts[compare] else {
            unreachable!()
        };
        let mut c = FuncCursor::new(&mut f);
        c.goto_inst(compare);
        let mask = c.ins().iconst(types::I64, -2);
        let numeric = c.ins().band(args[0], mask);
        c.func.replace(compare).icmp(IntCC::Equal, numeric, args[1]);
        let original = size(&f);
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
        valid(&f);
        assert_eq!(size(&f), (original.0 + 1, original.1));
        crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
            let entry = unsafe {
                std::mem::transmute::<
                    *const u8,
                    unsafe extern "C" fn(*const crate::jit::abi::payload::Payload) -> u64,
                >(pointer)
            };
            crate::jit::abi::payload::runtime::check_native_bound_numeric_read(entry, None);
        })
        .unwrap();
    }
}
