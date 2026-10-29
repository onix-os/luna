use cranelift_codegen::{
    ir::{condcodes::IntCC, types, AbiParam, Block, Function, InstBuilder, MemFlagsData, Value},
    isa::{CallConv, TargetFrontendConfig},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::{Plan, MAX_REGISTERS};
use crate::jit::{abi, leaf, CompileError};
use crate::opcode::Operation;

const FINISHED: u32 = 0;
const BUDGET: u32 = 1;
const GUARD: u32 = 2;
const FUEL: u32 = 3;
const INVALID: u32 = 4;

#[repr(C)]
#[derive(Clone, Debug)]
pub(in crate::jit) struct State {
    slots: [abi::Slot; MAX_REGISTERS],
    upper: abi::Slot,
    root: u64,
    pc: u64,
    fuel: i32,
    interrupted: u32,
    budget: u32,
    instructions: u32,
    calls: u32,
    reason: u32,
    capture: u32,
}

struct Emitter<'a> {
    b: FunctionBuilder<'a>,
    state: Value,
    exits: [Block; 5],
}

impl State {
    pub(in crate::jit) fn exit(&self) -> abi::Exit {
        abi::Exit {
            pc: self.pc,
            instructions: 0,
            reason: match self.reason {
                BUDGET => crate::jit::exits::Kind::Budget,
                GUARD | INVALID => crate::jit::exits::Kind::Guard,
                _ => crate::jit::exits::Kind::Interpreter,
            } as u32,
        }
    }
}

impl Emitter<'_> {
    fn load(&mut self, ty: cranelift_codegen::ir::Type, offset: usize) -> Value {
        self.b
            .ins()
            .load(ty, MemFlagsData::new(), self.state, offset as i32)
    }

    fn store(&mut self, offset: usize, value: Value) {
        self.b
            .ins()
            .store(MemFlagsData::new(), value, self.state, offset as i32);
    }

    fn constant(&mut self, offset: usize, ty: cranelift_codegen::ir::Type, value: i64) {
        let value = self.b.ins().iconst(ty, value);
        self.store(offset, value);
    }

    fn guard(&mut self, valid: Value, reason: u32) {
        let next = self.b.create_block();
        self.b
            .ins()
            .brif(valid, next, &[], self.exits[reason as usize], &[]);
        self.b.switch_to_block(next);
    }

    fn slot(&mut self, offset: usize) -> (Value, Value) {
        (
            self.load(types::I64, offset),
            self.load(types::I64, offset + 8),
        )
    }

    fn integer(&mut self, offset: usize) -> Value {
        let (tag, bits) = self.slot(offset);
        let valid = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, abi::INTEGER as i64);
        self.guard(valid, GUARD);
        bits
    }

    fn write_slot(&mut self, offset: usize, (tag, bits): (Value, Value)) {
        self.store(offset, tag);
        self.store(offset + 8, bits);
    }

    fn write_integer(&mut self, offset: usize, bits: Value) {
        self.constant(offset, types::I64, abi::INTEGER as i64);
        self.store(offset + 8, bits);
    }

    fn cell_integer(&mut self, cell: Value) -> Value {
        let tag = self.b.ins().load(types::I64, MemFlagsData::new(), cell, 0);
        let valid = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, abi::INTEGER as i64);
        self.guard(valid, GUARD);
        self.b.ins().load(types::I64, MemFlagsData::new(), cell, 8)
    }

    fn write_cell(&mut self, cell: Value, bits: Value) {
        let tag = self.b.ins().iconst(types::I64, abi::INTEGER as i64);
        self.b.ins().store(MemFlagsData::new(), tag, cell, 0);
        self.b.ins().store(MemFlagsData::new(), bits, cell, 8);
    }

    fn adjust(&mut self, offset: usize, amount: i64) {
        let value = self.load(types::I32, offset);
        let value = self.b.ins().iadd_imm_s(value, amount);
        self.store(offset, value);
    }

    fn charge(&mut self, instructions: u32, fuel: i32, pc: usize) {
        self.adjust(
            std::mem::offset_of!(State, budget),
            -i64::from(instructions),
        );
        self.adjust(
            std::mem::offset_of!(State, instructions),
            i64::from(instructions),
        );
        self.adjust(std::mem::offset_of!(State, fuel), -i64::from(fuel));
        self.constant(std::mem::offset_of!(State, pc), types::I64, pc as i64);
    }

    fn fuel_guard(&mut self, minimum: i64) {
        let fuel = self.load(types::I32, std::mem::offset_of!(State, fuel));
        let ready = self
            .b
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThan, fuel, minimum);
        self.guard(ready, FUEL);
    }
}

fn slot(index: usize) -> usize {
    std::mem::offset_of!(State, slots) + index * std::mem::size_of::<abi::Slot>()
}

impl Plan<'_> {
    pub(in crate::jit) fn function(
        &self,
        config: TargetFrontendConfig,
        convention: CallConv,
    ) -> Result<Function, CompileError> {
        self.verify(self.caller, self.callee)?;
        let function = usize::from(self.pair.function.0);
        let arguments = usize::from(self.pair.arguments);
        if arguments > self.callee.registers {
            return Err(CompileError::Compilation(
                "invalid inline loop binding".into(),
            ));
        }
        let mut result = Function::new();
        result.signature.call_conv = convention;
        result
            .signature
            .params
            .push(AbiParam::new(config.pointer_type()));
        let mut context = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut result, &mut context);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let state = b.block_params(entry)[0];
        let exits = std::array::from_fn(|_| b.create_block());
        let mut blocks =
            allocator_api2::vec::Vec::new_in(self.caller.operations.allocator().clone());
        blocks
            .try_reserve_exact(self.end - self.start + 1)
            .map_err(|_| CompileError::ResourceLimit("inline loop blocks"))?;
        blocks.extend((self.start..=self.end).map(|_| b.create_block()));
        let mut e = Emitter { b, state, exits };
        for offset in [
            std::mem::offset_of!(State, instructions),
            std::mem::offset_of!(State, calls),
        ] {
            e.constant(offset, types::I32, 0);
        }
        assert_eq!(slot(MAX_REGISTERS), std::mem::offset_of!(State, upper));
        let capture = e.load(types::I32, std::mem::offset_of!(State, capture));
        let local =
            e.b.ins()
                .icmp_imm_u(IntCC::UnsignedLessThan, capture, function as i64);
        let upper =
            e.b.ins()
                .icmp_imm_u(IntCC::Equal, capture, MAX_REGISTERS as i64);
        let valid = e.b.ins().bor(local, upper);
        e.guard(valid, INVALID);
        let offset = e.b.ins().uextend(config.pointer_type(), capture);
        let offset =
            e.b.ins()
                .imul_imm_u(offset, std::mem::size_of::<abi::Slot>() as i64);
        let cell = e.b.ins().iadd(state, offset);
        let budget = e.load(types::I32, std::mem::offset_of!(State, budget));
        let bounded =
            e.b.ins()
                .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, budget, 64);
        e.guard(bounded, INVALID);
        let root = e.load(types::I64, std::mem::offset_of!(State, root));
        let bounded =
            e.b.ins()
                .icmp_imm_u(IntCC::UnsignedLessThan, root, self.caller.registers as i64);
        e.guard(bounded, INVALID);
        let interrupted = e.load(types::I32, std::mem::offset_of!(State, interrupted));
        let ready = e.b.ins().icmp_imm_u(IntCC::Equal, interrupted, 0);
        e.guard(ready, FUEL);
        e.fuel_guard(0);
        let pc = e.load(types::I64, std::mem::offset_of!(State, pc));
        for (offset, block) in blocks.iter().copied().enumerate() {
            let next = e.b.create_block();
            let matches =
                e.b.ins()
                    .icmp_imm_u(IntCC::Equal, pc, (self.start + offset) as i64);
            e.b.ins().brif(matches, block, &[], next, &[]);
            e.b.switch_to_block(next);
        }
        e.b.ins().jump(exits[INVALID as usize], &[]);
        for (offset, block) in blocks.iter().copied().enumerate() {
            let pc = self.start + offset;
            e.b.switch_to_block(block);
            let required = if pc == self.call { 5 } else { 1 };
            let budget = e.load(types::I32, std::mem::offset_of!(State, budget));
            let ready =
                e.b.ins()
                    .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, budget, required);
            e.guard(ready, BUDGET);
            match self.caller.operations[pc] {
                Operation::Move { dest, source } => {
                    let value = e.slot(slot(usize::from(source.0)));
                    e.write_slot(slot(usize::from(dest.0)), value);
                    e.charge(1, 1, pc + 1);
                    e.b.ins().jump(blocks[offset + 1], &[]);
                }
                Operation::Call { .. } => {
                    let (tag, bits) = e.slot(slot(function));
                    let reference =
                        e.b.ins()
                            .icmp_imm_u(IntCC::Equal, tag, abi::REFERENCE as i64);
                    e.guard(reference, GUARD);
                    let expected = e.load(types::I64, std::mem::offset_of!(State, root));
                    let identity = e.b.ins().icmp(IntCC::Equal, bits, expected);
                    e.guard(identity, GUARD);
                    let left = e.cell_integer(cell);
                    let pattern = self.pair.pattern;
                    let right = match pattern.right {
                        leaf::Operand::Register(index) if index == pattern.read => left,
                        leaf::Operand::Register(index) => {
                            e.integer(slot(function + 1 + usize::from(index.0)))
                        }
                        leaf::Operand::Constant(index) => e.b.ins().iconst(
                            types::I64,
                            self.callee.constants[usize::from(index)].bits as i64,
                        ),
                    };
                    e.fuel_guard(8 + arguments as i64);
                    let updated = match pattern.arithmetic {
                        leaf::Arithmetic::Add => e.b.ins().iadd(left, right),
                        leaf::Arithmetic::Sub => e.b.ins().isub(left, right),
                        leaf::Arithmetic::Mul => e.b.ins().imul(left, right),
                    };
                    for index in 0..arguments {
                        let value = e.slot(slot(function + index + 1));
                        e.write_slot(slot(function + index), value);
                    }
                    for index in function + arguments..self.caller.registers {
                        e.constant(slot(index), types::I64, abi::NIL as i64);
                        e.constant(slot(index) + 8, types::I64, 0);
                    }
                    for (register, value) in [(pattern.read, left), (pattern.result, updated)] {
                        let index = function + usize::from(register.0);
                        if index < self.caller.registers {
                            e.write_integer(slot(index), value);
                        }
                    }
                    e.write_cell(cell, updated);
                    e.charge(5, 19 + arguments as i32, pc + 1);
                    e.adjust(std::mem::offset_of!(State, calls), 1);
                    e.fuel_guard(0);
                    e.b.ins().jump(blocks[offset + 1], &[]);
                }
                Operation::NumericForLoop { .. } => {
                    let index = e.integer(slot(self.base));
                    let limit = e.integer(slot(self.base + 1));
                    let step = e.integer(slot(self.base + 2));
                    let (next, overflow) = e.b.ins().sadd_overflow(index, step);
                    let negative = e.b.ins().icmp_imm_s(IntCC::SignedLessThan, step, 0);
                    let descending = e.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, next, limit);
                    let ascending = e.b.ins().icmp(IntCC::SignedLessThanOrEqual, next, limit);
                    let inside = e.b.ins().select(negative, descending, ascending);
                    let no_overflow = e.b.ins().bnot(overflow);
                    let continuing = e.b.ins().band(inside, no_overflow);
                    e.write_integer(slot(self.base), next);
                    e.charge(1, 1, pc + 1);
                    let again = e.b.create_block();
                    e.b.ins()
                        .brif(continuing, again, &[], exits[FINISHED as usize], &[]);
                    e.b.switch_to_block(again);
                    e.write_integer(slot(self.base + 3), next);
                    e.constant(
                        std::mem::offset_of!(State, pc),
                        types::I64,
                        self.start as i64,
                    );
                    e.b.ins().jump(blocks[0], &[]);
                }
                _ => unreachable!(),
            }
        }
        for (reason, block) in exits.into_iter().enumerate() {
            e.b.switch_to_block(block);
            e.constant(
                std::mem::offset_of!(State, reason),
                types::I32,
                reason as i64,
            );
            e.b.ins().return_(&[]);
        }
        e.b.seal_all_blocks();
        e.b.finalize(config);
        self.pair.expansion.verify_actual(
            result
                .layout
                .blocks()
                .map(|block| result.layout.block_insts(block).count())
                .sum(),
            result.dfg.num_blocks(),
        )?;
        Ok(result)
    }

    pub(in crate::jit) fn verify_function(
        &self,
        function: &Function,
        config: TargetFrontendConfig,
        convention: CallConv,
    ) -> Result<(), CompileError> {
        if *function != self.function(config, convention)? {
            return Err(CompileError::Compilation(
                "inline loop program differs from source".into(),
            ));
        }
        Ok(())
    }
}

pub(in crate::jit) mod bridge;
#[cfg(test)]
mod tests;
