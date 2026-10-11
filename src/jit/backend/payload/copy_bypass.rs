use super::*;

#[cfg(test)]
#[path = "copy_bypass_tests.rs"]
mod tests;

pub(super) struct Bypass {
    read: usize,
    write: usize,
    prefix: Block,
    check: Block,
}

pub(super) fn candidate(
    f: &Function,
    slots: IrValue,
    tag_store: Inst,
    bits_store: Inst,
    dest: u32,
    tag: IrValue,
    bits: IrValue,
) -> bool {
    let Some(source) = copy::source(f, slots, tag, bits) else {
        return false;
    };
    if source == dest || dest >= 256 || f.layout.next_inst(tag_store) != Some(bits_store) {
        return false;
    }
    let Some(tag_load) = f.dfg.value_def(tag).inst() else {
        return false;
    };
    let Some(bits_load) = f.dfg.value_def(bits).inst() else {
        return false;
    };
    let Some(direct) = f.layout.inst_block(tag_store) else {
        return false;
    };
    if f.layout.prev_inst(bits_load) != Some(tag_load)
        || f.layout.first_inst(direct) != Some(tag_store)
        || !f.dfg.block_params(direct).is_empty()
    {
        return false;
    }
    let Some(constant) = f.layout.next_inst(bits_load) else {
        return false;
    };
    let Some(compare) = f.layout.next_inst(constant) else {
        return false;
    };
    let Some(branch) = f.layout.next_inst(compare) else {
        return false;
    };
    let Ok(reference) = reads::constant(f, constant, abi::REFERENCE as i64) else {
        return false;
    };
    let Ok(scalar) = reads::compare(f, compare, IntCC::NotEqual, [tag, reference]) else {
        return false;
    };
    let InstructionData::Brif { blocks, .. } = f.dfg.insts[branch] else {
        return false;
    };
    let rejected = blocks[1].block(&f.dfg.value_lists);
    rejected != direct
        && f.layout.next_inst(branch).is_none()
        && reads::branch(f, branch, scalar, direct, rejected).is_ok()
}

fn sole_use(f: &Function, bits: IrValue, store: Inst, remaining: &mut usize) -> bool {
    let mut found = false;
    for block in f.layout.blocks() {
        for inst in f.layout.block_insts(block) {
            let Some(next) = remaining.checked_sub(1) else {
                return false;
            };
            *remaining = next;
            for &arg in f.dfg.inst_args(inst) {
                let Some(next) = remaining.checked_sub(1) else {
                    return false;
                };
                *remaining = next;
                if f.dfg.resolve_aliases(arg) == bits {
                    if inst != store || found {
                        return false;
                    }
                    found = true;
                }
            }
            for edge in
                f.dfg.insts[inst].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                for arg in edge.args(&f.dfg.value_lists) {
                    let Some(next) = remaining.checked_sub(1) else {
                        return false;
                    };
                    *remaining = next;
                    if matches!(arg, BlockArg::Value(v) if f.dfg.resolve_aliases(v) == bits) {
                        return false;
                    }
                }
            }
        }
    }
    found
}

pub(super) fn apply(
    source: &Function,
    f: &mut Function,
    slots: IrValue,
    reads: &mut [Read],
    writes: &[Write],
    records: &mut BudgetVec<Bypass, BudgetAllocator>,
    remaining: &mut usize,
) -> Result<(), JitError> {
    for (write_index, write) in writes.iter().enumerate() {
        let Write::Checked(write) = write else {
            continue;
        };
        let Some((proof, slow)) = writes::copy_site(write) else {
            continue;
        };
        let (tag_store, bits_store, dest, tag, bits) = proof.site();
        if !candidate(source, slots, tag_store, bits_store, dest, tag, bits)
            || !sole_use(source, bits, bits_store, remaining)
        {
            continue;
        }
        for (read_index, read) in reads.iter_mut().enumerate() {
            let Some(next) = remaining.checked_sub(1) else {
                break;
            };
            *remaining = next;
            let Read::Scalar(read) = read else { continue };
            if !reads::matches_copy(read, bits, proof.source()) {
                continue;
            }
            let prefix = reads::split_copy_entry(f, read)?;
            let before = reads::copy_entry(read);
            let after = reads::copy_exit(read);
            let check = reads::block(f);
            let [reference, compare, branch] = shape(f.layout.block_insts(after))?;
            let InstructionData::Brif { arg, blocks, .. } = f.dfg.insts[branch] else {
                return Err(invalid());
            };
            let direct = blocks[0].block(&f.dfg.value_lists);
            let rejected = blocks[1].block(&f.dfg.value_lists);
            for inst in [reference, compare, branch] {
                f.layout.remove_inst(inst);
                f.layout.append_inst(inst, prefix);
            }
            f.replace(branch).brif(arg, check, &[], rejected, &[]);
            let mut c = FuncCursor::new(f);
            c.goto_bottom(after);
            c.ins().jump(direct, &[]);
            c.goto_bottom(check);
            let actual = c
                .ins()
                .load(types::I64, MemFlagsData::new(), slots, dest as i32 * 16);
            let different = c.ins().icmp(IntCC::NotEqual, actual, tag);
            c.ins().brif(different, slow, &[], before, &[]);
            if records.len() == records.capacity() {
                return Err(invalid());
            }
            records.push(Bypass {
                read: read_index,
                write: write_index,
                prefix,
                check,
            });
            break;
        }
    }
    Ok(())
}

pub(super) fn verify(
    source: &Function,
    f: &Function,
    slots: IrValue,
    reads: &[Read],
    writes: &[Write],
    record: &Bypass,
    predecessors: &crate::jit::preds::Predecessors,
    remaining: &mut usize,
) -> Result<(), JitError> {
    let Some(Read::Scalar(read)) = reads.get(record.read) else {
        return Err(invalid());
    };
    let Some(Write::Checked(write)) = writes.get(record.write) else {
        return Err(invalid());
    };
    let (proof, slow) = writes::copy_site(write).ok_or_else(invalid)?;
    let (tag_store, bits_store, dest, tag, bits) = proof.site();
    if !candidate(source, slots, tag_store, bits_store, dest, tag, bits)
        || !sole_use(source, bits, bits_store, remaining)
        || !reads::matches_copy(read, bits, proof.source())
    {
        return Err(invalid());
    }
    let mut tail: [Inst; 4] = shape(f.layout.block_insts(record.prefix).rev().take(4))?;
    tail.reverse();
    let [source_tag, reference, scalar, guard] = tail;
    if reads::load(f, source_tag, slots, proof.source() as i32 * 16, types::I64)? != tag {
        return Err(invalid());
    }
    let reference = reads::constant(f, reference, abi::REFERENCE as i64)?;
    let scalar = reads::compare(f, scalar, IntCC::NotEqual, [tag, reference])?;
    let source_bits = source.dfg.value_def(bits).inst().ok_or_else(invalid)?;
    let original_constant = source.layout.next_inst(source_bits).ok_or_else(invalid)?;
    let original_compare = source
        .layout
        .next_inst(original_constant)
        .ok_or_else(invalid)?;
    let original_branch = source
        .layout
        .next_inst(original_compare)
        .ok_or_else(invalid)?;
    let InstructionData::Brif { blocks, .. } = source.dfg.insts[original_branch] else {
        return Err(invalid());
    };
    let direct = blocks[0].block(&source.dfg.value_lists);
    let rejected = blocks[1].block(&source.dfg.value_lists);
    if tail[1..] != [original_constant, original_compare, original_branch] {
        return Err(invalid());
    }
    reads::branch(f, guard, scalar, record.check, rejected)?;
    let [actual, different, branch] = shape(f.layout.block_insts(record.check))?;
    let actual = reads::load(f, actual, slots, dest as i32 * 16, types::I64)?;
    let different = reads::compare(f, different, IntCC::NotEqual, [actual, tag])?;
    let before = reads::copy_entry(read);
    reads::branch(f, branch, different, slow, before)?;
    let [resume] = shape(f.layout.block_insts(reads::copy_exit(read)))?;
    let InstructionData::Jump { destination, .. } = f.dfg.insts[resume] else {
        return Err(invalid());
    };
    if destination.block(&f.dfg.value_lists) != direct
        || destination.args(&f.dfg.value_lists).next().is_some()
    {
        return Err(invalid());
    }
    for (block, parent) in [(before, branch), (record.check, guard)] {
        if f.layout.entry_block() == Some(block) || !f.dfg.block_params(block).is_empty() {
            return Err(invalid());
        }
        let mut incoming = predecessors.pred_iter(block);
        if incoming.next().map(|p| p.inst) != Some(parent) || incoming.next().is_some() {
            return Err(invalid());
        }
    }
    let [head, nil] = writes::copy_predecessors(f, write).ok_or_else(invalid)?;
    let mut count = 0;
    for parent in predecessors.pred_iter(slow) {
        if ![head, nil, branch].contains(&parent.inst) {
            return Err(invalid());
        }
        count += 1;
    }
    if count != 3 || head == nil || head == branch || nil == branch {
        return Err(invalid());
    }
    Ok(())
}
