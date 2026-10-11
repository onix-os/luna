use super::*;
use crate::jit::abi::payload::packed::scoped::bridge::{
    Bridge as PackedBridge, Functions as PackedFunctions, REFUSED,
};
use cranelift_codegen::ir::SigRef;

#[derive(Clone, Copy)]
pub(super) struct Input {
    call: Inst,
    reference: FuncRef,
    kind: u32,
    args: [IrValue; 6],
    result: IrValue,
}

pub(super) struct Call {
    input: Input,
    signature: SigRef,
    blocks: [Block; 3],
}

pub(super) fn admit(
    f: &Function,
    call: Inst,
    slots: IrValue,
    host: IrValue,
    imports: &[(u32, FuncRef)],
) -> Result<Input, JitError> {
    let InstructionData::Call {
        opcode: Opcode::Call,
        func_ref,
        ..
    } = f.dfg.insts[call]
    else {
        return Err(invalid());
    };
    let kind = imports
        .iter()
        .find(|(_, reference)| *reference == func_ref)
        .map(|&(kind, _)| kind)
        .ok_or_else(invalid)?;
    let args: [IrValue; 6] = f.dfg.inst_args(call).try_into().map_err(|_| invalid())?;
    if !(1..=10).contains(&kind)
        || args[0] != host
        || args[1] != slots
        || f.layout.next_inst(call).is_none()
    {
        return Err(invalid());
    }
    let signature = &f.dfg.signatures[f.dfg.ext_funcs[func_ref].signature];
    if signature.call_conv != f.signature.call_conv
        || signature.params.iter().copied().ne([
            types::I64,
            types::I64,
            types::I32,
            types::I32,
            types::I32,
            types::I32,
        ]
        .map(AbiParam::new))
        || signature.returns != [AbiParam::new(types::I32)]
    {
        return Err(invalid());
    }
    let result = reads::value(f, call, Opcode::Call, &args, types::I32)?;
    Ok(Input {
        call,
        reference: func_ref,
        kind,
        args,
        result,
    })
}

pub(super) fn emit(f: &mut Function, input: Input) -> Call {
    let mut signature = Signature::new(f.signature.call_conv);
    signature
        .params
        .extend([types::I64, types::I32, types::I32, types::I32, types::I32].map(AbiParam::new));
    signature.returns.push(AbiParam::new(types::I32));
    let signature = f.import_signature(signature);
    emit_with_signature(f, input, signature)
}

pub(super) fn emit_with_signature(f: &mut Function, input: Input, signature: SigRef) -> Call {
    let before = f.layout.inst_block(input.call).unwrap();
    let next = f.layout.next_inst(input.call).unwrap();
    let after = f.dfg.make_block();
    f.layout.split_block(after, next);
    let refused = reads::block(f);
    let mut c = FuncCursor::new(f);
    c.goto_inst(input.call);
    let control = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        input.args[0],
        std::mem::offset_of!(PackedBridge, control) as i32,
    );
    let table = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        input.args[0],
        std::mem::offset_of!(PackedBridge, functions) as i32,
    );
    let callee = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        table,
        (std::mem::offset_of!(PackedFunctions, helpers)
            + (input.kind as usize - 1) * std::mem::size_of::<usize>()) as i32,
    );
    c.func.replace(input.call).call_indirect(
        signature,
        callee,
        &[
            control,
            input.args[2],
            input.args[3],
            input.args[4],
            input.args[5],
        ],
    );
    c.goto_bottom(before);
    let status = c.ins().iconst(types::I32, REFUSED as i64);
    let stopped = c.ins().icmp(IntCC::Equal, input.result, status);
    c.ins().brif(stopped, refused, &[], after, &[]);
    c.goto_bottom(refused);
    c.ins().return_(&[status]);
    Call {
        input,
        signature,
        blocks: [before, after, refused],
    }
}

pub(super) fn fragment<'a>(
    f: &Function,
    call: &'a Call,
) -> Result<packed_lower::Fragment<'a>, JitError> {
    Ok(packed_lower::Fragment {
        source: call.input.call,
        first: f
            .layout
            .block_insts(call.blocks[0])
            .rev()
            .nth(6)
            .ok_or_else(invalid)?,
        next: f.layout.first_inst(call.blocks[1]),
        blocks: &call.blocks[1..],
    })
}

pub(super) fn verify_source(
    f: &Function,
    call: &Call,
    slots: IrValue,
    host: IrValue,
    imports: &[(u32, FuncRef)],
) -> Result<(), JitError> {
    let actual = admit(f, call.input.call, slots, host, imports)?;
    if actual.reference != call.input.reference
        || actual.kind != call.input.kind
        || actual.args != call.input.args
        || actual.result != call.input.result
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify(f: &Function, call: &Call) -> Result<(), JitError> {
    if !(1..=10).contains(&call.input.kind) {
        return Err(invalid());
    }
    let [before, after, refused] = call.blocks;
    let [branch, predicate, status, gateway, callee, table, control] =
        shape(f.layout.block_insts(before).rev().take(7))?;
    if gateway != call.input.call
        || f.signature.returns != [AbiParam::new(types::I32)]
        || !f.dfg.block_params(after).is_empty()
        || !f.dfg.block_params(refused).is_empty()
    {
        return Err(invalid());
    }
    let control = reads::load(
        f,
        control,
        call.input.args[0],
        std::mem::offset_of!(PackedBridge, control) as i32,
        types::I64,
    )?;
    let table = reads::load(
        f,
        table,
        call.input.args[0],
        std::mem::offset_of!(PackedBridge, functions) as i32,
        types::I64,
    )?;
    let callee = reads::load(
        f,
        callee,
        table,
        (std::mem::offset_of!(PackedFunctions, helpers)
            + (call.input.kind as usize - 1) * std::mem::size_of::<usize>()) as i32,
        types::I64,
    )?;
    let result = reads::value(
        f,
        gateway,
        Opcode::CallIndirect,
        &[
            callee,
            control,
            call.input.args[2],
            call.input.args[3],
            call.input.args[4],
            call.input.args[5],
        ],
        types::I32,
    )?;
    let InstructionData::CallIndirect { sig_ref, .. } = f.dfg.insts[gateway] else {
        return Err(invalid());
    };
    let signature = &f.dfg.signatures[sig_ref];
    if result != call.input.result
        || sig_ref != call.signature
        || signature.call_conv != f.signature.call_conv
        || signature.params.iter().copied().ne([
            types::I64,
            types::I32,
            types::I32,
            types::I32,
            types::I32,
        ]
        .map(AbiParam::new))
        || signature.returns != [AbiParam::new(types::I32)]
    {
        return Err(invalid());
    }
    let status_value = reads::value(f, status, Opcode::Iconst, &[], types::I32)?;
    if !matches!(f.dfg.insts[status], InstructionData::UnaryImm { imm, .. } if i64::from(imm) == REFUSED as i64)
    {
        return Err(invalid());
    }
    let predicate = reads::compare(f, predicate, IntCC::Equal, [result, status_value])?;
    reads::branch(f, branch, predicate, refused, after)?;
    let [returned] = shape(f.layout.block_insts(refused))?;
    if f.dfg.insts[returned].opcode() != Opcode::Return
        || f.dfg.inst_args(returned) != [status_value]
    {
        return Err(invalid());
    }
    Ok(())
}

#[path = "packed_call_tests.rs"]
pub(super) mod tests;
