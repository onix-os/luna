use super::*;
use crate::jit::readonly_lookup::{binding::Entry as ReadEntry, Plan};
use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{BlockArg, Function, InstructionData, Opcode},
};

mod witness;

pub(in crate::jit) struct ReadCode {
    _memory: Memory,
    pub entry: ReadEntry,
    pub plan: Plan,
    pub registers: usize,
    pub relocations: usize,
    pub integer_resume: Option<(
        crate::jit::readonly_lookup::binding::integer_resume::Plan,
        super::integer_resume::Code,
    )>,
}

pub(in crate::jit) fn compile(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: crate::jit::work::Limits,
) -> Result<ReadCode, JitError> {
    let plan = Plan::recognize(source).ok_or_else(invalid)?;
    let image = compile_image(
        source,
        total,
        limit,
        metadata,
        work,
        Selection {
            projected: false,
            readonly_scope: true,
            #[cfg(test)]
            leaf: false,
            #[cfg(test)]
            cell_kernel: false,
            #[cfg(test)]
            integer_activation: false,
            scoped_helpers: false,
            #[cfg(test)]
            failure: Failure::None,
        },
        false,
    )?;
    if image.entry_arity != 6 || image.projected_upvalues {
        return Err(invalid());
    }
    Ok(ReadCode {
        _memory: image._memory,
        entry: unsafe { std::mem::transmute::<*const u8, ReadEntry>(image.entry) },
        plan,
        registers: image.registers,
        relocations: image.relocations,
        integer_resume: None,
    })
}

pub(in crate::jit) fn compile_pair(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: crate::jit::work::Limits,
    #[cfg(test)] failure: Failure,
) -> Result<Code, JitError> {
    let bound = crate::jit::work::Expansion::admit(source, work)?;
    let mut ordinary = compile_in(
        source,
        total.clone(),
        limit,
        metadata.clone(),
        work,
        #[cfg(test)]
        failure,
    )?;
    if Plan::recognize(source).is_none() {
        return Ok(ordinary);
    }
    let Ok(_owner) = Reservation::new(
        metadata.0.clone(),
        crate::jit::owner::Shared::<Code>::allocation_bytes(),
    ) else {
        return Ok(ordinary);
    };
    let remaining = crate::jit::work::Limits {
        instructions: work.instructions.saturating_sub(bound.instructions),
        blocks: work.blocks.saturating_sub(bound.blocks),
        relocations: work.relocations.saturating_sub(ordinary.relocations),
    };
    let Ok(mut child) = compile(source, total.clone(), limit, metadata.clone(), remaining) else {
        return Ok(ordinary);
    };
    if crate::jit::readonly_lookup::binding::integer_resume::enabled() {
        if let Some(plan) =
            crate::jit::readonly_lookup::binding::integer_resume::Plan::recognize(source)
        {
            if let Ok(bound) = crate::jit::work::Expansion::admit(source, remaining)
                .and_then(|bound| expansion(bound, remaining))
            {
                let extra = crate::jit::work::Limits {
                    instructions: remaining.instructions.saturating_sub(bound.instructions),
                    blocks: remaining.blocks.saturating_sub(bound.blocks),
                    relocations: remaining.relocations.saturating_sub(child.relocations),
                };
                if let Ok(code) =
                    super::integer_resume::compile(source, total, limit, metadata.clone(), extra)
                {
                    child.integer_resume = Some((plan, code));
                }
            }
        }
    }
    let Some(relocations) = ordinary
        .relocations
        .checked_add(child.relocations)
        .filter(|&count| count <= work.relocations)
    else {
        return Ok(ordinary);
    };
    #[cfg(test)]
    let previous_limit = metadata.0.limit();
    #[cfg(test)]
    match failure {
        Failure::RefuseReadonlyOwnerStorage => metadata.0.set_limit(metadata.0.current()),
        Failure::RefuseReadonlyOwnerAllocation => metadata.0.fail_after(0),
        _ => {}
    }
    ordinary.readonly_lookup = crate::jit::owner::Shared::try_new(child, metadata.clone()).ok();
    #[cfg(test)]
    match failure {
        Failure::RefuseReadonlyOwnerStorage => metadata.0.set_limit(previous_limit),
        Failure::RefuseReadonlyOwnerAllocation => metadata.0.fail_after(usize::MAX),
        _ => {}
    }
    if ordinary.readonly_lookup.is_some() {
        ordinary.relocations = relocations;
    }
    Ok(ordinary)
}

fn invalid() -> JitError {
    JitError::Compilation("invalid read-only lookup lowering".into())
}

pub(super) fn expansion(
    mut bound: crate::jit::work::Expansion,
    limits: crate::jit::work::Limits,
) -> Result<crate::jit::work::Expansion, JitError> {
    bound.instructions = bound
        .instructions
        .checked_add(64)
        .ok_or(JitError::ResourceLimit("read-only IR"))?;
    bound.blocks = bound
        .blocks
        .checked_add(3)
        .ok_or(JitError::ResourceLimit("read-only blocks"))?;
    if bound.instructions > limits.instructions || bound.blocks > limits.blocks {
        return Err(JitError::ResourceLimit("read-only expansion"));
    }
    Ok(bound)
}

pub(super) struct Rewrite {
    root: Block,
    check: Block,
    refused: Block,
    original: Block,
    call: Inst,
    body: [Inst; 6],
    view: IrValue,
    slots: IrValue,
    exit: IrValue,
}

pub(super) fn lower(
    function: &mut Function,
    source: &Snapshot,
    plan: Plan,
    call: Inst,
    slots: IrValue,
    exit: IrValue,
    allocator: BudgetAllocator,
) -> Result<(), JitError> {
    if Plan::recognize(source) != Some(plan) || function.signature.params.len() != 5 {
        return Err(invalid());
    }
    let witness = witness::Witness::new(function, allocator)?;
    let rewrite = rewrite(function, plan, call, slots, exit)?;
    verify(function, source, plan, &rewrite, &witness)
}

fn rewrite(
    function: &mut Function,
    plan: Plan,
    call: Inst,
    slots: IrValue,
    exit: IrValue,
) -> Result<Rewrite, JitError> {
    let original = function.layout.entry_block().ok_or_else(invalid)?;
    let ptr = function.dfg.value_type(slots);
    let root = function.dfg.make_block();
    let check = function.dfg.make_block();
    let refused = function.dfg.make_block();
    function.layout.insert_block(root, original);
    function.layout.insert_block(check, original);
    function.layout.insert_block(refused, original);
    let mut args = [slots; 6];
    for (index, argument) in args.iter_mut().enumerate() {
        let ty = if index == 5 {
            ptr
        } else {
            function.signature.params[index].value_type
        };
        *argument = function.dfg.append_block_param(root, ty);
    }
    function.signature.params.push(AbiParam::new(ptr));
    let view = args[5];
    {
        let mut cursor = FuncCursor::new(function).at_bottom(root);
        let nonnull = cursor.ins().icmp_imm_s(IntCC::NotEqual, view, 0);
        cursor.ins().brif(nonnull, check, &[], refused, &[]);
    }
    {
        let mut cursor = FuncCursor::new(function).at_bottom(check);
        let mut valid = cursor.ins().iconst(types::I8, 1);
        for (offset, expected) in [(0, plan.binding()[0]), (8, plan.binding()[1]), (32, 0)] {
            let value = cursor
                .ins()
                .load(types::I64, MemFlagsData::trusted(), view, offset);
            let same = cursor
                .ins()
                .icmp_imm_s(IntCC::Equal, value, expected as i64);
            valid = cursor.ins().band(valid, same);
        }
        let tag = cursor
            .ins()
            .load(types::I64, MemFlagsData::trusted(), view, 16);
        let admitted =
            cursor
                .ins()
                .icmp_imm_s(IntCC::UnsignedLessThanOrEqual, tag, abi::REFERENCE as i64);
        valid = cursor.ins().band(valid, admitted);
        let forwarded: [BlockArg; 5] = std::array::from_fn(|i| args[i].into());
        cursor.ins().brif(valid, original, &forwarded, refused, &[]);
    }
    {
        let mut cursor = FuncCursor::new(function).at_bottom(refused);
        let zero = cursor.ins().iconst(types::I32, 0);
        cursor
            .ins()
            .store(MemFlagsData::trusted(), args[1], args[3], 0);
        cursor
            .ins()
            .store(MemFlagsData::trusted(), zero, args[3], 8);
        cursor
            .ins()
            .store(MemFlagsData::trusted(), zero, args[3], 12);
        cursor.ins().return_(&[]);
    }
    let body = {
        let mut cursor = FuncCursor::new(function).at_inst(call);
        let tag = cursor
            .ins()
            .load(types::I64, MemFlagsData::trusted(), view, 16);
        let bits = cursor
            .ins()
            .load(types::I64, MemFlagsData::trusted(), view, 24);
        let destination = i32::from(plan.destination()) * 16;
        let tag_store = cursor
            .ins()
            .store(MemFlagsData::trusted(), tag, slots, destination);
        let bits_store = cursor
            .ins()
            .store(MemFlagsData::trusted(), bits, slots, destination + 8);
        let one = cursor.ins().iconst(types::I64, 1);
        let marked = cursor.ins().store(MemFlagsData::trusted(), one, view, 32);
        [
            value_inst(cursor.func, tag)?,
            value_inst(cursor.func, bits)?,
            tag_store,
            bits_store,
            value_inst(cursor.func, one)?,
            marked,
        ]
    };
    function
        .replace(call)
        .iconst(types::I32, i64::from(abi::HELPER_COMPLETED));
    Ok(Rewrite {
        root,
        check,
        refused,
        original,
        call,
        body,
        view,
        slots,
        exit,
    })
}

fn value_inst(function: &Function, value: IrValue) -> Result<Inst, JitError> {
    match function.dfg.value_def(value) {
        cranelift_codegen::ir::ValueDef::Result(inst, 0) => Ok(inst),
        _ => Err(invalid()),
    }
}

fn verify(
    function: &Function,
    source: &Snapshot,
    plan: Plan,
    rewrite: &Rewrite,
    witness: &witness::Witness,
) -> Result<(), JitError> {
    if Plan::recognize(source) != Some(plan) || function.signature.params.len() != 6 {
        return Err(invalid());
    }
    witness.verify(function, rewrite)?;
    let body = rewrite.body;
    for (inst, offset) in [(body[0], 16), (body[1], 24)] {
        match function.dfg.insts[inst] {
            InstructionData::Load {
                opcode: Opcode::Load,
                flags,
                arg,
                offset: found,
            } if function.dfg.mem_flags[flags] == MemFlagsData::trusted()
                && arg == rewrite.view
                && i32::from(found) == offset
                && function.dfg.value_type(function.dfg.inst_results(inst)[0]) == types::I64 => {}
            _ => return Err(invalid()),
        }
    }
    for (inst, value, pointer, offset) in [
        (
            body[2],
            function.dfg.inst_results(body[0])[0],
            rewrite.slots,
            i32::from(plan.destination()) * 16,
        ),
        (
            body[3],
            function.dfg.inst_results(body[1])[0],
            rewrite.slots,
            i32::from(plan.destination()) * 16 + 8,
        ),
        (
            body[5],
            function.dfg.inst_results(body[4])[0],
            rewrite.view,
            32,
        ),
    ] {
        match function.dfg.insts[inst] {
            InstructionData::Store {
                opcode: Opcode::Store,
                flags,
                args,
                offset: found,
            } if function.dfg.mem_flags[flags] == MemFlagsData::trusted()
                && args == [value, pointer]
                && i32::from(found) == offset => {}
            _ => return Err(invalid()),
        }
    }
    for (inst, ty) in [(body[4], types::I64), (rewrite.call, types::I32)] {
        match function.dfg.insts[inst] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } if i64::from(imm) == 1
                && function.dfg.value_type(function.dfg.inst_results(inst)[0]) == ty => {}
            _ => return Err(invalid()),
        }
    }
    verify_guard(function, plan, rewrite)
}

fn verify_guard(function: &Function, plan: Plan, rewrite: &Rewrite) -> Result<(), JitError> {
    if function.layout.entry_block() != Some(rewrite.root)
        || function.dfg.block_params(rewrite.root).len() != 6
        || function.dfg.block_params(rewrite.original).len() != 5
        || function.dfg.block_params(rewrite.root)[5] != rewrite.view
    {
        return Err(invalid());
    }
    let args = function.dfg.block_params(rewrite.root);
    let old = function.dfg.block_params(rewrite.original);
    let ptr = function.dfg.value_type(rewrite.slots);
    if old[0] != rewrite.slots
        || old[3] != rewrite.exit
        || function.signature.params[5] != AbiParam::new(ptr)
        || args
            .iter()
            .zip(&function.signature.params)
            .any(|(&arg, param)| function.dfg.value_type(arg) != param.value_type)
        || !function.dfg.block_params(rewrite.check).is_empty()
        || !function.dfg.block_params(rewrite.refused).is_empty()
    {
        return Err(invalid());
    }
    let mut root = function.layout.block_insts(rewrite.root);
    let condition = compare(function, &mut root, IntCC::NotEqual, rewrite.view, 0)?;
    branch(
        function,
        root.next().ok_or_else(invalid)?,
        condition,
        rewrite.check,
        &[],
        rewrite.refused,
    )?;
    if root.next().is_some() {
        return Err(invalid());
    }
    let mut check = function.layout.block_insts(rewrite.check);
    let mut valid = constant(function, check.next().ok_or_else(invalid)?, types::I8, 1)?;
    for (offset, expected) in [(0, plan.binding()[0]), (8, plan.binding()[1]), (32, 0)] {
        let value = load(
            function,
            check.next().ok_or_else(invalid)?,
            rewrite.view,
            offset,
        )?;
        let same = compare(function, &mut check, IntCC::Equal, value, expected as i64)?;
        valid = and(function, check.next().ok_or_else(invalid)?, valid, same)?;
    }
    let tag = load(
        function,
        check.next().ok_or_else(invalid)?,
        rewrite.view,
        16,
    )?;
    let admitted = compare(
        function,
        &mut check,
        IntCC::UnsignedLessThanOrEqual,
        tag,
        abi::REFERENCE as i64,
    )?;
    valid = and(function, check.next().ok_or_else(invalid)?, valid, admitted)?;
    branch(
        function,
        check.next().ok_or_else(invalid)?,
        valid,
        rewrite.original,
        &args[..5],
        rewrite.refused,
    )?;
    if check.next().is_some() {
        return Err(invalid());
    }
    let mut refused = function.layout.block_insts(rewrite.refused);
    let zero = constant(function, refused.next().ok_or_else(invalid)?, types::I32, 0)?;
    for (value, offset) in [(args[1], 0), (zero, 8), (zero, 12)] {
        store(
            function,
            refused.next().ok_or_else(invalid)?,
            value,
            args[3],
            offset,
        )?;
    }
    let done = refused.next().ok_or_else(invalid)?;
    if function.dfg.insts[done].opcode() != Opcode::Return
        || !function.dfg.inst_args(done).is_empty()
        || refused.next().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

fn output(
    function: &Function,
    inst: Inst,
    ty: cranelift_codegen::ir::Type,
) -> Result<IrValue, JitError> {
    let values = function.dfg.inst_results(inst);
    if values.len() != 1 || function.dfg.value_type(values[0]) != ty {
        return Err(invalid());
    }
    Ok(values[0])
}
fn constant(
    function: &Function,
    inst: Inst,
    ty: cranelift_codegen::ir::Type,
    expected: i64,
) -> Result<IrValue, JitError> {
    match function.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } if i64::from(imm) == expected => output(function, inst, ty),
        _ => Err(invalid()),
    }
}
fn load(
    function: &Function,
    inst: Inst,
    pointer: IrValue,
    expected: i32,
) -> Result<IrValue, JitError> {
    match function.dfg.insts[inst] {
        InstructionData::Load {
            opcode: Opcode::Load,
            flags,
            arg,
            offset,
        } if function.dfg.mem_flags[flags] == MemFlagsData::trusted()
            && arg == pointer
            && i32::from(offset) == expected =>
        {
            output(function, inst, types::I64)
        }
        _ => Err(invalid()),
    }
}
fn store(
    function: &Function,
    inst: Inst,
    value: IrValue,
    pointer: IrValue,
    expected: i32,
) -> Result<(), JitError> {
    match function.dfg.insts[inst] {
        InstructionData::Store {
            opcode: Opcode::Store,
            flags,
            args,
            offset,
        } if function.dfg.mem_flags[flags] == MemFlagsData::trusted()
            && args == [value, pointer]
            && i32::from(offset) == expected =>
        {
            Ok(())
        }
        _ => Err(invalid()),
    }
}
fn compare(
    function: &Function,
    nodes: &mut impl Iterator<Item = Inst>,
    expected: IntCC,
    value: IrValue,
    immediate: i64,
) -> Result<IrValue, JitError> {
    let rhs = constant(
        function,
        nodes.next().ok_or_else(invalid)?,
        function.dfg.value_type(value),
        immediate,
    )?;
    let inst = nodes.next().ok_or_else(invalid)?;
    match function.dfg.insts[inst] {
        InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args,
        } if cond == expected && args == [value, rhs] => output(function, inst, types::I8),
        _ => Err(invalid()),
    }
}
fn and(function: &Function, inst: Inst, a: IrValue, b: IrValue) -> Result<IrValue, JitError> {
    match function.dfg.insts[inst] {
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } if args == [a, b] => output(function, inst, types::I8),
        _ => Err(invalid()),
    }
}
fn branch(
    function: &Function,
    inst: Inst,
    condition: IrValue,
    yes: Block,
    forwarded: &[IrValue],
    no: Block,
) -> Result<(), JitError> {
    match &function.dfg.insts[inst] {
        InstructionData::Brif {
            opcode: Opcode::Brif,
            arg,
            blocks,
        } if *arg == condition
            && blocks[0].block(&function.dfg.value_lists) == yes
            && blocks[1].block(&function.dfg.value_lists) == no
            && blocks[0]
                .args(&function.dfg.value_lists)
                .eq(forwarded.iter().copied().map(BlockArg::Value))
            && blocks[1].args(&function.dfg.value_lists).len() == 0 =>
        {
            Ok(())
        }
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests;
