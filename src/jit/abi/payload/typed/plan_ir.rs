use cranelift_codegen::{
    ir::{Block, BlockArg, Function, Inst, InstructionData, Opcode, Type, ValueDef},
    isa::TargetIsa,
};

use super::*;

fn require(condition: bool) -> Result<(), CompileError> {
    condition.then_some(()).ok_or_else(invalid)
}

fn definition(function: &Function, value: IrValue) -> Option<Inst> {
    match function.dfg.value_def(value) {
        ValueDef::Result(inst, 0) => Some(inst),
        _ => None,
    }
}

fn constant(function: &Function, value: IrValue, ty: Type, bits: i64) -> bool {
    let Some(inst) = definition(function, value) else {
        return false;
    };
    matches!(function.dfg.insts[inst], InstructionData::UnaryImm {
        opcode: Opcode::Iconst, imm,
    } if i64::from(imm) == bits && function.dfg.value_type(value) == ty)
}

fn load(function: &Function, value: IrValue) -> Option<(IrValue, i32)> {
    let inst = definition(function, value)?;
    match function.dfg.insts[inst] {
        InstructionData::Load {
            opcode: Opcode::Load,
            arg,
            offset,
            flags,
        } if function.dfg.value_type(value) == types::I64
            && function.dfg.mem_flags[flags] == MemFlagsData::new() =>
        {
            Some((arg, offset.into()))
        }
        _ => None,
    }
}

fn field(function: &Function, value: IrValue, table: IrValue, index: u16) -> bool {
    load(function, value) == Some((table, i32::from(index) * 8))
}

fn input(
    function: &Function,
    value: IrValue,
    expected: Input,
    pointers: IrValue,
    results: &[Option<IrValue>; MAX_STEPS],
) -> bool {
    match expected {
        Input::Previous(index) => results.get(usize::from(index)) == Some(&Some(value)),
        Input::Literal(bits) => constant(function, value, types::I64, bits),
        Input::Binding(index) => load(function, value)
            .is_some_and(|(base, offset)| offset == 0 && field(function, base, pointers, index)),
    }
}

fn expression(
    function: &Function,
    value: IrValue,
    expected: Expression,
    pointers: IrValue,
    results: &[Option<IrValue>; MAX_STEPS],
) -> bool {
    let check = |value, expected| input(function, value, expected, pointers, results);
    if let Expression::Copy(expected) = expected {
        return check(value, expected);
    }
    let Some(inst) = definition(function, value) else {
        return false;
    };
    if function.dfg.value_type(value) != types::I64 {
        return false;
    }
    match (expected, function.dfg.insts[inst]) {
        (
            Expression::Negate(expected),
            InstructionData::Unary {
                opcode: Opcode::Ineg,
                arg,
            },
        ) => check(arg, expected),
        (
            Expression::Add(a, b),
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            },
        )
        | (
            Expression::Sub(a, b),
            InstructionData::Binary {
                opcode: Opcode::Isub,
                args,
            },
        )
        | (
            Expression::Mul(a, b),
            InstructionData::Binary {
                opcode: Opcode::Imul,
                args,
            },
        ) => check(args[0], a) && check(args[1], b),
        _ => false,
    }
}

fn initialized_cell(plan: &Plan, offset: i32) -> bool {
    offset >= 0
        && offset % 8 == 0
        && plan
            .steps
            .iter()
            .any(|step| step.store == Store::Canonical && i32::from(step.destination) == offset / 8)
}

fn pure(function: &Function, inst: Inst, plan: &Plan, tables: [IrValue; 2]) -> bool {
    if function.dfg.inst_results(inst).len() != 1
        || function.dfg.value_type(function.dfg.first_result(inst)) != types::I64
    {
        return false;
    }
    match function.dfg.insts[inst] {
        InstructionData::Load { .. } => {
            let Some((base, offset)) = load(function, function.dfg.first_result(inst)) else {
                return false;
            };
            if base == tables[0] {
                offset >= 0 && offset % 8 == 0 && (offset as usize / 8) < plan.bindings.len()
            } else if base == tables[1] {
                initialized_cell(plan, offset)
            } else {
                offset == 0
                    && load(function, base).is_some_and(|(table, offset)| {
                        table == tables[0]
                            && offset >= 0
                            && offset % 8 == 0
                            && (offset as usize / 8) < plan.bindings.len()
                    })
            }
        }
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            ..
        }
        | InstructionData::Unary {
            opcode: Opcode::Ineg,
            ..
        }
        | InstructionData::Binary {
            opcode: Opcode::Iadd | Opcode::Isub | Opcode::Imul,
            ..
        } => true,
        _ => false,
    }
}

fn write(
    function: &Function,
    inst: Inst,
    step: Step,
    arguments: [IrValue; 4],
) -> Result<IrValue, CompileError> {
    let [pointers, destinations, writer, _] = arguments;
    require(function.dfg.inst_results(inst).is_empty())?;
    match (step.store, function.dfg.insts[inst]) {
        (
            Store::Typed(index),
            InstructionData::Store {
                opcode: Opcode::Store,
                args,
                offset,
                flags,
            },
        ) => {
            require(
                i32::from(offset) == 0
                    && function.dfg.mem_flags[flags] == MemFlagsData::new()
                    && field(function, args[1], pointers, index),
            )?;
            Ok(args[0])
        }
        (
            Store::Canonical,
            InstructionData::CallIndirect {
                opcode: Opcode::CallIndirect,
                sig_ref,
                ..
            },
        ) => {
            let signature = &function.dfg.signatures[sig_ref];
            require(
                signature.call_conv == function.signature.call_conv
                    && signature.params == [AbiParam::new(types::I64); 3]
                    && signature.returns.is_empty(),
            )?;
            let args: [_; 4] = function
                .dfg
                .inst_args(inst)
                .try_into()
                .map_err(|_| invalid())?;
            require(
                args[0] == writer
                    && field(function, args[1], destinations, u16::from(step.destination))
                    && constant(function, args[2], types::I64, 0),
            )?;
            Ok(args[3])
        }
        _ => Err(invalid()),
    }
}

fn guard(
    function: &Function,
    instructions: &[Inst],
    position: usize,
    budget: IrValue,
    done: Block,
    next: Block,
) -> Result<(), CompileError> {
    let [count, compare, branch]: [_; 3] = instructions.try_into().map_err(|_| invalid())?;
    require(function.dfg.inst_results(count).len() == 1)?;
    let count = function.dfg.first_result(count);
    require(constant(function, count, types::I32, position as i64))?;
    require(
        matches!(function.dfg.insts[compare], InstructionData::IntCompare {
        opcode: Opcode::Icmp, cond: IntCC::UnsignedLessThanOrEqual, args,
    } if args == [budget, count]),
    )?;
    let InstructionData::Brif {
        opcode: Opcode::Brif,
        arg,
        blocks,
    } = function.dfg.insts[branch]
    else {
        return Err(invalid());
    };
    require(
        arg == function.dfg.first_result(compare)
            && blocks[0].block(&function.dfg.value_lists) == done
            && blocks[0]
                .args(&function.dfg.value_lists)
                .eq([BlockArg::Value(count)])
            && blocks[1].block(&function.dfg.value_lists) == next
            && blocks[1].args(&function.dfg.value_lists).next().is_none(),
    )
}

pub(super) fn verify(
    source: &Snapshot,
    plan: &Plan,
    function: &Function,
    isa: &dyn TargetIsa,
) -> Result<(), CompileError> {
    plan.verify(source)?;
    require(
        function.dfg.num_blocks() == plan.steps.len() + 2
            && function.dfg.num_insts() <= 24 * plan.steps.len() + 8
            && function.dfg.num_values() <= 24 * plan.steps.len() + 8
            && function.dfg.signatures.len() == 1
            && function.signature.call_conv == isa.default_call_conv()
            && function.signature.params
                == [types::I64, types::I64, types::I64, types::I32].map(AbiParam::new)
            && function.signature.returns == [AbiParam::new(types::I32)],
    )?;
    require(
        function.dfg.num_insts()
            == function
                .layout
                .blocks()
                .map(|block| function.layout.block_insts(block).count())
                .sum::<usize>(),
    )?;
    cranelift_codegen::verify_function(function, isa).map_err(|_| invalid())?;
    let mut blocks = [Block::from_u32(0); MAX_STEPS + 2];
    let mut count = 0;
    for block in function.layout.blocks() {
        *blocks.get_mut(count).ok_or_else(invalid)? = block;
        count += 1;
    }
    require(count == plan.steps.len() + 2)?;
    let done = blocks[count - 1];
    let arguments: [_; 4] = function
        .dfg
        .block_params(blocks[0])
        .try_into()
        .map_err(|_| invalid())?;
    let mut results = [None; MAX_STEPS];
    for (position, &block) in blocks[..count - 1].iter().enumerate() {
        require(position == 0 || function.dfg.block_params(block).is_empty())?;
        let mut instructions = [Inst::from_u32(0); 24];
        let mut length = 0;
        for inst in function.layout.block_insts(block) {
            *instructions.get_mut(length).ok_or_else(invalid)? = inst;
            length += 1;
        }
        let suffix = if position == plan.steps.len() { 2 } else { 3 };
        let prefix = length.checked_sub(suffix).ok_or_else(invalid)?;
        if position == 0 {
            require(prefix == 0)?;
        } else {
            let (store, reads) = instructions[..prefix].split_last().ok_or_else(invalid)?;
            require(
                reads
                    .iter()
                    .all(|&inst| pure(function, inst, plan, [arguments[0], arguments[1]])),
            )?;
            let step = plan.steps[position - 1];
            let value = write(function, *store, step, arguments)?;
            require(expression(
                function,
                value,
                step.expression,
                arguments[0],
                &results,
            ))?;
            results[position - 1] = Some(value);
        }
        if position < plan.steps.len() {
            guard(
                function,
                &instructions[prefix..length],
                position,
                arguments[3],
                done,
                blocks[position + 1],
            )?;
        } else {
            let [count, jump]: [_; 2] = instructions[prefix..length]
                .try_into()
                .map_err(|_| invalid())?;
            require(function.dfg.inst_results(count).len() == 1)?;
            let count = function.dfg.first_result(count);
            require(constant(
                function,
                count,
                types::I32,
                plan.steps.len() as i64,
            ))?;
            let InstructionData::Jump {
                opcode: Opcode::Jump,
                destination,
            } = function.dfg.insts[jump]
            else {
                return Err(invalid());
            };
            require(
                destination.block(&function.dfg.value_lists) == done
                    && destination
                        .args(&function.dfg.value_lists)
                        .eq([BlockArg::Value(count)]),
            )?;
        }
    }
    let [result]: [_; 1] = function
        .dfg
        .block_params(done)
        .try_into()
        .map_err(|_| invalid())?;
    let mut instructions = function.layout.block_insts(done);
    let end = instructions.next().ok_or_else(invalid)?;
    require(
        function.dfg.value_type(result) == types::I32
            && instructions.next().is_none()
            && function.dfg.insts[end].opcode() == Opcode::Return
            && function.dfg.inst_args(end) == [result],
    )
}

#[path = "plan_ir_tests.rs"]
mod tests;
