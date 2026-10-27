use super::*;
use cranelift_codegen::{
    ir::{
        condcodes::IntCC, types, AbiParam, Function, InstBuilder, MemFlagsData, Signature,
        UserFuncName, Value as IrValue,
    },
    isa::{CallConv, TargetFrontendConfig},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

pub(in crate::jit) fn verify(
    plan: Plan,
    candidate: &Function,
    target: TargetFrontendConfig,
    convention: CallConv,
) -> Result<(), crate::jit::JitError> {
    if candidate != &function(plan, target, convention) {
        return Err(crate::jit::JitError::Compilation(
            "canonical resume differs from source template".into(),
        ));
    }
    Ok(())
}

fn exit(b: &mut FunctionBuilder<'_>, target: IrValue, pc: usize, work: u32, reason: IrValue) {
    let pc = b.ins().iconst(types::I64, pc as i64);
    let work = b.ins().iconst(types::I32, i64::from(work));
    b.ins().store(MemFlagsData::new(), pc, target, 0);
    b.ins().store(MemFlagsData::new(), work, target, 8);
    b.ins().store(MemFlagsData::new(), reason, target, 12);
    b.ins().return_(&[]);
}

fn constant_exit(b: &mut FunctionBuilder<'_>, target: IrValue, pc: usize, work: u32, reason: u32) {
    let reason = b.ins().iconst(types::I32, i64::from(reason));
    exit(b, target, pc, work, reason);
}

fn terminal(b: &mut FunctionBuilder<'_>, target: IrValue, budget: IrValue, pc: usize, work: u32) {
    let exhausted = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, budget, i64::from(work));
    let bounded = b
        .ins()
        .iconst(types::I32, crate::jit::exits::Kind::Budget as i64);
    let interpreted = b
        .ins()
        .iconst(types::I32, crate::jit::exits::Kind::Interpreter as i64);
    let reason = b.ins().select(exhausted, bounded, interpreted);
    exit(b, target, pc, work, reason);
}

pub(in crate::jit) fn function(
    plan: Plan,
    target: TargetFrontendConfig,
    convention: CallConv,
) -> Function {
    let pointer = target.pointer_type();
    let mut signature = Signature::new(convention);
    signature
        .params
        .extend([pointer, types::I32, pointer].map(AbiParam::new));
    let mut function = Function::with_name_signature(UserFuncName::user(0, 0), signature);
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut function, &mut context);
    let entry = b.create_block();
    let cookie = b.create_block();
    let rejected = b.create_block();
    let add = b.create_block();
    let step = b.create_block();
    let taken = b.create_block();
    let lookup = b.create_block();
    let argument = b.create_block();
    let finish = b.create_block();
    let finish_write = b.create_block();
    let budgets = std::array::from_fn::<_, 4, _>(|_| b.create_block());
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let view = b.block_params(entry)[0];
    let budget = b.block_params(entry)[1];
    let output = b.block_params(entry)[2];
    let valid = b.ins().icmp_imm_s(IntCC::NotEqual, view, 0);
    b.ins().brif(valid, cookie, &[], rejected, &[]);
    b.switch_to_block(cookie);
    let mut valid = b.ins().iconst(types::I8, 1);
    for (offset, expected) in [(0, plan.cookie()[0]), (8, plan.cookie()[1]), (80, 0)] {
        let actual = b.ins().load(types::I64, MemFlagsData::new(), view, offset);
        let matches = b.ins().icmp_imm_s(IntCC::Equal, actual, expected as i64);
        valid = b.ins().band(valid, matches);
    }
    b.ins().brif(valid, add, &[], rejected, &[]);
    b.switch_to_block(rejected);
    constant_exit(
        &mut b,
        output,
        plan.entry,
        0,
        crate::jit::exits::Kind::Guard as u32,
    );
    b.switch_to_block(add);
    let enough = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, budget, 1);
    let add_body = b.create_block();
    b.ins().brif(enough, add_body, &[], budgets[0], &[]);
    b.switch_to_block(add_body);
    let cells: [IrValue; 6] = std::array::from_fn(|i| {
        b.ins()
            .load(pointer, MemFlagsData::new(), view, 16 + 8 * i as i32)
    });
    let left = b.ins().load(types::I64, MemFlagsData::new(), cells[0], 0);
    let right = b.ins().load(types::I64, MemFlagsData::new(), cells[1], 0);
    let sum = b.ins().iadd(left, right);
    b.ins().store(MemFlagsData::new(), sum, cells[0], 0);
    let enough = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, budget, 2);
    b.ins().brif(enough, step, &[], budgets[1], &[]);
    b.switch_to_block(step);
    let index = b.ins().load(types::I64, MemFlagsData::new(), cells[2], 0);
    let limit = b.ins().load(types::I64, MemFlagsData::new(), cells[3], 0);
    let stride = b.ins().load(types::I64, MemFlagsData::new(), cells[4], 0);
    let (next, overflow) = b.ins().sadd_overflow(index, stride);
    b.ins().store(MemFlagsData::new(), next, cells[2], 0);
    let negative = b.ins().icmp_imm_s(IntCC::SignedLessThan, stride, 0);
    let below = b.ins().icmp(IntCC::SignedLessThan, next, limit);
    let above = b.ins().icmp(IntCC::SignedGreaterThan, next, limit);
    let past = b.ins().select(negative, below, above);
    let past = b.ins().bor(past, overflow);
    b.ins().brif(past, finish, &[], taken, &[]);
    b.switch_to_block(taken);
    b.ins().store(MemFlagsData::new(), next, cells[5], 0);
    let enough = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, budget, 3);
    b.ins().brif(enough, lookup, &[], budgets[2], &[]);
    b.switch_to_block(lookup);
    let one = b.ins().iconst(types::I64, 1);
    b.ins().store(MemFlagsData::new(), one, view, 80);
    let enough = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, budget, 4);
    b.ins().brif(enough, argument, &[], budgets[3], &[]);
    b.switch_to_block(argument);
    b.ins().store(MemFlagsData::new(), next, view, 64);
    let three = b.ins().iconst(types::I64, 3);
    b.ins().store(MemFlagsData::new(), three, view, 80);
    terminal(&mut b, output, budget, plan.lookup.call_pc as usize, 4);
    b.switch_to_block(finish);
    let enough = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, budget, 3);
    let finish_budget = b.create_block();
    b.ins().brif(enough, finish_write, &[], finish_budget, &[]);
    b.switch_to_block(finish_write);
    b.ins().store(MemFlagsData::new(), sum, view, 72);
    let four = b.ins().iconst(types::I64, 4);
    b.ins().store(MemFlagsData::new(), four, view, 80);
    terminal(&mut b, output, budget, plan.return_pc, 3);
    b.switch_to_block(finish_budget);
    constant_exit(
        &mut b,
        output,
        plan.finish_pc,
        2,
        crate::jit::exits::Kind::Budget as u32,
    );
    for (index, pc) in [
        plan.entry,
        plan.loop_pc,
        plan.lookup.helper_pc as usize,
        plan.lookup.helper_pc as usize + 1,
    ]
    .into_iter()
    .enumerate()
    {
        b.switch_to_block(budgets[index]);
        constant_exit(
            &mut b,
            output,
            pc,
            index as u32,
            crate::jit::exits::Kind::Budget as u32,
        );
    }
    b.seal_all_blocks();
    b.finalize(target);
    function
}
