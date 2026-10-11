use super::*;
use cranelift_codegen::ir::{ExtFuncData, ExternalName};

struct Fixture {
    f: Function,
    slots: IrValue,
    host: IrValue,
    tag_store: Inst,
    bits_store: Inst,
    tag: IrValue,
    bits: IrValue,
    helpers: [(u32, FuncRef); 1],
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
    let reference = f.import_function(ExtFuncData {
        name: ExternalName::testcase("copy_helper"),
        signature,
        colocated: false,
        patchable: false,
    });
    let [entry, left, right, done] = std::array::from_fn(|_| reads::block(&mut f));
    let slots = f.dfg.append_block_param(entry, types::I64);
    let host = f.dfg.append_block_param(entry, types::I64);
    let condition = f.dfg.append_block_param(entry, types::I32);
    let mut c = FuncCursor::new(&mut f);
    c.goto_bottom(entry);
    let zero = c.ins().iconst(types::I32, 0);
    let tag = c.ins().load(types::I64, MemFlagsData::new(), slots, 0);
    if matches!(fault, 3 | 4) {
        c.ins().store(
            MemFlagsData::new(),
            tag,
            slots,
            if fault == 3 { 0 } else { 32 },
        );
        c.ins().store(
            MemFlagsData::new(),
            tag,
            slots,
            if fault == 3 { 8 } else { 40 },
        );
    }
    let bits = c.ins().load(
        if fault == 8 { types::F64 } else { types::I64 },
        MemFlagsData::new(),
        slots,
        if fault == 7 { 40 } else { 8 },
    );
    let tag = if fault == 9 {
        c.ins().iconst(types::I64, abi::INTEGER as i64)
    } else {
        tag
    };
    if fault == 6 {
        c.ins()
            .call(reference, &[host, slots, zero, zero, zero, zero]);
    }
    c.ins().brif(condition, left, &[], right, &[]);
    c.goto_bottom(left);
    if fault == 5 {
        c.ins().store(MemFlagsData::new(), tag, slots, 32);
        c.ins().store(MemFlagsData::new(), tag, slots, 40);
    }
    c.ins().jump(done, &[]);
    c.goto_bottom(right);
    if fault == 12 {
        c.ins().brif(condition, right, &[], done, &[]);
    } else {
        c.ins().jump(done, &[]);
    }
    c.goto_bottom(done);
    let tag_store = c.ins().store(MemFlagsData::new(), tag, slots, 16);
    let bits_store = c.ins().store(MemFlagsData::new(), bits, slots, 24);
    if matches!(fault, 10 | 11) {
        c.ins()
            .call(reference, &[host, slots, zero, zero, zero, zero]);
    }
    if matches!(fault, 1 | 2 | 10 | 11) {
        c.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots,
            if fault == 2 { 24 } else { 8 },
        );
    }
    c.ins().return_(&[]);
    let helpers = [(
        if fault == 11 {
            abi::HELPER_GET_UPVALUE
        } else {
            1
        },
        reference,
    )];
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    Fixture {
        f,
        slots,
        host,
        tag_store,
        bits_store,
        tag,
        bits,
        helpers,
    }
}

fn prove(
    f: &Fixture,
    tags: &mut same_tag::Analysis,
    terminal: &mut terminal::Analysis<'_>,
) -> Option<Proof> {
    admit(
        &f.f,
        f.slots,
        f.tag_store,
        f.bits_store,
        1,
        f.tag,
        f.bits,
        tags,
        terminal,
    )
}

#[test]
fn copy_proofs_require_fresh_pairs_and_both_terminal_cells() {
    for fault in 0..=12 {
        let f = fixture(fault);
        let memory = allocator();
        let mut tags = same_tag::Analysis::new(&f.f, memory.clone()).unwrap();
        let mut terminal =
            terminal::Analysis::new(&f.f, f.slots, f.host, 3, &f.helpers, memory.clone()).unwrap();
        let proof = prove(&f, &mut tags, &mut terminal);
        assert_eq!(proof.is_some(), matches!(fault, 0 | 10), "fault={fault}");
        if let Some(proof) = proof {
            assert_eq!(proof.source(), 0);
            assert!(verify(&f.f, f.slots, proof, &mut tags, &mut terminal, true));
            assert!(!proof.matches(0, f.tag, f.bits));
            assert!(!proof.matches(1, f.bits, f.tag));
            let other = f.f.clone();
            assert!(!verify(
                &other,
                f.slots,
                proof,
                &mut tags,
                &mut terminal,
                true
            ));
        }
        drop((tags, terminal));
        assert_eq!(memory.0.current(), 0);
    }
}

#[test]
fn copy_lowering_selects_gateway_without_changing_other_writes() {
    for fault in 0..=12 {
        for contract in [Contract::Checked, Contract::FrameBound] {
            let f = fixture(fault);
            let memory = allocator();
            let mut candidate = f.f.clone();
            lower_contract(
                &mut candidate,
                f.slots,
                f.host,
                3,
                &f.helpers,
                memory.clone(),
                crate::jit::work::Expansion {
                    instructions: 8192,
                    blocks: 4096,
                },
                contract,
            )
            .unwrap();
            cranelift_codegen::verify_function(
                &candidate,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
            let copies = candidate.layout.blocks().flat_map(|b| candidate.layout.block_insts(b)).filter(|&inst| {
                matches!(candidate.dfg.insts[inst], InstructionData::Load { offset, .. } if i32::from(offset) == std::mem::offset_of!(Functions, copy) as i32)
            }).count();
            assert_eq!(
                copies,
                usize::from(matches!(fault, 0 | 10)),
                "fault={fault} contract={contract:?}"
            );
            assert_eq!(memory.0.current(), 0);
        }
    }
}

#[test]
fn copy_expansion_reserves_both_gateway_constants_before_mutation() {
    let f = fixture(0);
    let generous = crate::jit::work::Expansion {
        instructions: 8192,
        blocks: 4096,
    };
    let p = plan(&f.f, f.slots, f.host, 3, &f.helpers, generous).unwrap();
    assert_eq!(p.copies, 1);
    let (instructions, blocks) = size(&f.f);
    for (extra, accepted) in [(0, true), (1, false), (2, false)] {
        let mut candidate = f.f.clone();
        let memory = allocator();
        let result = lower_contract(
            &mut candidate,
            f.slots,
            f.host,
            3,
            &f.helpers,
            memory.clone(),
            crate::jit::work::Expansion {
                instructions: instructions + p.instructions - extra,
                blocks: blocks + p.blocks,
            },
            Contract::FrameBound,
        );
        assert_eq!(result.is_ok(), accepted);
        if !accepted {
            assert_eq!(candidate, f.f);
            assert_eq!(memory.0.peak(), 0);
        }
        assert_eq!(memory.0.current(), 0);
    }
}

#[test]
fn copy_checker_rejects_wrong_source_gateway_constants_and_arguments() {
    let f = fixture(0);
    let mut tags = same_tag::Analysis::new(&f.f, allocator()).unwrap();
    let mut terminal =
        terminal::Analysis::new(&f.f, f.slots, f.host, 3, &f.helpers, allocator()).unwrap();
    let proof = prove(&f, &mut tags, &mut terminal).unwrap();
    let mut candidate = f.f.clone();
    let mut signature = Signature::new(f.f.signature.call_conv);
    signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let signature = candidate.import_signature(signature);
    let write = writes::emit(
        &mut candidate,
        f.tag_store,
        f.bits_store,
        writes::Input {
            slots: f.slots,
            host: f.host,
            index: 1,
            tag: f.tag,
            bits: f.bits,
            signature,
            terminal: true,
            known_tag: None,
            copy: Some(proof),
        },
    );
    writes::verify_copy_source(&f.f, &write, &mut tags, &mut terminal).unwrap();
    writes::verify(&candidate, &write).unwrap();
    let call = candidate
        .layout
        .blocks()
        .flat_map(|b| candidate.layout.block_insts(b))
        .find(|&i| candidate.dfg.insts[i].opcode() == Opcode::CallIndirect)
        .unwrap();
    let slow = candidate.layout.inst_block(call).unwrap();
    let flags = settings::Flags::new(settings::builder());
    let mut mutations = 0;
    for inst in candidate.layout.block_insts(slow) {
        let mut changed = candidate.clone();
        match &mut changed.dfg.insts[inst] {
            InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 8).into(),
            InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) + 1).into(),
            InstructionData::CallIndirect { .. } => changed.dfg.inst_args_mut(inst).swap(4, 5),
            _ => continue,
        }
        cranelift_codegen::verify_function(&changed, &flags).unwrap();
        assert!(writes::verify(&changed, &write).is_err());
        mutations += 1;
    }
    assert_eq!(mutations, 6);
    for altered in [
        Proof { source: 1, ..proof },
        Proof { dest: 0, ..proof },
        Proof {
            tag: f.bits,
            ..proof
        },
        Proof {
            bits: f.tag,
            ..proof
        },
    ] {
        assert!(!verify(
            &f.f,
            f.slots,
            altered,
            &mut tags,
            &mut terminal,
            true
        ));
    }
}

#[cfg(not(miri))]
#[test]
fn native_copy_keeps_fast_stores_and_retires_only_slow_path_cells() {
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
            crate::jit::abi::payload::runtime::check_native_terminal_copy(entry);
        }).unwrap();
    }
}
