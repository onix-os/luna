use super::*;
use crate::jit::abi::payload::packed::scoped::bridge::entry;
use cranelift_codegen::ir::{ExtFuncData, ExternalName};

fn fixture(kind: u32, destination: u32) -> (Function, IrValue, IrValue, Inst, [(u32, FuncRef); 1]) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    f.signature.returns.push(AbiParam::new(types::I32));
    let mut helper = Signature::new(f.signature.call_conv);
    helper.params.extend(
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
    helper.returns.push(AbiParam::new(types::I32));
    let signature = f.import_signature(helper);
    let reference = f.import_function(ExtFuncData {
        name: ExternalName::testcase("packed_helper"),
        signature,
        colocated: false,
        patchable: false,
    });
    let mut frontend = FunctionBuilderContext::new();
    let (slots, host, call);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut frontend);
        let entry = b.create_block();
        let invoke = b.create_block();
        let empty = b.create_block();
        let done = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.append_block_param(done, types::I64);
        b.append_block_param(done, types::I32);
        b.append_block_param(done, types::I32);
        b.switch_to_block(entry);
        let pc;
        let budget;
        let exit;
        [slots, pc, budget, exit, host] = b.block_params(entry).try_into().unwrap();
        let exhausted = b.ins().icmp_imm_u(IntCC::Equal, budget, 0);
        b.ins().brif(exhausted, empty, &[], invoke, &[]);
        b.switch_to_block(empty);
        let zero = b.ins().iconst(types::I32, 0);
        let reason = b
            .ins()
            .iconst(types::I32, crate::jit::exits::Kind::Budget as i64);
        b.ins().jump(done, &[pc.into(), zero.into(), reason.into()]);
        b.switch_to_block(invoke);
        let a = b.ins().iconst(types::I32, destination as i64);
        let zero = b.ins().iconst(types::I32, 0);
        let narrow_pc = b.ins().ireduce(types::I32, pc);
        call = b
            .ins()
            .call(reference, &[host, slots, a, zero, zero, narrow_pc]);
        let status = b.inst_results(call)[0];
        let completed = b
            .ins()
            .icmp_imm_u(IntCC::Equal, status, abi::HELPER_COMPLETED as i64);
        let panicked = b
            .ins()
            .icmp_imm_u(IntCC::Equal, status, abi::HELPER_PANICKED as i64);
        let next_pc = b.ins().iadd_imm_s(pc, 1);
        let next_pc = b.ins().select(completed, next_pc, pc);
        let count = b.ins().uextend(types::I32, completed);
        let panic = b
            .ins()
            .iconst(types::I32, crate::jit::exits::Kind::Panic as i64);
        let reason = b.ins().select(panicked, panic, zero);
        b.ins()
            .jump(done, &[next_pc.into(), count.into(), reason.into()]);
        b.switch_to_block(done);
        let [pc, count, reason]: [_; 3] = b.block_params(done).try_into().unwrap();
        b.ins().store(MemFlagsData::new(), pc, exit, 0);
        b.ins().store(MemFlagsData::new(), count, exit, 8);
        b.ins().store(MemFlagsData::new(), reason, exit, 12);
        let returned = b.ins().iconst(types::I32, entry::RETURNED as i64);
        b.ins().return_(&[returned]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    (f, slots, host, call, [(kind, reference)])
}

#[test]
fn packed_helper_lowering_preserves_source_operands_and_status_flow_for_all_kinds() {
    let flags = settings::Flags::new(settings::builder());
    for kind in 1..=10 {
        let (source, slots, host, inst, imports) = fixture(kind, 1);
        cranelift_codegen::verify_function(&source, &flags).unwrap();
        let input = admit(&source, inst, slots, host, &imports).unwrap();
        let mut actual = source.clone();
        let call = emit(&mut actual, input);
        verify_source(&source, &call, slots, host, &imports).unwrap();
        verify(&actual, &call).unwrap();
        cranelift_codegen::verify_function(&actual, &flags).unwrap();
        let (before_i, before_b) = size(&source);
        assert_eq!(size(&actual), (before_i + 7, before_b + 2));
    }
}

#[test]
fn packed_helper_checker_rejects_valid_ir_with_wrong_offsets_operands_status_or_edges() {
    let (source, slots, host, inst, imports) = fixture(1, 1);
    let input = admit(&source, inst, slots, host, &imports).unwrap();
    let mut emitted = source.clone();
    let call = emit(&mut emitted, input);
    let flags = settings::Flags::new(settings::builder());
    let mut rejected = 0;
    for inst in emitted
        .layout
        .block_insts(call.blocks[0])
        .rev()
        .take(7)
        .chain(emitted.layout.block_insts(call.blocks[2]))
    {
        let mut changed = emitted.clone();
        match &mut changed.dfg.insts[inst] {
            InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 8).into(),
            InstructionData::UnaryImm { imm, .. } => *imm = 2.into(),
            InstructionData::IntCompare { cond, .. } => *cond = IntCC::NotEqual,
            InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
            InstructionData::CallIndirect { .. } => changed.dfg.inst_args_mut(inst).swap(2, 3),
            InstructionData::MultiAry {
                opcode: Opcode::Return,
                ..
            } => changed.dfg.inst_args_mut(inst)[0] = input.result,
            _ => panic!("unexpected packed call instruction"),
        }
        cranelift_codegen::verify_function(&changed, &flags).unwrap();
        assert!(verify(&changed, &call).is_err());
        rejected += 1;
    }
    assert_eq!(rejected, 8);
    let mut wrong_source = source.clone();
    wrong_source.dfg.inst_args_mut(input.call).swap(2, 3);
    cranelift_codegen::verify_function(&wrong_source, &flags).unwrap();
    assert!(verify_source(&wrong_source, &call, slots, host, &imports).is_err());
    for kind in [0, 11, u32::MAX] {
        assert!(admit(&source, input.call, slots, host, &[(kind, imports[0].1)]).is_err());
    }
    assert!(admit(&source, input.call, host, slots, &imports).is_err());
    assert!(admit(&source, input.call, slots, host, &[]).is_err());
}

#[cfg(not(miri))]
#[test]
fn packed_generated_helper_entries_preserve_exits_and_stop_refused_effects() {
    for (kind, destination) in [(1, 1), (4, 1), (1, 99)] {
        let (source, slots, host, inst, imports) = fixture(kind, destination);
        let input = admit(&source, inst, slots, host, &imports).unwrap();
        let mut f = source.clone();
        let call = emit(&mut f, input);
        verify_source(&source, &call, slots, host, &imports).unwrap();
        verify(&f, &call).unwrap();
        crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
            let entry = unsafe { std::mem::transmute::<*const u8, entry::Entry>(pointer) };
            entry::check_native_helper(entry, kind, destination);
        })
        .unwrap();
    }
}
