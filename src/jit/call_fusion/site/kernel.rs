use cranelift_codegen::{
    ir::{
        condcodes::IntCC, types, AbiParam, Function, InstBuilder, MemFlagsData, Signature,
        UserFuncName,
    },
    isa::{CallConv, TargetFrontendConfig},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::{abi, Argument, Input, Leaf, Site};
use crate::jit::{exits::Kind, resources::BudgetAllocator, work, CompileError};

#[repr(C)]
#[derive(Clone, Copy)]
pub(in crate::jit) struct View {
    binding: [u64; 4],
    completed: u64,
}

impl View {
    pub(in crate::jit) fn completed_from(&self, original: &Self) -> bool {
        self.binding == original.binding && original.completed == 0 && self.completed == 1
    }
}

pub(in crate::jit) type Entry =
    unsafe extern "C" fn(*mut abi::Slot, u64, u32, *mut abi::Exit, *mut abi::Host, *mut View);

pub(in crate::jit) struct Plan<'source> {
    input: &'source Input,
    site: Site,
}

impl<'source> Plan<'source> {
    pub(in crate::jit) fn new(input: &'source Input) -> Result<Self, CompileError> {
        let (parent, child, site) = input.sources();
        let capture = crate::types::UpValueDescriptor::ParentLocal(crate::types::RegisterIndex(
            site.leaf.parent,
        ));
        if Leaf::recognize(child, 1, false, &[capture]) != Some(site.leaf)
            || Site::recognize(parent, site.leaf, site.prototype) != Some(site)
        {
            return Err(invalid());
        }
        Ok(Self { input, site })
    }

    pub(in crate::jit) fn allocator(&self) -> BudgetAllocator {
        self.input.sources().0.operations.allocator().clone()
    }

    pub(in crate::jit) fn registers(&self) -> usize {
        usize::from(self.site.registers)
    }

    pub(in crate::jit) fn view(&self) -> View {
        View {
            binding: self.binding(),
            completed: 0,
        }
    }

    fn binding(&self) -> [u64; 4] {
        let s = self.site;
        let (kind, argument) = match s.argument {
            Argument::Register(index) => (0, u64::from(index)),
            Argument::Integer(value) => (1, value as u64),
        };
        [
            (u64::from(s.region_pc) << 32) | u64::from(s.return_pc),
            (u64::from(s.registers) << 48)
                | (u64::from(s.temporary) << 40)
                | (u64::from(s.holder) << 32)
                | (u64::from(s.prototype) << 24)
                | (u64::from(s.leaf.parent) << 16)
                | (u64::from(s.leaf.read) << 8)
                | u64::from(s.leaf.result),
            (u64::from(s.leaf.width) << 1) | kind,
            argument,
        ]
    }

    pub(in crate::jit) fn expansion(
        &self,
        limits: work::Limits,
    ) -> Result<work::Expansion, CompileError> {
        let expansion = work::Expansion {
            instructions: 128 + 6 * (self.registers() - usize::from(self.site.temporary)),
            blocks: 5,
        };
        if expansion.instructions > limits.instructions || expansion.blocks > limits.blocks {
            return Err(CompileError::ResourceLimit("fused call IR"));
        }
        Ok(expansion)
    }

    pub(in crate::jit) fn program(
        &self,
        target: TargetFrontendConfig,
        convention: CallConv,
    ) -> Function {
        let pointer = target.pointer_type();
        let mut signature = Signature::new(convention);
        signature.params.extend(
            [pointer, types::I64, types::I32, pointer, pointer, pointer].map(AbiParam::new),
        );
        let mut function = Function::with_name_signature(UserFuncName::user(0, 0), signature);
        let mut context = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut function, &mut context);
        let entry = b.create_block();
        let header = b.create_block();
        let scalars = b.create_block();
        let body = b.create_block();
        let refused = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let [slots, pc, budget, exit, _host, view]: [_; 6] =
            b.block_params(entry).try_into().unwrap();
        let at_site = b
            .ins()
            .icmp_imm_s(IntCC::Equal, pc, i64::from(self.site.region_pc));
        let funded = b
            .ins()
            .icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, budget, 7);
        let nonnull = b.ins().icmp_imm_s(IntCC::NotEqual, view, 0);
        let ready = b.ins().band(at_site, funded);
        let ready = b.ins().band(ready, nonnull);
        b.ins().brif(ready, header, &[], refused, &[]);

        b.switch_to_block(header);
        let mut valid = b.ins().iconst(types::I8, 1);
        for (index, expected) in self.binding().into_iter().chain([0]).enumerate() {
            let actual = b.ins().load(
                types::I64,
                MemFlagsData::trusted(),
                view,
                (index * 8) as i32,
            );
            let same = b.ins().icmp_imm_s(IntCC::Equal, actual, expected as i64);
            valid = b.ins().band(valid, same);
        }
        b.ins().brif(valid, scalars, &[], refused, &[]);

        b.switch_to_block(scalars);
        let mut valid = b.ins().iconst(types::I8, 1);
        for (register, offset, expected) in [
            (self.site.holder, 0, abi::REFERENCE),
            (self.site.holder, 8, 0),
            (self.site.leaf.parent, 0, abi::INTEGER),
        ] {
            let value = b.ins().load(
                types::I64,
                MemFlagsData::trusted(),
                slots,
                i32::from(register) * 16 + offset,
            );
            let same = b.ins().icmp_imm_s(IntCC::Equal, value, expected as i64);
            valid = b.ins().band(valid, same);
        }
        if let Argument::Register(index) = self.site.argument {
            let tag = b.ins().load(
                types::I64,
                MemFlagsData::trusted(),
                slots,
                i32::from(index) * 16,
            );
            let integer = b.ins().icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
            valid = b.ins().band(valid, integer);
        }
        b.ins().brif(valid, body, &[], refused, &[]);

        b.switch_to_block(body);
        let sum = b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            slots,
            i32::from(self.site.leaf.parent) * 16 + 8,
        );
        let argument = match self.site.argument {
            Argument::Register(index) => b.ins().load(
                types::I64,
                MemFlagsData::trusted(),
                slots,
                i32::from(index) * 16 + 8,
            ),
            Argument::Integer(value) => b.ins().iconst(types::I64, value),
        };
        let result = b.ins().iadd(sum, argument);
        let zero = b.ins().iconst(types::I64, 0);
        for (index, register) in (usize::from(self.site.temporary)..self.registers()).enumerate() {
            let mut value = None;
            if index < usize::from(self.site.leaf.width) {
                if index == 0 {
                    value = Some(argument);
                }
                if index == usize::from(self.site.leaf.read) {
                    value = Some(sum);
                }
                if index == usize::from(self.site.leaf.result) {
                    value = Some(result);
                }
            }
            let tag = b.ins().iconst(
                types::I64,
                if value.is_some() {
                    abi::INTEGER
                } else {
                    abi::NIL
                } as i64,
            );
            b.ins()
                .store(MemFlagsData::trusted(), tag, slots, (register * 16) as i32);
            b.ins().store(
                MemFlagsData::trusted(),
                value.unwrap_or(zero),
                slots,
                (register * 16 + 8) as i32,
            );
        }
        b.ins().store(
            MemFlagsData::trusted(),
            result,
            slots,
            i32::from(self.site.leaf.parent) * 16 + 8,
        );
        let one = b.ins().iconst(types::I64, 1);
        b.ins().store(MemFlagsData::trusted(), one, view, 32);
        let resume = b.ins().iconst(types::I64, i64::from(self.site.return_pc));
        let count = b.ins().iconst(types::I32, 5);
        let reason = b.ins().iconst(types::I32, Kind::Interpreter as i64);
        b.ins().store(MemFlagsData::trusted(), resume, exit, 0);
        b.ins().store(MemFlagsData::trusted(), count, exit, 8);
        b.ins().store(MemFlagsData::trusted(), reason, exit, 12);
        b.ins().return_(&[]);

        b.switch_to_block(refused);
        let zero = b.ins().iconst(types::I32, 0);
        let guard = b.ins().iconst(types::I32, Kind::Guard as i64);
        b.ins().store(MemFlagsData::trusted(), pc, exit, 0);
        b.ins().store(MemFlagsData::trusted(), zero, exit, 8);
        b.ins().store(MemFlagsData::trusted(), guard, exit, 12);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(target);
        function
    }

    pub(in crate::jit) fn verify(
        &self,
        function: &Function,
        target: TargetFrontendConfig,
        convention: CallConv,
    ) -> Result<(), CompileError> {
        if *function != self.program(target, convention) {
            return Err(invalid());
        }
        Ok(())
    }
}

fn invalid() -> CompileError {
    CompileError::Compilation("fused call source binding".into())
}

const _: () = assert!(std::mem::size_of::<View>() == 40);
const _: () = assert!(std::mem::offset_of!(View, completed) == 32);

#[cfg(test)]
mod tests;
