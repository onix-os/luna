use super::*;

fn allocator() -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(1 << 20))
}

fn fixture(tag_value: u64, floating: bool) -> (Function, IrValue, IrValue, IrValue, Inst, Inst) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.extend([AbiParam::new(types::I64); 3]);
    let mut frontend = FunctionBuilderContext::new();
    let (slots, host, tag, store, guard);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut frontend);
        let entry = b.create_block();
        let yes = b.create_block();
        let no = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let bits;
        [slots, host, bits] = b.block_params(entry).try_into().unwrap();
        let loaded = b.ins().load(types::I64, MemFlagsData::new(), slots, 0);
        tag = b.ins().iconst(types::I64, tag_value as i64);
        let same = b.ins().icmp(IntCC::Equal, loaded, tag);
        guard = b.ins().brif(same, yes, &[], no, &[]);
        b.switch_to_block(yes);
        let bits = b.ins().bor(bits, bits);
        let bits = if floating {
            b.ins().bitcast(types::F64, MemFlagsData::new(), bits)
        } else {
            bits
        };
        store = b.ins().store(MemFlagsData::new(), tag, slots, 0);
        b.ins().store(MemFlagsData::new(), bits, slots, 8);
        b.ins().return_(&[]);
        b.switch_to_block(no);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(
            cranelift_codegen::isa::lookup_by_name("x86_64")
                .unwrap()
                .finish(settings::Flags::new(settings::builder()))
                .unwrap()
                .frontend_config(),
        );
    }
    (f, slots, host, tag, store, guard)
}

#[test]
fn bound_rewrite_has_exact_expansion_and_no_host_copy_without_helpers() {
    for tag in [abi::NIL, abi::BOOLEAN, abi::INTEGER, abi::NUMBER] {
        for floating in [false, true] {
            let (source, slots, host, _, _, _) = fixture(tag, floating);
            let original = size(&source);
            let mut f = source.clone();
            let allocator = allocator();
            let count = lower_contract(
                &mut f,
                slots,
                host,
                1,
                &[],
                allocator.clone(),
                crate::jit::work::Expansion {
                    instructions: 4096,
                    blocks: 4096,
                },
                Contract::FrameBound,
            )
            .unwrap();
            assert_eq!(allocator.0.current(), 0);
            assert!(allocator.0.peak() > 0);
            let peak = allocator.0.peak();
            for limit in [peak - 1, peak] {
                let ledger = BudgetAllocator(crate::jit::resources::Ledger::new(limit));
                let mut candidate = source.clone();
                let result = lower_contract(
                    &mut candidate,
                    slots,
                    host,
                    1,
                    &[],
                    ledger.clone(),
                    crate::jit::work::Expansion {
                        instructions: 4096,
                        blocks: 4096,
                    },
                    Contract::FrameBound,
                );
                assert_eq!(result.is_ok(), limit == peak);
                assert_eq!(ledger.0.current(), 0);
                if result.is_err() {
                    assert_eq!(candidate, source);
                }
            }
            cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder()))
                .unwrap();
            let bound = matches!(tag, abi::INTEGER | abi::NUMBER);
            assert_eq!(count, usize::from(bound));
            if bound {
                assert_eq!(
                    size(&f),
                    (original.0 + 1 + usize::from(floating), original.1 + 1)
                );
                assert!(f.sized_stack_slots.is_empty());
                assert!(!f
                    .layout
                    .blocks()
                    .flat_map(|b| f.layout.block_insts(b))
                    .any(|i| f.dfg.insts[i].opcode().is_call()));
            } else {
                assert!(!f.sized_stack_slots.is_empty());
            }
            let mut refused = source.clone();
            let ledger = BudgetAllocator(crate::jit::resources::Ledger::new(0));
            assert!(lower_contract(
                &mut refused,
                slots,
                host,
                1,
                &[],
                ledger.clone(),
                crate::jit::work::Expansion {
                    instructions: 4096,
                    blocks: 4096
                },
                Contract::FrameBound
            )
            .is_err());
            assert_eq!(refused, source);
            assert_eq!(ledger.0.current(), 0);
        }
    }
}

#[test]
fn bound_write_checker_refuses_well_formed_pointer_store_and_guard_corruption() {
    for tag_value in [abi::INTEGER, abi::NUMBER] {
        for floating in [false, true] {
            let (source, slots, host, tag, tag_store, guard) = fixture(tag_value, floating);
            let bits_store = source.layout.next_inst(tag_store).unwrap();
            let bits = source.dfg.inst_args(bits_store)[0];
            let proof = same_tag::Analysis::new(&source, allocator())
                .unwrap()
                .admit(&source, slots, tag_store, 0, tag)
                .unwrap();
            let mut f = source.clone();
            let write = emit(&mut f, tag_store, bits_store, slots, 0, tag, bits, proof);
            verify_source(
                &source,
                &write,
                &mut same_tag::Analysis::new(&source, allocator()).unwrap(),
            )
            .unwrap();
            let check = |f: &Function| {
                cranelift_codegen::verify_function(f, &settings::Flags::new(settings::builder()))
                    .unwrap();
                let preds = crate::jit::preds::Predecessors::new(f, allocator()).unwrap();
                verify(f, &write, &preds, &mut 4096)
            };
            check(&f).unwrap();
            let [end, store, pointer] =
                shape(f.layout.block_insts(write.blocks[0]).rev().take(3)).unwrap();
            for mutation in 0..10 {
                let mut changed = f.clone();
                match mutation {
                    0 => {
                        changed.dfg.inst_args_mut(pointer)[0] = host;
                    }
                    1 => {
                        let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[pointer]
                        else {
                            unreachable!()
                        };
                        *offset = 0.into();
                    }
                    2 => {
                        changed.dfg.inst_args_mut(store)[0] = tag;
                    }
                    3 => {
                        changed.dfg.inst_args_mut(store)[1] = slots;
                    }
                    4 => {
                        let InstructionData::Store { offset, .. } = &mut changed.dfg.insts[store]
                        else {
                            unreachable!()
                        };
                        *offset = 8.into();
                    }
                    5 => {
                        let InstructionData::Brif { blocks, .. } = &mut changed.dfg.insts[guard]
                        else {
                            unreachable!()
                        };
                        blocks.swap(0, 1);
                    }
                    6 => {
                        let mut c = FuncCursor::new(&mut changed);
                        c.goto_inst(pointer);
                        c.ins().store(MemFlagsData::new(), tag, slots, 0);
                    }
                    7 => {
                        changed.replace(end).return_(&[]);
                    }
                    8 => {
                        let mut c = FuncCursor::new(&mut changed);
                        c.goto_inst(guard);
                        c.ins().store(MemFlagsData::new(), tag, slots, 0);
                    }
                    _ => {
                        let def = changed.dfg.value_def(tag).inst().unwrap();
                        changed.replace(def).iconst(types::I64, abi::BOOLEAN as i64);
                    }
                }
                assert!(
                    check(&changed).is_err(),
                    "tag={tag_value} floating={floating} mutation={mutation}"
                );
            }
            let preds = crate::jit::preds::Predecessors::new(&f, allocator()).unwrap();
            assert!(verify(&f, &write, &preds, &mut 0).is_err());
        }
    }
}
