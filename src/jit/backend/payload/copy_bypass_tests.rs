use super::*;
use cranelift_codegen::ir::{ExtFuncData, ExternalName};

struct Fixture {
    f: Function,
    slots: IrValue,
    host: IrValue,
    tag: IrValue,
    bits: IrValue,
    bits_load: Inst,
    tag_store: Inst,
    bits_store: Inst,
    reference: Block,
    helpers: [(u32, FuncRef); 1],
    dest: u32,
}

fn allocator() -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(1 << 20))
}

fn fixture(fault: u8) -> Fixture {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64, types::I64, types::I32].map(AbiParam::new));
    let mut signature = Signature::new(f.signature.call_conv);
    signature.params.extend(
        [
            types::I64,
            types::I64,
            types::I32,
            types::I32,
            types::I32,
            types::I32,
        ]
        .map(AbiParam::new),
    );
    signature.returns.push(AbiParam::new(types::I32));
    let signature = f.import_signature(signature);
    let helper = f.import_function(ExtFuncData {
        name: ExternalName::testcase("bypass_move"),
        signature,
        colocated: false,
        patchable: false,
    });
    let [entry, direct, reference] = std::array::from_fn(|_| reads::block(&mut f));
    let slots = f.dfg.append_block_param(entry, types::I64);
    let host = f.dfg.append_block_param(entry, types::I64);
    f.dfg.append_block_param(entry, types::I32);
    if fault == 7 {
        f.dfg.append_block_param(reference, types::I64);
    }
    let mut c = FuncCursor::new(&mut f);
    c.goto_bottom(entry);
    let tag = c.ins().load(types::I64, MemFlagsData::new(), slots, 0);
    if fault == 8 {
        c.ins().iconst(types::I64, 7);
    }
    let bits = c.ins().load(types::I64, MemFlagsData::new(), slots, 8);
    let bits_load = c.func.dfg.value_def(bits).inst().unwrap();
    let reference_tag = c.ins().iconst(
        types::I64,
        if fault == 6 {
            abi::INTEGER
        } else {
            abi::REFERENCE
        } as i64,
    );
    let scalar = c.ins().icmp(
        if fault == 5 {
            IntCC::Equal
        } else {
            IntCC::NotEqual
        },
        tag,
        reference_tag,
    );
    if fault == 4 {
        c.ins().brif(scalar, reference, &[], direct, &[]);
    } else if fault == 7 {
        c.ins().brif(scalar, direct, &[], reference, &[bits.into()]);
    } else {
        c.ins().brif(scalar, direct, &[], reference, &[]);
    }
    c.goto_bottom(direct);
    if fault == 2 {
        c.ins().iconst(types::I64, 9);
    }
    let dest = if fault == 3 { 0 } else { 1 };
    let tag_store = c.ins().store(MemFlagsData::new(), tag, slots, dest * 16);
    let bits_store = c
        .ins()
        .store(MemFlagsData::new(), bits, slots, dest * 16 + 8);
    if fault == 1 {
        c.ins().iadd(bits, bits);
    }
    if fault == 12 {
        let alias = c.ins().iconst(types::I64, 0);
        let alias_inst = c.func.dfg.value_def(alias).inst().unwrap();
        c.func.dfg.detach_inst_results(alias_inst);
        c.func.layout.remove_inst(alias_inst);
        c.func.dfg.change_to_alias(alias, bits);
        c.ins().iadd(alias, alias);
    }
    if matches!(fault, 9 | 11) {
        c.ins().load(types::I64, MemFlagsData::new(), slots, 8);
    }
    if matches!(fault, 10 | 11) {
        c.ins().load(types::I64, MemFlagsData::new(), slots, 24);
    }
    if fault == 13 {
        let carried = reads::block(c.func);
        c.func.dfg.append_block_param(carried, types::I64);
        c.ins().jump(carried, &[bits.into()]);
        c.goto_bottom(carried);
    }
    c.ins().return_(&[]);
    c.goto_bottom(reference);
    let zero = c.ins().iconst(types::I32, 0);
    let destination = c.ins().iconst(types::I32, dest as i64);
    let pc = c.ins().iconst(types::I32, 23);
    c.ins()
        .call(helper, &[host, slots, destination, zero, zero, pc]);
    c.ins().return_(&[]);
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    Fixture {
        f,
        slots,
        host,
        tag,
        bits,
        bits_load,
        tag_store,
        bits_store,
        reference,
        helpers: [(abi::HELPER_MOVE, helper)],
        dest: dest as u32,
    }
}

fn staged(
    f: &Fixture,
    contract: Contract,
    work: usize,
) -> (
    Function,
    [Read; 1],
    [Write; 1],
    BudgetVec<Bypass, BudgetAllocator>,
) {
    let mut tags = same_tag::Analysis::new(&f.f, allocator()).unwrap();
    let mut terminal =
        terminal::Analysis::new(&f.f, f.slots, f.host, 3, &f.helpers, allocator()).unwrap();
    let proof = copy::admit(
        &f.f,
        f.slots,
        f.tag_store,
        f.bits_store,
        f.dest,
        f.tag,
        f.bits,
        &mut tags,
        &mut terminal,
    )
    .unwrap();
    let mut output = f.f.clone();
    let mut signature = Signature::new(f.f.signature.call_conv);
    signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let signature = output.import_signature(signature);
    let mut reads = [Read::Scalar(reads::emit_contract(
        &mut output,
        f.bits_load,
        f.slots,
        0,
        contract,
    ))];
    let writes = [Write::Checked(writes::emit_contract(
        &mut output,
        f.tag_store,
        f.bits_store,
        writes::Input {
            slots: f.slots,
            host: f.host,
            index: f.dest,
            tag: f.tag,
            bits: f.bits,
            signature,
            terminal: true,
            known_tag: None,
            copy: Some(proof),
        },
        contract,
    ))];
    let mut records = BudgetVec::new_in(allocator());
    records.try_reserve_exact(1).unwrap();
    let before = size(&output);
    apply(
        &f.f,
        &mut output,
        f.slots,
        &mut reads,
        &writes,
        &mut records,
        &mut { work },
    )
    .unwrap();
    assert_eq!(
        size(&output),
        (before.0 + 4 * records.len(), before.1 + 2 * records.len())
    );
    (output, reads, writes, records)
}

#[test]
fn bypass_requires_exact_guard_sole_use_and_both_terminal_cells() {
    for fault in 0..=13 {
        let f = fixture(fault);
        let memory = allocator();
        let mut output = f.f.clone();
        lower_contract(
            &mut output,
            f.slots,
            f.host,
            3,
            &f.helpers,
            memory.clone(),
            crate::jit::work::Expansion {
                instructions: 8192,
                blocks: 4096,
            },
            Contract::FrameBound,
        )
        .unwrap();
        cranelift_codegen::verify_function(&output, &settings::Flags::new(settings::builder()))
            .unwrap();
        assert_eq!(memory.0.current(), 0);
        let source_shape = candidate(
            &f.f,
            f.slots,
            f.tag_store,
            f.bits_store,
            f.dest,
            f.tag,
            f.bits,
        );
        assert_eq!(
            source_shape,
            matches!(fault, 0 | 1 | 9..=13),
            "fault={fault}"
        );
        if fault <= 1 {
            let (_, _, _, records) = staged(&f, Contract::FrameBound, 8192);
            assert_eq!(records.len(), usize::from(fault == 0));
        }
        if fault >= 12 {
            assert!(!sole_use(&f.f, f.bits, f.bits_store, &mut 8192));
        }
    }
}

#[test]
fn bypass_work_exhaustion_preserves_the_original_reader() {
    let f = fixture(0);
    for work in [0, 1, 5, 8192] {
        let (output, _, _, records) = staged(&f, Contract::Checked, work);
        assert_eq!(records.len(), usize::from(work == 8192));
        cranelift_codegen::verify_function(&output, &settings::Flags::new(settings::builder()))
            .unwrap();
    }
}

#[test]
fn bypass_checker_rejects_condition_edges_and_record_substitution() {
    let f = fixture(0);
    for contract in [Contract::Checked, Contract::FrameBound] {
        let (output, reads, writes, records) = staged(&f, contract, 8192);
        let record = &records[0];
        let flags = settings::Flags::new(settings::builder());
        let check = |output: &Function, record: &Bypass| {
            cranelift_codegen::verify_function(output, &flags).unwrap();
            let predecessors = crate::jit::preds::Predecessors::new(output, allocator()).unwrap();
            verify(
                &f.f,
                output,
                f.slots,
                &reads,
                &writes,
                record,
                &predecessors,
                &mut 8192,
            )
        };
        check(&output, record).unwrap();
        for inst in output
            .layout
            .block_insts(record.prefix)
            .rev()
            .take(4)
            .chain(output.layout.block_insts(record.check))
        {
            let mut changed = output.clone();
            match &mut changed.dfg.insts[inst] {
                InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 16).into(),
                InstructionData::IntCompare { cond, .. } => *cond = IntCC::Equal,
                InstructionData::UnaryImm { imm, .. } => *imm = (abi::INTEGER as i64).into(),
                InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bor,
                InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                _ => unreachable!(),
            }
            assert!(check(&changed, record).is_err());
        }
        for changed in [
            Bypass { read: 1, ..*record },
            Bypass {
                write: 1,
                ..*record
            },
            Bypass {
                prefix: f.reference,
                ..*record
            },
            Bypass {
                check: f.reference,
                ..*record
            },
        ] {
            assert!(check(&output, &changed).is_err());
        }
        let Read::Scalar(read) = &reads[0] else {
            unreachable!()
        };
        let Write::Checked(write) = &writes[0] else {
            unreachable!()
        };
        let mut changed = output.clone();
        let resume = changed.layout.first_inst(reads::copy_exit(read)).unwrap();
        changed.replace(resume).jump(f.reference, &[]);
        assert!(check(&changed, record).is_err());
        for target in [
            reads::copy_entry(read),
            record.check,
            writes::copy_site(write).unwrap().1,
        ] {
            let mut changed = output.clone();
            let end = changed.layout.last_inst(f.reference).unwrap();
            changed.replace(end).jump(target, &[]);
            assert!(check(&changed, record).is_err());
        }
    }
}

#[test]
fn bypass_expansion_and_workspace_refusals_leave_source_unchanged() {
    let f = fixture(0);
    let generous = crate::jit::work::Expansion {
        instructions: 8192,
        blocks: 4096,
    };
    let p = plan(&f.f, f.slots, f.host, 3, &f.helpers, generous).unwrap();
    assert_eq!(p.bypasses, 1);
    let (instructions, blocks) = size(&f.f);
    for (removed_i, removed_b, accepted) in
        [(0, 0, true), (1, 0, false), (0, 1, false), (4, 2, false)]
    {
        let memory = allocator();
        let mut output = f.f.clone();
        let result = lower_contract(
            &mut output,
            f.slots,
            f.host,
            3,
            &f.helpers,
            memory.clone(),
            crate::jit::work::Expansion {
                instructions: instructions + p.instructions - removed_i,
                blocks: blocks + p.blocks - removed_b,
            },
            Contract::FrameBound,
        );
        assert_eq!(result.is_ok(), accepted);
        if !accepted {
            assert_eq!(output, f.f);
            assert_eq!(memory.0.peak(), 0);
        }
        assert_eq!(memory.0.current(), 0);
    }
    for allocations in 0..3 {
        let memory = allocator();
        memory.0.fail_after(allocations);
        let mut output = f.f.clone();
        assert!(matches!(
            lower_contract(
                &mut output,
                f.slots,
                f.host,
                3,
                &f.helpers,
                memory.clone(),
                generous,
                Contract::FrameBound
            ),
            Err(JitError::ResourceLimit(_))
        ));
        assert_eq!(output, f.f);
        assert_eq!(memory.0.current(), 0);
    }
}

#[cfg(not(miri))]
#[test]
fn native_bypass_keeps_reference_helpers_and_same_tag_stores() {
    for contract in [Contract::Checked, Contract::FrameBound] {
        let mut f = fixture(0);
        lower_contract(
            &mut f.f,
            f.slots,
            f.host,
            3,
            &f.helpers,
            allocator(),
            crate::jit::work::Expansion {
                instructions: 8192,
                blocks: 4096,
            },
            contract,
        )
        .unwrap();
        crate::jit::backend::projection_probe::with_projection_probe(f.f, |pointer| {
            let entry = unsafe { std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut Payload, *mut Bridge, u32)>(pointer) };
            crate::jit::abi::payload::runtime::check_native_guarded_terminal_copy(entry);
        }).unwrap();
    }
}
