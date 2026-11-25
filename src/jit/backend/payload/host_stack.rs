use super::*;
use cranelift_codegen::ir::{StackSlot, StackSlotData, StackSlotKind, ValueDef};

#[derive(Clone, Copy)]
struct Reload {
    address: Inst,
    address_value: IrValue,
    load: Inst,
    value: IrValue,
    user: Inst,
}

#[cfg(test)]
pub(super) fn workspace(uses: usize) -> usize {
    uses * std::mem::size_of::<Reload>()
}

pub(super) struct Copy {
    host: IrValue,
    slot: StackSlot,
    address: Inst,
    address_value: IrValue,
    store: Inst,
    reloads: BudgetVec<Reload, BudgetAllocator>,
}

fn flags() -> MemFlagsData {
    let mut flags = MemFlagsData::new();
    flags.set_notrap();
    flags
}

fn references(f: &Function, inst: Inst, host: IrValue) -> bool {
    f.dfg
        .inst_args(inst)
        .iter()
        .any(|&arg| f.dfg.resolve_aliases(arg) == host)
}

pub(super) fn lower(
    f: &mut Function,
    host: IrValue,
    allocator: BudgetAllocator,
    expansion: crate::jit::work::Expansion,
) -> Result<Option<Copy>, CompileError> {
    let mut calls = false;
    let mut uses = 0usize;
    for block in f.layout.blocks() {
        for inst in f.layout.block_insts(block) {
            if !references(f, inst, host) {
                continue;
            }
            calls |= f.dfg.insts[inst].opcode() == Opcode::CallIndirect;
            uses += 1;
        }
    }
    if !calls {
        return Ok(None);
    }
    let entry = f.layout.entry_block().ok_or_else(invalid)?;
    if !matches!(f.dfg.value_def(host), ValueDef::Param(block, _) if block == entry)
        || f.dfg.value_type(host) != types::I64
    {
        return Err(invalid());
    }
    for block in f.layout.blocks() {
        for inst in f.layout.block_insts(block) {
            if f.dfg.insts[inst]
                .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                .iter()
                .any(|b| {
                    b.args(&f.dfg.value_lists).any(
                        |v| matches!(v, BlockArg::Value(v) if f.dfg.resolve_aliases(v) == host),
                    )
                })
            {
                return Err(invalid());
            }
            if references(f, inst, host)
                && !matches!(
                    f.dfg.insts[inst].opcode(),
                    Opcode::Load | Opcode::CallIndirect | Opcode::Icmp
                )
            {
                return Err(invalid());
            }
        }
    }
    let (instructions, blocks) = size(f);
    let expected = uses
        .checked_add(1)
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(instructions))
        .ok_or(CompileError::ResourceLimit("payload host copy work"))?;
    expansion.verify_actual(expected, blocks)?;
    let mut reloads = BudgetVec::new_in(allocator);
    reloads
        .try_reserve_exact(uses)
        .map_err(|_| CompileError::ResourceLimit("payload host copy records"))?;
    let slot = f.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let mut block = f.layout.entry_block();
    while let Some(current) = block {
        block = f.layout.next_block(current);
        let mut instruction = f.layout.first_inst(current);
        while let Some(inst) = instruction {
            instruction = f.layout.next_inst(inst);
            if !references(f, inst, host) {
                continue;
            }
            let mut c = FuncCursor::new(f);
            c.goto_inst(inst);
            let address_value = c.ins().stack_addr(types::I64, slot, 0);
            let value = c.ins().load(types::I64, flags(), address_value, 0);
            let address = c.func.dfg.value_def(address_value).inst().unwrap();
            let load = c.func.dfg.value_def(value).inst().unwrap();
            for index in 0..c.func.dfg.inst_args(inst).len() {
                if c.func
                    .dfg
                    .resolve_aliases(c.func.dfg.inst_args(inst)[index])
                    == host
                {
                    c.func.dfg.inst_args_mut(inst)[index] = value;
                }
            }
            reloads.push(Reload {
                address,
                address_value,
                load,
                value,
                user: inst,
            });
        }
    }
    let first = f.layout.first_inst(entry).ok_or_else(invalid)?;
    let mut c = FuncCursor::new(f);
    c.goto_inst(first);
    let address_value = c.ins().stack_addr(types::I64, slot, 0);
    let store = c.ins().store(flags(), host, address_value, 0);
    let address = c.func.dfg.value_def(address_value).inst().unwrap();
    if size(c.func) != (expected, blocks) || reloads.len() != uses {
        return Err(invalid());
    }
    Ok(Some(Copy {
        host,
        slot,
        address,
        address_value,
        store,
        reloads,
    }))
}

fn address(f: &Function, inst: Inst, slot: StackSlot, value: IrValue) -> Result<(), CompileError> {
    if !matches!(f.dfg.insts[inst], InstructionData::StackAddr { opcode: Opcode::StackAddr, stack_slot, offset }
        if stack_slot == slot && i32::from(offset) == 0)
        || f.dfg.inst_results(inst) != [value]
        || f.dfg.value_type(value) != types::I64
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify(f: &Function, copy: &Copy) -> Result<(), CompileError> {
    if copy.reloads.windows(2).any(|r| {
        r[0].address_value.as_u32() >= r[1].address_value.as_u32()
            || r[0].value.as_u32() >= r[1].value.as_u32()
    }) || f.sized_stack_slots[copy.slot] != StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3)
        || f.layout.entry_block().and_then(|b| f.layout.first_inst(b)) != Some(copy.address)
        || f.layout.next_inst(copy.address) != Some(copy.store)
    {
        return Err(invalid());
    }
    address(f, copy.address, copy.slot, copy.address_value)?;
    if !matches!(f.dfg.insts[copy.store], InstructionData::Store { opcode: Opcode::Store, args, flags: actual, offset }
        if args == [copy.host, copy.address_value] && f.dfg.mem_flags[actual] == flags() && i32::from(offset) == 0)
    {
        return Err(invalid());
    }
    for r in &copy.reloads {
        address(f, r.address, copy.slot, r.address_value)?;
        if f.layout.next_inst(r.address) != Some(r.load)
            || f.layout.next_inst(r.load) != Some(r.user)
            || !matches!(f.dfg.insts[r.load], InstructionData::Load { opcode: Opcode::Load, arg, flags: actual, offset }
                if arg == r.address_value && f.dfg.mem_flags[actual] == flags() && i32::from(offset) == 0)
            || f.dfg.inst_results(r.load) != [r.value]
            || f.dfg.value_type(r.value) != types::I64
            || !f.dfg.inst_args(r.user).contains(&r.value)
        {
            return Err(invalid());
        }
    }
    let allowed = |value: IrValue, user: Inst| {
        let value = f.dfg.resolve_aliases(value);
        if value == copy.host {
            return user == copy.store;
        }
        if value == copy.address_value {
            return user == copy.store;
        }
        if let Ok(i) = copy
            .reloads
            .binary_search_by_key(&value.as_u32(), |r| r.address_value.as_u32())
        {
            return user == copy.reloads[i].load;
        }
        if let Ok(i) = copy
            .reloads
            .binary_search_by_key(&value.as_u32(), |r| r.value.as_u32())
        {
            return user == copy.reloads[i].user;
        }
        true
    };
    let mut addresses = 0;
    for block in f.layout.blocks() {
        for inst in f.layout.block_insts(block) {
            if matches!(f.dfg.insts[inst], InstructionData::StackAddr { stack_slot, .. } if stack_slot == copy.slot)
            {
                let value = f.dfg.first_result(inst);
                if inst != copy.address
                    && copy
                        .reloads
                        .binary_search_by_key(&value.as_u32(), |r| r.address_value.as_u32())
                        .is_err()
                {
                    return Err(invalid());
                }
                addresses += 1;
            }
            if f.dfg.inst_args(inst).iter().any(|&v| !allowed(v, inst))
                || f.dfg.insts[inst]
                    .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                    .iter()
                    .any(|b| {
                        b.args(&f.dfg.value_lists)
                            .any(|v| matches!(v, BlockArg::Value(v) if !allowed(v, inst)))
                    })
            {
                return Err(invalid());
            }
        }
    }
    if addresses != copy.reloads.len() + 1 {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
#[path = "host_stack_tests.rs"]
mod tests;
