use super::*;

pub(super) fn emit(f: &mut Function, slots: IrValue, read: Read) {
    let mut c = FuncCursor::new(f);
    c.goto_inst(read.inst);
    let base = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        std::mem::offset_of!(Header, base) as i32,
    );
    let displacement = c.ins().iconst(
        types::I64,
        (read.index * std::mem::size_of::<crate::Value<'_>>() + read.uniform.unwrap()) as i64,
    );
    c.func.replace(read.inst).iadd(base, displacement);
}

pub(super) fn verify(f: &Function, slots: IrValue, read: Read) -> Result<(), CompileError> {
    let displacement = f.layout.prev_inst(read.inst).ok_or_else(invalid)?;
    let base = f.layout.prev_inst(displacement).ok_or_else(invalid)?;
    let base = reads::load(
        f,
        base,
        slots,
        std::mem::offset_of!(Header, base) as i32,
        types::I64,
    )?;
    let displacement = reads::constant(
        f,
        displacement,
        (read.index * std::mem::size_of::<crate::Value<'_>>() + read.uniform.ok_or_else(invalid)?)
            as i64,
    )?;
    if reads::value(
        f,
        read.inst,
        Opcode::Iadd,
        &[base, displacement],
        types::I64,
    )? != read.result
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify_plan(
    source: &Function,
    candidate: &Function,
    slots: IrValue,
    records: &[Read],
    analysis: &mut same_tag::Analysis,
) -> Result<(), CompileError> {
    for &read in records {
        if read.numeric && !analysis.verify_numeric(source, slots, read.inst, read.index as u32) {
            return Err(invalid());
        }
        super::verify(candidate, slots, read)?;
    }
    if source.name != candidate.name
        || source.debug_tags != candidate.debug_tags
        || source.params != candidate.params
        || source.signature != candidate.signature
        || source.sized_stack_slots != candidate.sized_stack_slots
        || source.dynamic_stack_slots != candidate.dynamic_stack_slots
        || source.global_values != candidate.global_values
        || source.stack_limit != candidate.stack_limit
        || source.dfg.dynamic_types != candidate.dfg.dynamic_types
        || source.dfg.signatures != candidate.dfg.signatures
        || source.dfg.ext_funcs != candidate.dfg.ext_funcs
        || source.dfg.constants != candidate.dfg.constants
        || source.dfg.immediates != candidate.dfg.immediates
        || source.dfg.exception_tables != candidate.dfg.exception_tables
        || source.dfg.mem_flags != candidate.dfg.mem_flags
        || source.dfg.alias_regions != candidate.dfg.alias_regions
        || source.dfg.values_labels != candidate.dfg.values_labels
        || !source.dfg.blocks.iter().eq(candidate.dfg.blocks.iter())
        || !source.layout.blocks().eq(candidate.layout.blocks())
    {
        return Err(invalid());
    }
    for (value, definition) in source.dfg.values_and_defs() {
        if !candidate.dfg.value_is_real(value)
            || candidate.dfg.value_def(value) != definition
            || candidate.dfg.value_type(value) != source.dfg.value_type(value)
            || candidate.dfg.resolve_aliases(value) != source.dfg.resolve_aliases(value)
        {
            return Err(invalid());
        }
    }
    let mut records = records.iter().peekable();
    for block in source.layout.blocks() {
        if source.dfg.block_params(block) != candidate.dfg.block_params(block) {
            return Err(invalid());
        }
        let mut actual = candidate.layout.block_insts(block);
        for inst in source.layout.block_insts(block) {
            let rewritten = records.peek().is_some_and(|read| read.inst == inst);
            if rewritten {
                for _ in 0..records.next().unwrap().extra() {
                    actual.next().ok_or_else(invalid)?;
                }
            }
            if actual.next() != Some(inst)
                || source.srclocs[inst] != candidate.srclocs[inst]
                || source.dfg.inst_results(inst) != candidate.dfg.inst_results(inst)
                || source.dfg.user_stack_map_entries(inst)
                    != candidate.dfg.user_stack_map_entries(inst)
                || (!rewritten
                    && (source.dfg.insts[inst] != candidate.dfg.insts[inst]
                        || source.dfg.inst_args(inst) != candidate.dfg.inst_args(inst)))
            {
                return Err(invalid());
            }
            let left = source.dfg.insts[inst]
                .branch_destination(&source.dfg.jump_tables, &source.dfg.exception_tables);
            let right = candidate.dfg.insts[inst]
                .branch_destination(&candidate.dfg.jump_tables, &candidate.dfg.exception_tables);
            if left.len() != right.len()
                || left.iter().zip(right).any(|(a, b)| {
                    a.block(&source.dfg.value_lists) != b.block(&candidate.dfg.value_lists)
                        || !a
                            .args(&source.dfg.value_lists)
                            .eq(b.args(&candidate.dfg.value_lists))
                })
            {
                return Err(invalid());
            }
        }
        if actual.next().is_some() {
            return Err(invalid());
        }
    }
    if records.next().is_some() {
        return Err(invalid());
    }
    Ok(())
}
