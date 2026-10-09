use cranelift_codegen::{
    ir::{
        condcodes::IntCC, types, AbiParam, Block, Function, InstBuilder, MemFlagsData, Signature,
        Value as IrValue,
    },
    isa::TargetIsa,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::{lowering, plan::Plan, *};
use crate::{
    jit::{exits::Kind, ir::Snapshot, JitError},
    opcode::{Operation, RCIndex},
};

struct Emitter<'a, 'f> {
    b: &'a mut FunctionBuilder<'f>,
    source: &'a Snapshot,
    slots: IrValue,
    view: IrValue,
    decline: Block,
}

impl Emitter<'_, '_> {
    fn load(&mut self, register: u8) -> (IrValue, IrValue) {
        let offset = i32::from(register) * 16;
        let tag = self
            .b
            .ins()
            .load(types::I64, MemFlagsData::new(), self.slots, offset);
        let bits = self
            .b
            .ins()
            .load(types::I64, MemFlagsData::new(), self.slots, offset + 8);
        (tag, bits)
    }

    fn literal(&mut self, slot: Slot) -> (IrValue, IrValue) {
        (
            self.b.ins().iconst(types::I64, slot.tag as i64),
            self.b.ins().iconst(types::I64, slot.bits as i64),
        )
    }

    fn operand(&mut self, operand: RCIndex) -> (IrValue, IrValue) {
        match operand {
            RCIndex::Register(index) => self.load(index.0),
            RCIndex::Constant(index) => self.literal(self.source.constants[usize::from(index.0)]),
        }
    }

    fn store(&mut self, register: u8, tag: IrValue, bits: IrValue) {
        self.b.ins().store(
            MemFlagsData::new(),
            tag,
            self.slots,
            i32::from(register) * 16,
        );
        self.b.ins().store(
            MemFlagsData::new(),
            bits,
            self.slots,
            i32::from(register) * 16 + 8,
        );
    }

    fn integer(&mut self, tag: IrValue) {
        let valid = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, abi::INTEGER as i64);
        lowering::guard(self.b, valid, self.decline);
    }

    fn numeric(&mut self, tag: IrValue) {
        let integer = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, abi::INTEGER as i64);
        let number = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, abi::NUMBER as i64);
        let valid = self.b.ins().bor(integer, number);
        lowering::guard(self.b, valid, self.decline);
    }

    fn float(&mut self, tag: IrValue, bits: IrValue) -> IrValue {
        let integer = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, abi::INTEGER as i64);
        let converted = self.b.ins().fcvt_from_sint(types::F64, bits);
        let original = self.b.ins().bitcast(types::F64, MemFlagsData::new(), bits);
        self.b.ins().select(integer, converted, original)
    }

    fn arithmetic(&mut self, operation: Operation, dest: u8, left: RCIndex, right: RCIndex) {
        let (lt, lb) = self.operand(left);
        let (rt, rb) = self.operand(right);
        self.numeric(lt);
        self.numeric(rt);
        let li = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, lt, abi::INTEGER as i64);
        let ri = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, rt, abi::INTEGER as i64);
        let both = self.b.ins().band(li, ri);
        let integer = self.b.create_block();
        let float = self.b.create_block();
        let join = self.b.create_block();
        self.b.append_block_param(join, types::I64);
        self.b.append_block_param(join, types::I64);
        self.b.ins().brif(both, integer, &[], float, &[]);
        self.b.switch_to_block(integer);
        let bits = match operation {
            Operation::Add { .. } => self.b.ins().iadd(lb, rb),
            Operation::Sub { .. } => self.b.ins().isub(lb, rb),
            _ => self.b.ins().imul(lb, rb),
        };
        let tag = self.b.ins().iconst(types::I64, abi::INTEGER as i64);
        self.b.ins().jump(join, &[tag.into(), bits.into()]);
        self.b.switch_to_block(float);
        let left = self.float(lt, lb);
        let right = self.float(rt, rb);
        let value = match operation {
            Operation::Add { .. } => self.b.ins().fadd(left, right),
            Operation::Sub { .. } => self.b.ins().fsub(left, right),
            _ => self.b.ins().fmul(left, right),
        };
        let bits = self.b.ins().bitcast(types::I64, MemFlagsData::new(), value);
        let tag = self.b.ins().iconst(types::I64, abi::NUMBER as i64);
        self.b.ins().jump(join, &[tag.into(), bits.into()]);
        self.b.switch_to_block(join);
        let tag = self.b.block_params(join)[0];
        let bits = self.b.block_params(join)[1];
        self.store(dest, tag, bits);
    }

    fn operation(&mut self, operation: Operation) {
        match operation {
            Operation::GetTable { dest, key, .. } => {
                let (tag, key) = self.operand(key);
                self.integer(tag);
                let dest = self.b.ins().iadd_imm_u(self.slots, i64::from(dest.0) * 16);
                lowering::emit(
                    self.b,
                    self.view,
                    key,
                    lowering::Input::ReadTo(dest),
                    self.decline,
                );
            }
            Operation::SetTable { key, value, .. } => {
                let (tag, key) = self.operand(key);
                self.integer(tag);
                let (tag, bits) = self.operand(value);
                lowering::emit(
                    self.b,
                    self.view,
                    key,
                    lowering::Input::WriteValue(tag, bits),
                    self.decline,
                );
            }
            Operation::Move { dest, source } => {
                let (tag, bits) = self.load(source.0);
                lowering::guard_scalar(self.b, tag, bits, self.decline);
                self.store(dest.0, tag, bits);
            }
            Operation::LoadConstant { dest, constant } => {
                let (tag, bits) = self.literal(self.source.constants[usize::from(constant.0)]);
                self.store(dest.0, tag, bits);
            }
            Operation::Add { dest, left, right }
            | Operation::Sub { dest, left, right }
            | Operation::Mul { dest, left, right } => {
                self.arithmetic(operation, dest.0, left, right)
            }
            _ => unreachable!("operation outside array-loop plan"),
        }
    }

    fn for_loop(&mut self, base: u8, taken: Block, done: Block, count: IrValue) {
        let (it, index) = self.load(base);
        let (lt, limit) = self.load(base + 1);
        let (st, step) = self.load(base + 2);
        for tag in [it, lt, st] {
            self.integer(tag);
        }
        let (next, overflow) = self.b.ins().sadd_overflow(index, step);
        let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, step, 0);
        let ge = self
            .b
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, next, limit);
        let le = self.b.ins().icmp(IntCC::SignedLessThanOrEqual, next, limit);
        let in_range = self.b.ins().select(negative, ge, le);
        let no_overflow = self.b.ins().bxor_imm_u(overflow, 1);
        let continuing = self.b.ins().band(in_range, no_overflow);
        self.store(base, it, next);
        let update = self.b.create_block();
        self.b.ins().brif(continuing, update, &[], done, &[]);
        self.b.switch_to_block(update);
        self.store(base + 3, it, next);
        self.b.ins().jump(taken, &[count.into()]);
    }
}

pub(super) fn program(
    source: &Snapshot,
    plan: Plan,
    isa: &dyn TargetIsa,
) -> Result<Function, JitError> {
    if Plan::new(source, plan.end) != Some(plan) {
        return Err(JitError::Compilation("array loop source mismatch".into()));
    }
    let mut signature = Signature::new(isa.default_call_conv());
    signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let mut function = Function::new();
    function.signature = signature;
    let mut frontend = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut function, &mut frontend);
    let entry = b.create_block();
    let exit = b.create_block();
    for ty in [types::I64, types::I32, types::I32] {
        b.append_block_param(exit, ty);
    }
    let blocks: Vec<_> = (plan.start..=plan.end)
        .map(|_| {
            let block = b.create_block();
            b.append_block_param(block, types::I32);
            block
        })
        .collect();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let [slots, pc, budget, output, view]: [_; 5] = b.block_params(entry).try_into().unwrap();
    let zero = b.ins().iconst(types::I32, 0);
    let cap = b.ins().iconst(types::I32, LIMIT as i64);
    let budget = b.ins().umin(budget, cap);
    let interpreter = b.ins().iconst(types::I32, Kind::Interpreter as i64);
    let guard = b.ins().iconst(types::I32, Kind::Guard as i64);
    let exhausted = b.ins().iconst(types::I32, Kind::Budget as i64);
    for (index, block) in blocks.iter().copied().enumerate() {
        let matched = b
            .ins()
            .icmp_imm_u(IntCC::Equal, pc, (plan.start + index) as i64);
        let next = b.create_block();
        b.ins().brif(matched, block, &[zero.into()], next, &[]);
        b.switch_to_block(next);
    }
    b.ins()
        .jump(exit, &[pc.into(), zero.into(), interpreter.into()]);
    for (index, block) in blocks.iter().copied().enumerate() {
        let source_pc = plan.start + index;
        b.switch_to_block(block);
        let count = b.block_params(block)[0];
        let current = b.ins().iconst(types::I64, source_pc as i64);
        let within_budget = b.ins().icmp(IntCC::UnsignedLessThan, count, budget);
        let body = b.create_block();
        let decline = b.create_block();
        b.ins().brif(
            within_budget,
            body,
            &[],
            exit,
            &[current.into(), count.into(), exhausted.into()],
        );
        b.switch_to_block(decline);
        b.ins()
            .jump(exit, &[current.into(), count.into(), guard.into()]);
        b.switch_to_block(body);
        let next_count = b.ins().iadd_imm_u(count, 1);
        let mut emitter = Emitter {
            b: &mut b,
            source,
            slots,
            view,
            decline,
        };
        if source_pc == plan.end {
            let done = emitter.b.create_block();
            emitter.for_loop(plan.base, blocks[0], done, next_count);
            b.switch_to_block(done);
            let next_pc = b.ins().iconst(types::I64, (plan.end + 1) as i64);
            b.ins().jump(
                exit,
                &[next_pc.into(), next_count.into(), interpreter.into()],
            );
        } else {
            emitter.operation(source.operations[source_pc]);
            b.ins().jump(blocks[index + 1], &[next_count.into()]);
        }
    }
    b.switch_to_block(exit);
    let [pc, instructions, reason]: [_; 3] = b.block_params(exit).try_into().unwrap();
    b.ins().store(
        MemFlagsData::new(),
        pc,
        output,
        std::mem::offset_of!(abi::Exit, pc) as i32,
    );
    b.ins().store(
        MemFlagsData::new(),
        instructions,
        output,
        std::mem::offset_of!(abi::Exit, instructions) as i32,
    );
    b.ins().store(
        MemFlagsData::new(),
        reason,
        output,
        std::mem::offset_of!(abi::Exit, reason) as i32,
    );
    b.ins().return_(&[]);
    b.seal_all_blocks();
    b.finalize(isa.frontend_config());
    cranelift_codegen::verify_function(&function, isa)
        .map_err(|error| JitError::Compilation(error.to_string()))?;
    Ok(function)
}

#[cfg(test)]
mod tests;
