use super::*;

#[test]
fn numeric_read_proofs_refuse_exhausted_admission_and_verification_work() {
    for tag in [abi::INTEGER, abi::NUMBER] {
        let fixture = fixture(tag, true);
        let f = &fixture.function;
        let mut analysis = Analysis::new(f, allocator()).unwrap();
        assert!(analysis.admit_numeric(f, fixture.slots, fixture.store, 0));
        assert!(analysis.verify_numeric(f, fixture.slots, fixture.store, 0));
        analysis.remaining = 0;
        assert!(!analysis.admit_numeric(f, fixture.slots, fixture.store, 0));
        assert!(analysis.verify_numeric(f, fixture.slots, fixture.store, 0));
        analysis.verification_remaining = 0;
        assert!(!analysis.verify_numeric(f, fixture.slots, fixture.store, 0));
    }
}

struct Fixture {
    function: Function,
    slots: IrValue,
    host: IrValue,
    tag: IrValue,
    store: Inst,
    guard: Inst,
    yes: Block,
    no: Block,
}

fn fixture(tag: u64, conjunction: bool) -> Fixture {
    let mut function = Function::new();
    function.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    function
        .signature
        .params
        .extend([AbiParam::new(types::I64); 3]);
    function.signature.returns.push(AbiParam::new(types::I64));
    let mut frontend = FunctionBuilderContext::new();
    let (slots, host, expected, store, guard, yes, no);
    {
        let mut b = FunctionBuilder::new(&mut function, &mut frontend);
        let entry = b.create_block();
        yes = b.create_block();
        no = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let bits;
        [slots, host, bits] = b.block_params(entry).try_into().unwrap();
        let actual = b.ins().load(types::I64, MemFlagsData::new(), slots, 0);
        expected = b.ins().iconst(types::I64, tag as i64);
        let condition = b.ins().icmp(IntCC::Equal, actual, expected);
        let condition = if conjunction {
            b.ins().band(condition, condition)
        } else {
            condition
        };
        guard = b.ins().brif(condition, yes, &[], no, &[]);
        b.switch_to_block(yes);
        let bits = b.ins().bor(bits, bits);
        store = b.ins().store(MemFlagsData::new(), expected, slots, 0);
        b.ins().store(MemFlagsData::new(), bits, slots, 8);
        let one = b.ins().iconst(types::I64, 1);
        b.ins().return_(&[one]);
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
    Fixture {
        function,
        slots,
        host,
        tag: expected,
        store,
        guard,
        yes,
        no,
    }
}

fn allocator() -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(1 << 20))
}

fn admitted(f: &Function, fixture: &Fixture) -> bool {
    cranelift_codegen::verify_function(f, &settings::Flags::new(settings::builder())).unwrap();
    Analysis::new(f, allocator())
        .unwrap()
        .admit(f, fixture.slots, fixture.store, 0, fixture.tag)
        .is_some()
}

#[test]
fn tag_proofs_require_fresh_equal_tags_on_every_incoming_path() {
    for tag in [abi::NIL, abi::BOOLEAN, abi::INTEGER, abi::NUMBER] {
        for conjunction in [false, true] {
            let fixture = fixture(tag, conjunction);
            assert!(admitted(&fixture.function, &fixture));
            for effect in 0..8 {
                let mut f = fixture.function.clone();
                match effect {
                    0 => {
                        let InstructionData::Brif { blocks, .. } = &mut f.dfg.insts[fixture.guard]
                        else {
                            unreachable!()
                        };
                        blocks.swap(0, 1);
                    }
                    1 => {
                        let end = f.layout.last_inst(fixture.no).unwrap();
                        f.replace(end).jump(fixture.yes, &[]);
                    }
                    2 | 3 => {
                        let mut c = FuncCursor::new(&mut f);
                        c.goto_inst(if effect == 2 {
                            fixture.guard
                        } else {
                            fixture.store
                        });
                        c.ins()
                            .store(MemFlagsData::new(), fixture.tag, fixture.slots, 0);
                    }
                    4 | 5 => {
                        let convention = f.signature.call_conv;
                        let signature = f.import_signature(Signature::new(convention));
                        let mut c = FuncCursor::new(&mut f);
                        c.goto_inst(if effect == 4 {
                            fixture.guard
                        } else {
                            fixture.store
                        });
                        c.ins().call_indirect(signature, fixture.host, &[]);
                    }
                    6 => {
                        let InstructionData::Store { offset, .. } = &mut f.dfg.insts[fixture.store]
                        else {
                            unreachable!()
                        };
                        *offset = 16.into();
                    }
                    _ => {
                        let InstructionData::Brif { blocks, .. } = &mut f.dfg.insts[fixture.guard]
                        else {
                            unreachable!()
                        };
                        blocks[1] = blocks[0];
                    }
                }
                assert!(
                    !admitted(&f, &fixture),
                    "tag={tag} conjunction={conjunction} effect={effect}"
                );
            }
            let mut proof = Analysis::new(&fixture.function, allocator()).unwrap();
            proof.remaining = 0;
            assert!(proof
                .admit(
                    &fixture.function,
                    fixture.slots,
                    fixture.store,
                    0,
                    fixture.tag
                )
                .is_none());
        }
    }
}

#[test]
fn tag_proofs_recheck_source_and_release_bounded_graph_storage() {
    let fixture = fixture(abi::INTEGER, true);
    let ledger = allocator();
    let peak;
    {
        let mut analysis = Analysis::new(&fixture.function, ledger.clone()).unwrap();
        let proof = analysis
            .admit(
                &fixture.function,
                fixture.slots,
                fixture.store,
                0,
                fixture.tag,
            )
            .unwrap();
        assert!(analysis.verify(&fixture.function, fixture.slots, proof));
        let mut changed = fixture.function.clone();
        let InstructionData::Brif { blocks, .. } = &mut changed.dfg.insts[fixture.guard] else {
            unreachable!()
        };
        blocks.swap(0, 1);
        assert!(!Analysis::new(&changed, allocator()).unwrap().verify(
            &changed,
            fixture.slots,
            proof
        ));
        assert!(!proof.matches(1, fixture.tag));
        peak = ledger.0.peak();
        assert!(peak > 0);
    }
    assert_eq!(ledger.0.current(), 0);
    let limited = BudgetAllocator(crate::jit::resources::Ledger::new(peak - 1));
    assert!(Analysis::new(&fixture.function, limited.clone()).is_err());
    assert_eq!(limited.0.current(), 0);
}

#[test]
fn proven_tag_lowering_removes_reload_without_removing_pointer_guards() {
    for conjunction in [false, true] {
        let fixture = fixture(abi::INTEGER, conjunction);
        let mut f = fixture.function.clone();
        lower(
            &mut f,
            fixture.slots,
            fixture.host,
            1,
            &[],
            allocator(),
            crate::jit::work::Expansion {
                instructions: 4096,
                blocks: 4096,
            },
        )
        .unwrap();
        cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
        let self_tags = f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).filter(|&inst| {
            matches!(f.dfg.insts[inst], InstructionData::Binary { opcode: Opcode::Bor, args } if args == [fixture.tag, fixture.tag])
        }).count();
        assert_eq!(self_tags, 1);
        assert_eq!(
            f.layout
                .blocks()
                .filter(|&block| f.layout.is_cold(block))
                .count(),
            1
        );
    }
}

#[cfg(not(miri))]
#[test]
fn original_integer_loop_contains_proven_tag_writes() {
    crate::Lua::empty().enter(|ctx| {
        let closure =
            crate::Closure::load(ctx, None, b"local s=0 for i=1,120 do s=s+i end return s")
                .unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(8 * 1024 * 1024));
        let code = compile_in(
            &snapshot,
            total.clone(),
            1024 * 1024,
            allocator(),
            crate::jit::work::Limits::from(&crate::jit::JitConfig::default()),
            Failure::RequirePayloadTags,
        )
        .unwrap();
        assert!(code.entry_kind.is_payload());
        drop(code);
        assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
    });
}

#[test]
fn emitted_tag_proofs_reject_guard_and_substitution_corruption() {
    let fixture = fixture(abi::INTEGER, true);
    let source = &fixture.function;
    let proof = Analysis::new(source, allocator())
        .unwrap()
        .admit(source, fixture.slots, fixture.store, 0, fixture.tag)
        .unwrap();
    let mut f = source.clone();
    let bits_store = f.layout.next_inst(fixture.store).unwrap();
    let bits = f.dfg.inst_args(bits_store)[0];
    let mut signature = Signature::new(f.signature.call_conv);
    signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let signature = f.import_signature(signature);
    let write = writes::emit(
        &mut f,
        fixture.store,
        bits_store,
        writes::Input {
            slots: fixture.slots,
            host: fixture.host,
            index: 0,
            tag: fixture.tag,
            bits,
            signature,
            terminal: false,
            known_tag: Some(proof),
            copy: None,
        },
    );
    writes::verify_source(
        source,
        &write,
        &mut Analysis::new(source, allocator()).unwrap(),
    )
    .unwrap();
    let check = |f: &Function| {
        cranelift_codegen::verify_function(f, &settings::Flags::new(settings::builder())).unwrap();
        let predecessors = Predecessors::new(f, allocator()).unwrap();
        writes::verify(f, &write)
            .and_then(|()| writes::verify_guard(f, &write, Some(&predecessors), &mut 4096))
    };
    check(&f).unwrap();
    for mutation in 0..5 {
        let mut changed = f.clone();
        match mutation {
            0 => {
                let actual = changed.layout.blocks().flat_map(|b| changed.layout.block_insts(b)).find(|&inst|
                    matches!(changed.dfg.insts[inst], InstructionData::Binary { opcode: Opcode::Bor, args } if args == [fixture.tag, fixture.tag])
                ).unwrap();
                changed.dfg.inst_args_mut(actual)[0] = bits;
            }
            1 => {
                let InstructionData::Brif { blocks, .. } = &mut changed.dfg.insts[fixture.guard]
                else {
                    unreachable!()
                };
                blocks.swap(0, 1);
            }
            2 => {
                let end = changed.layout.last_inst(fixture.no).unwrap();
                changed.replace(end).jump(fixture.yes, &[]);
            }
            3 => {
                let mut c = FuncCursor::new(&mut changed);
                c.goto_inst(fixture.guard);
                c.ins()
                    .store(MemFlagsData::new(), fixture.tag, fixture.slots, 0);
            }
            _ => {
                let load = changed.layout.blocks().flat_map(|b| changed.layout.block_insts(b)).find(|&inst|
                    matches!(changed.dfg.insts[inst], InstructionData::Load { offset, .. } if i32::from(offset) == 8)
                ).unwrap();
                let InstructionData::Load { offset, .. } = &mut changed.dfg.insts[load] else {
                    unreachable!()
                };
                *offset = 0.into();
            }
        }
        assert!(check(&changed).is_err(), "mutation={mutation}");
    }
}

#[cfg(not(miri))]
#[test]
fn native_proven_tag_writes_preserve_guards_and_canonical_values() {
    let fixture = fixture(abi::INTEGER, true);
    let mut f = fixture.function;
    lower(
        &mut f,
        fixture.slots,
        fixture.host,
        1,
        &[],
        allocator(),
        crate::jit::work::Expansion {
            instructions: 4096,
            blocks: 4096,
        },
    )
    .unwrap();
    crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
        let entry = unsafe {
            std::mem::transmute::<
                *const u8,
                unsafe extern "C" fn(
                    *mut crate::jit::abi::payload::Payload,
                    *mut Bridge,
                    u64,
                ) -> u64,
            >(pointer)
        };
        crate::jit::abi::payload::runtime::check_native_proven_tag_write(entry);
    })
    .unwrap();
}
