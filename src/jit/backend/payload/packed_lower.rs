use super::*;
use crate::jit::abi::payload::packed::scoped::bridge::entry;
use crate::jit::work::Expansion;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::SigRef;

pub(super) struct Fragment<'a> {
    pub source: Inst,
    pub first: Inst,
    pub next: Option<Inst>,
    pub blocks: &'a [Block],
}

enum Record {
    Read(packed_reads::Read),
    Write(packed_writes::Write),
    Call(packed_calls::Call),
    Return(Inst),
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    instructions: usize,
    blocks: usize,
    writes: bool,
    calls: bool,
}

pub(in crate::jit::backend) struct Candidate {
    pub function: Function,
    records: BudgetVec<Option<Record>, BudgetAllocator>,
    _signatures: Reservation,
}

impl Candidate {
    pub(in crate::jit::backend) fn install(self, target: &mut Function) -> Reservation {
        let Self {
            function,
            records,
            _signatures,
        } = self;
        drop(records);
        *target = function;
        _signatures
    }
}

fn add(left: usize, right: usize) -> Result<usize, JitError> {
    left.checked_add(right)
        .ok_or(JitError::ResourceLimit("packed expansion overflow"))
}

fn plan(
    source: &Function,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    helpers: &[(u32, FuncRef)],
    expansion: Expansion,
) -> Result<Plan, JitError> {
    let params = [types::I64, types::I64, types::I32, types::I64, types::I64];
    let block = source.layout.entry_block().ok_or_else(invalid)?;
    if source
        .signature
        .params
        .iter()
        .copied()
        .ne(params.map(AbiParam::new))
        || !source.signature.returns.is_empty()
        || source.dfg.block_params(block).len() != 5
        || source.dfg.block_params(block)[0] != slots
        || source.dfg.block_params(block)[4] != host
    {
        return Err(invalid());
    }
    expansion.verify_actual(source.dfg.num_insts(), source.dfg.num_blocks())?;
    let (instructions, blocks) = size(source);
    let mut result = Plan {
        instructions,
        blocks,
        writes: false,
        calls: false,
    };
    scan_with_tags(source, slots, host, registers, helpers, true, |rewrite| {
        let (instructions, blocks) = match rewrite {
            Rewrite::Read(inst, _) => {
                packed_reads::admit(source, inst, slots, registers)?;
                let InstructionData::Load { offset, .. } = source.dfg.insts[inst] else {
                    return Err(invalid());
                };
                if i32::from(offset) % 16 == 0 {
                    (20, 0)
                } else {
                    (
                        16 + usize::from(
                            source.dfg.value_type(source.dfg.first_result(inst)) == types::F64,
                        ),
                        3,
                    )
                }
            }
            Rewrite::Write {
                tag_store, bits, ..
            } => {
                packed_writes::admit(source, tag_store, slots, host, registers, false)?;
                result.writes = true;
                (
                    24 + usize::from(source.dfg.value_type(bits) == types::F64),
                    4,
                )
            }
            Rewrite::Helper(inst, ..) => {
                packed_calls::admit(source, inst, slots, host, helpers)?;
                result.calls = true;
                (7, 2)
            }
        };
        result.instructions = add(result.instructions, instructions)?;
        result.blocks = add(result.blocks, blocks)?;
        Ok(())
    })?;
    for block in source.layout.blocks() {
        for inst in source.layout.block_insts(block) {
            let data = source.dfg.insts[inst];
            if data.opcode() == Opcode::Return {
                if !source.dfg.inst_args(inst).is_empty() {
                    return Err(invalid());
                }
                result.instructions = add(result.instructions, 1)?;
            }
            if source
                .dfg
                .inst_args(inst)
                .iter()
                .any(|&v| source.dfg.resolve_aliases(v) == host)
                && !(data.opcode() == Opcode::Call && source.dfg.inst_args(inst)[0] == host)
                && !null_test(source, inst, host)
            {
                return Err(invalid());
            }
        }
    }
    expansion.verify_actual(
        add(instructions, result.instructions)?,
        add(blocks, result.blocks)?,
    )?;
    Ok(result)
}

fn null_test(f: &Function, inst: Inst, host: IrValue) -> bool {
    let InstructionData::IntCompare {
        cond: IntCC::Equal | IntCC::NotEqual,
        args,
        ..
    } = f.dfg.insts[inst]
    else {
        return false;
    };
    let other = if args[0] == host {
        args[1]
    } else if args[1] == host {
        args[0]
    } else {
        return false;
    };
    let Some(constant) = f.dfg.value_def(other).inst() else {
        return false;
    };
    reads::constant(f, constant, 0).is_ok_and(|value| value == other)
}

fn signature(f: &mut Function, params: &[cranelift_codegen::ir::Type]) -> Result<SigRef, JitError> {
    let mut signature = Signature::new(f.signature.call_conv);
    signature
        .params
        .try_reserve_exact(params.len())
        .map_err(|_| JitError::ResourceLimit("packed signature"))?;
    signature
        .returns
        .try_reserve_exact(1)
        .map_err(|_| JitError::ResourceLimit("packed signature"))?;
    signature
        .params
        .extend(params.iter().copied().map(AbiParam::new));
    signature.returns.push(AbiParam::new(types::I32));
    if signature.params.capacity() != params.len() || signature.returns.capacity() != 1 {
        return Err(JitError::ResourceLimit("packed signature capacity"));
    }
    Ok(f.import_signature(signature))
}

pub(in crate::jit::backend) fn lower(
    source: &Function,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    helpers: &[(u32, FuncRef)],
    allocator: BudgetAllocator,
    expansion: Expansion,
) -> Result<Candidate, JitError> {
    let plan = plan(source, slots, host, registers, helpers, expansion)?;
    let mut records = BudgetVec::new_in(allocator.clone());
    records
        .try_reserve_exact(source.dfg.num_insts())
        .map_err(|_| JitError::ResourceLimit("packed rewrite records"))?;
    records.resize_with(source.dfg.num_insts(), || None);
    let signature_bytes =
        Layout::array::<AbiParam>(1 + 5 * usize::from(plan.writes) + 6 * usize::from(plan.calls))
            .map_err(|_| JitError::ResourceLimit("packed signature size"))?
            .size();
    let signatures = Reservation::new(allocator.0.clone(), signature_bytes)
        .map_err(|_| JitError::ResourceLimit("packed signatures"))?;
    let mut terminal =
        terminal::Analysis::new(source, slots, host, registers, helpers, allocator.clone())?;
    let mut function = source.clone();
    function
        .signature
        .returns
        .try_reserve_exact(1)
        .map_err(|_| JitError::ResourceLimit("packed entry signature"))?;
    function.signature.returns.push(AbiParam::new(types::I32));
    if function.signature.returns.capacity() != 1 {
        return Err(JitError::ResourceLimit("packed entry signature capacity"));
    }
    let writer = plan
        .writes
        .then(|| {
            signature(
                &mut function,
                &[types::I64, types::I32, types::I64, types::I64],
            )
        })
        .transpose()?;
    let helper = plan
        .calls
        .then(|| {
            signature(
                &mut function,
                &[types::I64, types::I32, types::I32, types::I32, types::I32],
            )
        })
        .transpose()?;
    scan_with_tags(source, slots, host, registers, helpers, true, |rewrite| {
        let (inst, record) = match rewrite {
            Rewrite::Read(inst, _) => {
                let input = packed_reads::admit(source, inst, slots, registers)?;
                (inst, Record::Read(packed_reads::emit(&mut function, input)))
            }
            Rewrite::Write {
                tag_store,
                bits_store,
                index,
                ..
            } => {
                let dead = terminal.admits(bits_store, index);
                let input = packed_writes::admit(source, tag_store, slots, host, registers, dead)?;
                records[bits_store.index()] = Some(Record::Removed);
                (
                    tag_store,
                    Record::Write(packed_writes::emit_with_signature(
                        &mut function,
                        input,
                        writer.ok_or_else(invalid)?,
                    )),
                )
            }
            Rewrite::Helper(inst, ..) => {
                let input = packed_calls::admit(source, inst, slots, host, helpers)?;
                (
                    inst,
                    Record::Call(packed_calls::emit_with_signature(
                        &mut function,
                        input,
                        helper.ok_or_else(invalid)?,
                    )),
                )
            }
        };
        records[inst.index()] = Some(record);
        Ok(())
    })?;
    for block in source.layout.blocks() {
        for inst in source.layout.block_insts(block) {
            if source.dfg.insts[inst].opcode() == Opcode::Return {
                let mut c = FuncCursor::new(&mut function);
                c.goto_inst(inst);
                let status = c.ins().iconst(types::I32, entry::RETURNED as i64);
                let constant = c.func.dfg.value_def(status).inst().ok_or_else(invalid)?;
                c.func.replace(inst).return_(&[status]);
                records[inst.index()] = Some(Record::Return(constant));
            }
        }
    }
    drop(terminal);
    let candidate = Candidate {
        function,
        records,
        _signatures: signatures,
    };
    verify(
        source, &candidate, slots, host, registers, helpers, allocator, expansion,
    )?;
    Ok(candidate)
}

fn unchanged(source: &Function, f: &Function, inst: Inst) -> Result<(), JitError> {
    if source.dfg.insts[inst] != f.dfg.insts[inst]
        || source.dfg.inst_args(inst) != f.dfg.inst_args(inst)
        || source.dfg.inst_results(inst) != f.dfg.inst_results(inst)
    {
        return Err(invalid());
    }
    if let InstructionData::Load { flags, .. } | InstructionData::Store { flags, .. } =
        source.dfg.insts[inst]
    {
        if source.dfg.mem_flags[flags] != f.dfg.mem_flags[flags] {
            return Err(invalid());
        }
    }
    let left = source.dfg.insts[inst]
        .branch_destination(&source.dfg.jump_tables, &source.dfg.exception_tables);
    let right = f.dfg.insts[inst].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables);
    if left.len() != right.len()
        || left.iter().zip(right).any(|(a, b)| {
            a.block(&source.dfg.value_lists) != b.block(&f.dfg.value_lists)
                || !a
                    .args(&source.dfg.value_lists)
                    .eq(b.args(&f.dfg.value_lists))
        })
    {
        return Err(invalid());
    }
    for &value in source.dfg.inst_results(inst) {
        if source.dfg.value_type(value) != f.dfg.value_type(value)
            || source.dfg.resolve_aliases(value) != f.dfg.resolve_aliases(value)
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn verify(
    source: &Function,
    candidate: &Candidate,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    helpers: &[(u32, FuncRef)],
    allocator: BudgetAllocator,
    expansion: Expansion,
) -> Result<(), JitError> {
    let plan = plan(source, slots, host, registers, helpers, expansion)?;
    let f = &candidate.function;
    let (source_instructions, source_blocks) = size(source);
    let removed = source
        .layout
        .blocks()
        .flat_map(|b| source.layout.block_insts(b))
        .filter(|&i| f.layout.inst_block(i).is_none())
        .count();
    if size(f) != (plan.instructions, plan.blocks)
        || f.dfg.num_blocks() != add(source.dfg.num_blocks(), plan.blocks - source_blocks)?
        || f.dfg.num_insts()
            != add(
                add(
                    source.dfg.num_insts(),
                    plan.instructions - source_instructions,
                )?,
                removed,
            )?
        || candidate.records.len() != source.dfg.num_insts()
        || f.signature.params != source.signature.params
        || f.signature.call_conv != source.signature.call_conv
        || f.signature.returns != [AbiParam::new(types::I32)]
        || f.layout.entry_block() != source.layout.entry_block()
        || f.dfg.signatures.len()
            != source.dfg.signatures.len() + usize::from(plan.writes) + usize::from(plan.calls)
    {
        return Err(invalid());
    }
    let mut terminal =
        terminal::Analysis::new(source, slots, host, registers, helpers, allocator.clone())?;
    let mut owned = BudgetVec::new_in(allocator);
    owned
        .try_reserve_exact(f.dfg.num_blocks())
        .map_err(|_| JitError::ResourceLimit("packed block coverage"))?;
    owned.resize(f.dfg.num_blocks(), false);
    let mut claim = |block: Block| -> Result<(), JitError> {
        let taken = owned.get_mut(block.index()).ok_or_else(invalid)?;
        if std::mem::replace(taken, true) {
            return Err(invalid());
        }
        Ok(())
    };
    scan_with_tags(source, slots, host, registers, helpers, true, |rewrite| {
        let (inst, kind) = match rewrite {
            Rewrite::Read(inst, _) => (inst, 0),
            Rewrite::Write {
                tag_store,
                bits_store,
                ..
            } => {
                if !matches!(candidate.records[bits_store.index()], Some(Record::Removed)) {
                    return Err(invalid());
                }
                (tag_store, 1)
            }
            Rewrite::Helper(inst, ..) => (inst, 2),
        };
        if !matches!(
            (&candidate.records[inst.index()], kind),
            (Some(Record::Read(_)), 0) | (Some(Record::Write(_)), 1) | (Some(Record::Call(_)), 2)
        ) {
            return Err(invalid());
        }
        Ok(())
    })?;
    for block in source.layout.blocks() {
        claim(block)?;
        if source.dfg.block_params(block) != f.dfg.block_params(block) {
            return Err(invalid());
        }
        for &value in source.dfg.block_params(block) {
            if source.dfg.value_type(value) != f.dfg.value_type(value)
                || source.dfg.resolve_aliases(value) != f.dfg.resolve_aliases(value)
            {
                return Err(invalid());
            }
        }
        let mut cursor = f.layout.first_inst(block);
        let mut removed = None;
        for inst in source.layout.block_insts(block) {
            let fragment = match candidate.records[inst.index()].as_ref() {
                Some(Record::Read(read)) => {
                    packed_reads::verify_source(source, read, slots, registers)?;
                    packed_reads::verify(f, read)?;
                    packed_reads::fragment(f, read)?
                }
                Some(Record::Write(write)) => {
                    packed_writes::verify_source(
                        source,
                        write,
                        slots,
                        host,
                        registers,
                        &mut terminal,
                    )?;
                    packed_writes::verify(f, write)?;
                    removed = source.layout.next_inst(inst);
                    packed_writes::fragment(f, write)?
                }
                Some(Record::Call(call)) => {
                    packed_calls::verify_source(source, call, slots, host, helpers)?;
                    packed_calls::verify(f, call)?;
                    packed_calls::fragment(f, call)?
                }
                Some(Record::Removed) => {
                    if removed.take() != Some(inst) || f.layout.inst_block(inst).is_some() {
                        return Err(invalid());
                    }
                    continue;
                }
                Some(Record::Return(constant)) => {
                    if source.dfg.insts[inst].opcode() != Opcode::Return
                        || f.layout.next_inst(*constant) != Some(inst)
                        || f.dfg.insts[inst].opcode() != Opcode::Return
                        || f.dfg.inst_args(inst) != f.dfg.inst_results(*constant)
                        || !matches!(f.dfg.insts[*constant], InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if i64::from(imm) == entry::RETURNED as i64)
                        || f.dfg.value_type(f.dfg.first_result(*constant)) != types::I32
                    {
                        return Err(invalid());
                    }
                    Fragment {
                        source: inst,
                        first: *constant,
                        next: f.layout.next_inst(inst),
                        blocks: &[],
                    }
                }
                None => {
                    if source.dfg.insts[inst].opcode() == Opcode::Return {
                        return Err(invalid());
                    }
                    unchanged(source, f, inst)?;
                    Fragment {
                        source: inst,
                        first: inst,
                        next: f.layout.next_inst(inst),
                        blocks: &[],
                    }
                }
            };
            if fragment.source != inst || cursor != Some(fragment.first) {
                return Err(invalid());
            }
            for &block in fragment.blocks {
                claim(block)?;
            }
            cursor = fragment.next;
        }
        if cursor.is_some() || removed.is_some() {
            return Err(invalid());
        }
    }
    drop(claim);
    if f.layout.blocks().any(|block| !owned[block.index()]) {
        return Err(invalid());
    }
    Ok(())
}

#[path = "packed_lower_tests.rs"]
mod tests;
