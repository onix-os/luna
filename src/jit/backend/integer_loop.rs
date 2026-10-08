use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{
        condcodes::IntCC, types, Block, BlockArg, BlockCall, Function, InstBuilder, JumpTableData,
        MemFlagsData, Value,
    },
};

use super::super::{abi, ir::Snapshot, JitError};
use crate::opcode::{Operation, RCIndex};

const MAX_REGISTERS: usize = 16;
const MAX_OPERATIONS: usize = 32;

#[cfg(test)]
pub(super) mod audit;
mod verify;

#[derive(Clone)]
struct Region {
    plan: Plan,
    generic: Block,
    guards: Block,
    dispatch: Block,
    done: Block,
    budget_exit: Block,
    fast: [Block; MAX_OPERATIONS],
    bodies: [Block; MAX_OPERATIONS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    start: usize,
    end: usize,
    base: usize,
    used: u16,
    written: u16,
}

impl Plan {
    fn seeded(self, source: &Snapshot) -> bool {
        let mut known = 0u16;
        for (pc, op) in source.operations[..self.start].iter().copied().enumerate() {
            let write = match op {
                Operation::LoadConstant { dest, constant } => Some((
                    dest.0,
                    source.constants[usize::from(constant.0)].tag == abi::INTEGER,
                )),
                Operation::Move { dest, source } => Some((dest.0, known & (1 << source.0) != 0)),
                Operation::NumericForPrep { base, jump }
                    if usize::from(base.0) == self.base
                        && (pc + 1).checked_add_signed(isize::from(jump)) == Some(self.end) =>
                {
                    let controls = 7 << self.base;
                    if known & controls != controls {
                        return false;
                    }
                    known |= 8 << self.base;
                    None
                }
                _ => {
                    known = 0;
                    None
                }
            };
            if let Some((dest, integer)) = write {
                known &= !(1 << dest);
                if integer {
                    known |= 1 << dest;
                }
            }
        }
        for op in source.operations[self.start..self.end].iter().copied() {
            let integer = |input: RCIndex| match input {
                RCIndex::Register(index) => known & (1 << index.0) != 0,
                RCIndex::Constant(index) => {
                    source.constants[usize::from(index.0)].tag == abi::INTEGER
                }
            };
            let (dest, valid) = match op {
                Operation::Add { dest, left, right }
                | Operation::Sub { dest, left, right }
                | Operation::Mul { dest, left, right } => (dest.0, integer(left) && integer(right)),
                Operation::Move { dest, source } => (dest.0, integer(RCIndex::Register(source))),
                Operation::LoadConstant { dest, constant } => (
                    dest.0,
                    source.constants[usize::from(constant.0)].tag == abi::INTEGER,
                ),
                _ => return false,
            };
            if !valid {
                return false;
            }
            known |= 1 << dest;
        }
        known & self.used == self.used
    }

    fn new(source: &Snapshot) -> Option<Self> {
        if source.registers > MAX_REGISTERS {
            return None;
        }
        for (end, operation) in source.operations.iter().copied().enumerate() {
            let Operation::NumericForLoop { base, jump } = operation else {
                continue;
            };
            let start = (end + 1).checked_add_signed(isize::from(jump))?;
            if start >= end
                || end + 1 >= source.operations.len()
                || end - start + 1 > MAX_OPERATIONS
            {
                continue;
            }
            let base = usize::from(base.0);
            let mut plan = Self {
                start,
                end,
                base,
                used: 15 << base,
                written: 9 << base,
            };
            let mut accepted = true;
            for op in &source.operations[start..end] {
                let mut read = |input: RCIndex| match input {
                    RCIndex::Register(index) => {
                        plan.used |= 1 << index.0;
                        true
                    }
                    RCIndex::Constant(index) => {
                        source.constants[usize::from(index.0)].tag == abi::INTEGER
                    }
                };
                let dest = match *op {
                    Operation::Add { dest, left, right }
                    | Operation::Sub { dest, left, right }
                    | Operation::Mul { dest, left, right } => {
                        accepted &= read(left) && read(right);
                        dest.0
                    }
                    Operation::Move { dest, source } => {
                        read(RCIndex::Register(source));
                        dest.0
                    }
                    Operation::LoadConstant { dest, constant } => {
                        accepted &= source.constants[usize::from(constant.0)].tag == abi::INTEGER;
                        dest.0
                    }
                    _ => {
                        accepted = false;
                        break;
                    }
                };
                plan.used |= 1 << dest;
                plan.written |= 1 << dest;
            }
            if accepted && plan.used.count_ones() <= 8 && plan.seeded(source) {
                return Some(plan);
            }
        }
        None
    }

    fn state(self, function: &mut Function, block: Block, prefix: &[cranelift_codegen::ir::Type]) {
        for &ty in prefix {
            function.dfg.append_block_param(block, ty);
        }
        for _ in 0..self.used.count_ones() {
            function.dfg.append_block_param(block, types::I64);
        }
    }

    fn values(
        self,
        function: &Function,
        block: Block,
        prefix: usize,
    ) -> [Option<Value>; MAX_REGISTERS] {
        let mut values = [None; MAX_REGISTERS];
        let mut params = function.dfg.block_params(block)[prefix..].iter().copied();
        for (index, value) in values.iter_mut().enumerate() {
            if self.used & (1 << index) != 0 {
                *value = params.next();
            }
        }
        values
    }

    fn arguments(
        self,
        count: Value,
        values: &[Option<Value>; MAX_REGISTERS],
    ) -> ([BlockArg; MAX_REGISTERS + 1], usize) {
        let mut args = [BlockArg::Value(count); MAX_REGISTERS + 1];
        let mut len = 1;
        for (index, value) in values.iter().enumerate() {
            if self.used & (1 << index) != 0 {
                args[len] = value.unwrap().into();
                len += 1;
            }
        }
        (args, len)
    }

    fn flush(
        self,
        cursor: &mut FuncCursor<'_>,
        slots: Value,
        values: &[Option<Value>; MAX_REGISTERS],
    ) {
        let integer = cursor.ins().iconst(types::I64, abi::INTEGER as i64);
        for (index, value) in values.iter().enumerate() {
            if self.written & (1 << index) != 0 {
                cursor
                    .ins()
                    .store(MemFlagsData::new(), integer, slots, (index * 16) as i32);
                cursor.ins().store(
                    MemFlagsData::new(),
                    value.unwrap(),
                    slots,
                    (index * 16 + 8) as i32,
                );
            }
        }
    }
}

fn block(function: &mut Function) -> Block {
    let block = function.dfg.make_block();
    function.layout.append_block(block);
    block
}

fn operand(
    cursor: &mut FuncCursor<'_>,
    source: &Snapshot,
    values: &[Option<Value>; MAX_REGISTERS],
    input: RCIndex,
) -> Value {
    match input {
        RCIndex::Register(index) => values[usize::from(index.0)].unwrap(),
        RCIndex::Constant(index) => cursor.ins().iconst(
            types::I64,
            source.constants[usize::from(index.0)].bits as i64,
        ),
    }
}

pub(super) fn augment(
    function: &mut Function,
    source: &Snapshot,
    headers: &[Block],
    exhausted: Block,
    probe: bool,
    expansion: super::super::work::Expansion,
) -> Result<bool, JitError> {
    source.verify()?;
    let Some(plan) = Plan::new(source) else {
        return Ok(false);
    };
    let instructions: usize = function
        .layout
        .blocks()
        .map(|b| function.layout.block_insts(b).count())
        .sum();
    let length = plan.end - plan.start + 1;
    let extra = 32 + 9 * (plan.used.count_ones() as usize + length);
    if expansion
        .verify_actual(
            instructions.saturating_mul(2).saturating_add(extra),
            function
                .dfg
                .num_blocks()
                .saturating_mul(2)
                .saturating_add(5 + length * 2),
        )
        .is_err()
    {
        return Ok(false);
    }
    let mut candidate = function.clone();
    let region = emit(&mut candidate, source, headers, exhausted, probe, plan)?;
    commit(
        function, candidate, source, headers, exhausted, &region, probe, expansion,
    )?;
    Ok(true)
}

fn commit(
    function: &mut Function,
    candidate: Function,
    source: &Snapshot,
    headers: &[Block],
    exhausted: Block,
    region: &Region,
    probe: bool,
    expansion: super::super::work::Expansion,
) -> Result<(), JitError> {
    verify::check(
        function, &candidate, source, headers, exhausted, region, probe,
    )?;
    expansion.verify_actual(
        [function as &Function, &candidate]
            .into_iter()
            .map(|f| {
                f.layout
                    .blocks()
                    .map(|b| f.layout.block_insts(b).count())
                    .sum::<usize>()
            })
            .sum(),
        function.dfg.num_blocks() + candidate.dfg.num_blocks(),
    )?;
    *function = candidate;
    Ok(())
}

fn emit(
    function: &mut Function,
    source: &Snapshot,
    headers: &[Block],
    exhausted: Block,
    probe: bool,
    plan: Plan,
) -> Result<Region, JitError> {
    let entry = function
        .layout
        .entry_block()
        .ok_or_else(|| JitError::Compilation("integer loop entry".into()))?;
    let params: [Value; 5] = function
        .dfg
        .block_params(entry)
        .try_into()
        .map_err(|_| JitError::Compilation("integer loop ABI".into()))?;
    let generic = block(function);
    while let Some(inst) = function.layout.first_inst(entry) {
        function.layout.remove_inst(inst);
        function.layout.append_inst(inst, generic);
    }
    let guards = block(function);
    let dispatch = block(function);
    let done = block(function);
    let budget_exit = block(function);
    plan.state(function, done, &[types::I32]);
    plan.state(function, budget_exit, &[types::I64, types::I32]);
    let mut fast = [done; MAX_OPERATIONS];
    let mut bodies = [done; MAX_OPERATIONS];
    let length = plan.end - plan.start + 1;
    for index in 0..length {
        fast[index] = block(function);
        bodies[index] = block(function);
        plan.state(function, fast[index], &[types::I32]);
    }
    let mut cursor = FuncCursor::new(function);
    cursor.goto_bottom(entry);
    let lower = cursor.ins().icmp_imm_u(
        IntCC::UnsignedGreaterThanOrEqual,
        params[1],
        plan.start as i64,
    );
    let upper = cursor
        .ins()
        .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, params[1], plan.end as i64);
    let within = cursor.ins().band(lower, upper);
    let fuel = cursor.ins().icmp_imm_u(IntCC::NotEqual, params[2], 0);
    let ready = cursor.ins().band(within, fuel);
    cursor.ins().brif(ready, guards, &[], generic, &[]);
    cursor.goto_bottom(guards);
    let mut valid = cursor.ins().iconst(types::I8, 1);
    for index in 0..MAX_REGISTERS {
        if plan.used & (1 << index) != 0 {
            let tag = cursor.ins().load(
                types::I64,
                MemFlagsData::new(),
                params[0],
                (index * 16) as i32,
            );
            let integer = cursor
                .ins()
                .icmp_imm_u(IntCC::Equal, tag, abi::INTEGER as i64);
            valid = cursor.ins().band(valid, integer);
        }
    }
    cursor.ins().brif(valid, dispatch, &[], generic, &[]);
    cursor.goto_bottom(dispatch);
    if probe {
        let destination = cursor
            .ins()
            .load(types::I64, MemFlagsData::new(), params[4], 0);
        let reached = cursor.ins().iconst(types::I64, 1);
        cursor
            .ins()
            .store(MemFlagsData::new(), reached, destination, 0);
    }
    let mut initial = [None; MAX_REGISTERS];
    for (index, value) in initial.iter_mut().enumerate() {
        if plan.used & (1 << index) != 0 {
            *value = Some(cursor.ins().load(
                types::I64,
                MemFlagsData::new(),
                params[0],
                (index * 16 + 8) as i32,
            ));
        }
    }
    let zero = cursor.ins().iconst(types::I32, 0);
    let (args, len) = plan.arguments(zero, &initial);
    let default = BlockCall::new(generic, [], &mut cursor.func.dfg.value_lists);
    let mut branches = [default; MAX_OPERATIONS];
    for index in 0..length {
        branches[index] = BlockCall::new(
            fast[index],
            args[..len].iter().copied(),
            &mut cursor.func.dfg.value_lists,
        );
    }
    let table = cursor
        .func
        .dfg
        .jump_tables
        .push(JumpTableData::new(default, &branches[..length]));
    let offset = cursor.ins().iadd_imm_s(params[1], -(plan.start as i64));
    let index = cursor.ins().ireduce(types::I32, offset);
    cursor.ins().br_table(index, table);
    for index in 0..length {
        let pc = plan.start + index;
        let header = fast[index];
        let count = cursor.func.dfg.block_params(header)[0];
        let mut values = plan.values(cursor.func, header, 1);
        cursor.goto_bottom(header);
        let limit = cursor
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, count, params[2]);
        let source_pc = cursor.ins().iconst(types::I64, pc as i64);
        let (args, len) = plan.arguments(count, &values);
        let mut exit_args = [BlockArg::Value(source_pc); MAX_REGISTERS + 2];
        exit_args[1..len + 1].copy_from_slice(&args[..len]);
        cursor.ins().brif(
            limit,
            budget_exit,
            &exit_args[..len + 1],
            bodies[index],
            &[],
        );
        cursor.goto_bottom(bodies[index]);
        let next_count = cursor.ins().iadd_imm_u(count, 1);
        match source.operations[pc] {
            op @ (Operation::Add { dest, left, right }
            | Operation::Sub { dest, left, right }
            | Operation::Mul { dest, left, right }) => {
                let left = operand(&mut cursor, source, &values, left);
                let right = operand(&mut cursor, source, &values, right);
                values[usize::from(dest.0)] = Some(match op {
                    Operation::Add { .. } => cursor.ins().iadd(left, right),
                    Operation::Sub { .. } => cursor.ins().isub(left, right),
                    _ => cursor.ins().imul(left, right),
                });
            }
            Operation::Move { dest, source } => {
                values[usize::from(dest.0)] = values[usize::from(source.0)]
            }
            Operation::LoadConstant { dest, constant } => {
                values[usize::from(dest.0)] = Some(cursor.ins().iconst(
                    types::I64,
                    source.constants[usize::from(constant.0)].bits as i64,
                ));
            }
            Operation::NumericForLoop { .. } => {
                let base = plan.base;
                let step = values[base + 2].unwrap();
                let (next, overflow) = cursor.ins().sadd_overflow(values[base].unwrap(), step);
                let negative = cursor.ins().icmp_imm_s(IntCC::SignedLessThan, step, 0);
                let ge = cursor.ins().icmp(
                    IntCC::SignedGreaterThanOrEqual,
                    next,
                    values[base + 1].unwrap(),
                );
                let le = cursor.ins().icmp(
                    IntCC::SignedLessThanOrEqual,
                    next,
                    values[base + 1].unwrap(),
                );
                let in_range = cursor.ins().select(negative, ge, le);
                let no_overflow = cursor.ins().bxor_imm_u(overflow, 1);
                let taken = cursor.ins().band(in_range, no_overflow);
                values[base] = Some(next);
                let (done_args, done_len) = plan.arguments(next_count, &values);
                values[base + 3] = Some(next);
                let (back_args, back_len) = plan.arguments(next_count, &values);
                cursor.ins().brif(
                    taken,
                    fast[0],
                    &back_args[..back_len],
                    done,
                    &done_args[..done_len],
                );
                continue;
            }
            _ => unreachable!(),
        }
        let (args, len) = plan.arguments(next_count, &values);
        cursor.ins().jump(fast[index + 1], &args[..len]);
    }
    cursor.goto_bottom(done);
    let count = cursor.func.dfg.block_params(done)[0];
    let values = plan.values(cursor.func, done, 1);
    plan.flush(&mut cursor, params[0], &values);
    cursor.ins().jump(headers[plan.end + 1], &[count.into()]);
    cursor.goto_bottom(budget_exit);
    let pc = cursor.func.dfg.block_params(budget_exit)[0];
    let count = cursor.func.dfg.block_params(budget_exit)[1];
    let values = plan.values(cursor.func, budget_exit, 2);
    plan.flush(&mut cursor, params[0], &values);
    cursor.ins().jump(exhausted, &[pc.into(), count.into()]);
    Ok(Region {
        plan,
        generic,
        guards,
        dispatch,
        done,
        budget_exit,
        fast,
        bodies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(source: &[u8]) -> Snapshot {
        crate::Lua::empty().enter(|ctx| {
            let proto = crate::FunctionPrototype::compile(ctx, "integer-loop", source).unwrap();
            Snapshot::new(&proto, 4096, 2 * 1024 * 1024).unwrap()
        })
    }

    #[test]
    fn recognizes_source_loop_edges_and_integer_operands() {
        let source = snapshot(b"local s=0 for i=1,100 do s=s+i end return s");
        let plan = Plan::new(&source).unwrap();
        let Operation::NumericForLoop { base, jump } = source.operations[plan.end] else {
            unreachable!()
        };
        assert_eq!(
            plan.start,
            (plan.end + 1)
                .checked_add_signed(isize::from(jump))
                .unwrap()
        );
        assert_eq!(plan.base, usize::from(base.0));
        assert_eq!(plan.used & (15 << plan.base), 15 << plan.base);
        assert_eq!(plan.written & (9 << plan.base), 9 << plan.base);
        assert_eq!(plan.used & plan.written, plan.written);
    }

    #[test]
    fn refuses_effectful_or_non_integer_regions_without_ir_changes() {
        for program in [
            &b"local s=0 for i=1,100 do s=s+0.5 end return s"[..],
            &b"local t={} for i=1,100 do t[i]=i end return t"[..],
            &b"local f=... for i=1,100 do f(i) end"[..],
            &b"local s,x=... for i=1,100 do s=s+x end return s"[..],
            &b"return 1"[..],
        ] {
            let source = snapshot(program);
            let mut function = Function::new();
            let before = function.clone();
            assert!(!augment(
                &mut function,
                &source,
                &[],
                Block::from_u32(0),
                false,
                super::super::super::work::Expansion {
                    instructions: usize::MAX,
                    blocks: usize::MAX
                }
            )
            .unwrap());
            assert_eq!(function, before);
        }
    }
}
