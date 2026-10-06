use cranelift_codegen::ir::{
    condcodes::IntCC, types, Function, InstBuilder, MemFlagsData, Signature, UserFuncName,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::{abi, ir::Snapshot, leaf, JitError};

pub(super) struct Plan {
    pattern: leaf::Pattern,
    constant: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        opcode::{Operation, RCIndex},
        types::{RegisterIndex, UpValueIndex, VarCount},
    };
    use cranelift_codegen::{
        ir::{AbiParam, InstructionData},
        settings,
    };

    fn snapshot(math: leaf::Arithmetic, right: RCIndex) -> Snapshot {
        let read = RegisterIndex(1);
        let result = RegisterIndex(2);
        let left = RCIndex::Register(read);
        let operation = match math {
            leaf::Arithmetic::Add => Operation::Add {
                dest: result,
                left,
                right,
            },
            leaf::Arithmetic::Sub => Operation::Sub {
                dest: result,
                left,
                right,
            },
            leaf::Arithmetic::Mul => Operation::Mul {
                dest: result,
                left,
                right,
            },
        };
        Snapshot {
            operations: super::super::resources::owned(&[
                Operation::GetUpValue {
                    dest: read,
                    source: UpValueIndex(0),
                },
                operation,
                Operation::SetUpValue {
                    dest: UpValueIndex(0),
                    source: result,
                },
                Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[abi::Slot::from_value(
                crate::Value::Integer(7),
            )]),
            registers: 3,
            upvalues: 1,
            prototypes: 0,
        }
    }

    fn isa() -> cranelift_codegen::isa::OwnedTargetIsa {
        cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap()
    }

    fn function(plan: &Plan) -> Function {
        let isa = isa();
        let mut signature = Signature::new(isa.default_call_conv());
        for ty in [types::I64, types::I64, types::I32, types::I64, types::I64] {
            signature.params.push(AbiParam::new(ty));
        }
        plan.function(UserFuncName::default(), signature, isa.frontend_config())
            .unwrap()
    }

    #[test]
    fn integer_template_matches_source_and_rejects_field_mutations() {
        let isa = isa();
        for math in [
            leaf::Arithmetic::Add,
            leaf::Arithmetic::Sub,
            leaf::Arithmetic::Mul,
        ] {
            for right in [
                RCIndex::Register(RegisterIndex(0)),
                RCIndex::Register(RegisterIndex(1)),
                RCIndex::Constant(crate::types::ConstantIndex8(0)),
            ] {
                let source = snapshot(math, right);
                let plan = Plan::new(&source).unwrap();
                let original = function(&plan);
                cranelift_codegen::verify_function(&original, isa.as_ref()).unwrap();
                plan.verify(&original, original.signature.clone(), isa.frontend_config())
                    .unwrap();
                let mut tested = 0;
                for block in original.layout.blocks() {
                    for inst in original.layout.block_insts(block) {
                        let mut changed = original.clone();
                        let mutated = match &mut changed.dfg.insts[inst] {
                            InstructionData::UnaryImm { imm, .. } => {
                                *imm = (i64::from(*imm) ^ 1).into();
                                true
                            }
                            InstructionData::Brif { blocks, .. } => {
                                blocks.swap(0, 1);
                                true
                            }
                            InstructionData::Load { offset, .. }
                            | InstructionData::Store { offset, .. } => {
                                let value: i32 = (*offset).into();
                                *offset = (value + 1).into();
                                true
                            }
                            InstructionData::IntCompare { cond, .. } => {
                                *cond = if *cond == IntCC::Equal {
                                    IntCC::NotEqual
                                } else {
                                    IntCC::Equal
                                };
                                true
                            }
                            InstructionData::Binary { args, .. } => {
                                args.swap(0, 1);
                                args[0] != args[1]
                            }
                            _ => false,
                        };
                        if mutated {
                            assert!(plan
                                .verify(&changed, original.signature.clone(), isa.frontend_config())
                                .is_err());
                            tested += 1;
                        }
                    }
                }
                assert!(tested >= 20);
                let mut changed = original.clone();
                changed.signature.params[2].value_type = types::I64;
                assert!(plan
                    .verify(&changed, original.signature.clone(), isa.frontend_config())
                    .is_err());
            }
        }
    }

    #[test]
    fn integer_template_rejects_non_integer_constants_and_changed_sources() {
        let mut source = snapshot(
            leaf::Arithmetic::Add,
            RCIndex::Constant(crate::types::ConstantIndex8(0)),
        );
        for value in [
            crate::Value::Nil,
            crate::Value::Boolean(true),
            crate::Value::Number(7.0),
        ] {
            source.constants[0] = abi::Slot::from_value(value);
            assert!(Plan::new(&source).is_err());
        }
        source.constants[0] = abi::Slot::from_value(crate::Value::Integer(7));
        let plan = Plan::new(&source).unwrap();
        let original = function(&plan);
        source.constants[0] = abi::Slot::from_value(crate::Value::Integer(8));
        assert!(Plan::new(&source)
            .unwrap()
            .verify(
                &original,
                original.signature.clone(),
                isa().frontend_config()
            )
            .is_err());
        source.operations[2] = Operation::SetUpValue {
            dest: UpValueIndex(0),
            source: RegisterIndex(0),
        };
        assert!(Plan::new(&source).is_err());
    }

    #[cfg(not(miri))]
    fn code(source: &Snapshot) -> super::super::owner::Shared<super::super::backend::Code> {
        use super::super::{backend, resources, work, JitConfig};
        let mut ordinary = backend::compile_integer_leaf_pair_in(
            source,
            resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024)),
            128 * 1024,
            resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
            work::Limits::from(&JitConfig::default()),
            backend::Failure::None,
        )
        .unwrap();
        let kernel = ordinary
            .scalar_kernel
            .take()
            .expect("integer kernel was not admitted");
        assert!(kernel.integer_activation);
        assert_eq!(
            kernel.entries.iter().copied().collect::<Vec<_>>(),
            [true, false, false, false]
        );
        kernel
    }

    #[cfg(not(miri))]
    #[test]
    fn generated_integer_entries_preserve_all_slot_aliases_and_budget_declines() {
        for math in [
            leaf::Arithmetic::Add,
            leaf::Arithmetic::Sub,
            leaf::Arithmetic::Mul,
        ] {
            for right in [
                RCIndex::Register(RegisterIndex(0)),
                RCIndex::Register(RegisterIndex(1)),
                RCIndex::Register(RegisterIndex(2)),
                RCIndex::Constant(crate::types::ConstantIndex8(0)),
            ] {
                let source = snapshot(math, right);
                let kernel = code(&source);
                for alias in [None, Some(0), Some(1), Some(2)] {
                    for pc in 0..4 {
                        for budget in [0, 1, 2, 3, 4, 64] {
                            let mut slots = [2, 10, 3]
                                .map(|value| abi::Slot::from_value(crate::Value::Integer(value)));
                            let before = slots;
                            let mut cell = abi::Slot::from_value(crate::Value::Integer(7));
                            let pointer = if let Some(index) = alias {
                                std::ptr::addr_of_mut!(slots[index])
                            } else {
                                std::ptr::addr_of_mut!(cell)
                            };
                            let left = if let Some(index) = alias {
                                before[index].bits as i64
                            } else {
                                7
                            };
                            let rhs = match right {
                                RCIndex::Register(RegisterIndex(1)) => left,
                                RCIndex::Register(index) => {
                                    before[usize::from(index.0)].bits as i64
                                }
                                RCIndex::Constant(_) => 7,
                            };
                            let mut view = leaf::View {
                                version: leaf::VERSION,
                                cell: pointer,
                                reads: 0,
                                writes: 0,
                                dirty: 0,
                            };
                            let exit = unsafe {
                                kernel.invoke_cell_raw(slots.as_mut_ptr(), pc, budget, &mut view)
                            };
                            if pc == 0 && budget > 3 {
                                let value = match math {
                                    leaf::Arithmetic::Add => left.wrapping_add(rhs),
                                    leaf::Arithmetic::Sub => left.wrapping_sub(rhs),
                                    leaf::Arithmetic::Mul => left.wrapping_mul(rhs),
                                };
                                let mut expected = before;
                                expected[1] = abi::Slot::from_value(crate::Value::Integer(left));
                                expected[2] = abi::Slot::from_value(crate::Value::Integer(value));
                                if let Some(index) = alias {
                                    expected[index] = expected[2];
                                } else {
                                    assert_eq!((cell.tag, cell.bits), (abi::INTEGER, value as u64));
                                }
                                assert_eq!(
                                    slots.map(|slot| (slot.tag, slot.bits)),
                                    expected.map(|slot| (slot.tag, slot.bits))
                                );
                                assert_eq!(
                                    (
                                        exit.pc,
                                        exit.instructions,
                                        view.reads,
                                        view.writes,
                                        view.dirty
                                    ),
                                    (3, 3, 1, 1, 1)
                                );
                            } else {
                                assert_eq!(
                                    slots.map(|slot| (slot.tag, slot.bits)),
                                    before.map(|slot| (slot.tag, slot.bits))
                                );
                                assert_eq!((cell.tag, cell.bits), (abi::INTEGER, 7));
                                assert_eq!(
                                    (
                                        exit.pc,
                                        exit.instructions,
                                        view.reads,
                                        view.writes,
                                        view.dirty
                                    ),
                                    (pc as u64, 0, 0, 0, 0)
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(not(miri))]
    #[test]
    fn generated_integer_entries_decline_invalid_views_and_non_integer_operands() {
        let source = snapshot(leaf::Arithmetic::Add, RCIndex::Register(RegisterIndex(0)));
        let kernel = code(&source);
        for fault in 0..9 {
            let mut slots = [abi::Slot::from_value(crate::Value::Integer(2)); 3];
            let mut cell = abi::Slot::from_value(crate::Value::Integer(7));
            let mut view = leaf::View {
                version: leaf::VERSION,
                cell: &mut cell,
                reads: 0,
                writes: 0,
                dirty: 0,
            };
            match fault {
                1 => view.version ^= 1,
                2 => view.cell = std::ptr::null_mut(),
                3 => view.reads = 1,
                4 => view.writes = 1,
                5 => view.dirty = 1,
                6 => cell = abi::Slot::from_value(crate::Value::Number(7.0)),
                7 => slots[0] = abi::Slot::from_value(crate::Value::Number(2.0)),
                8 => {
                    slots[0] = abi::Slot {
                        tag: abi::REFERENCE,
                        bits: 0,
                    }
                }
                _ => {}
            }
            let before = (
                slots.map(|s| (s.tag, s.bits)),
                (cell.tag, cell.bits),
                (view.version, view.cell, view.reads, view.writes, view.dirty),
            );
            let pointer = if fault == 0 {
                std::ptr::null_mut()
            } else {
                &mut view
            };
            let exit = unsafe { kernel.invoke_cell_raw(slots.as_mut_ptr(), 0, 64, pointer) };
            assert_eq!((exit.pc, exit.instructions), (0, 0));
            assert_eq!(
                (
                    slots.map(|s| (s.tag, s.bits)),
                    (cell.tag, cell.bits),
                    (view.version, view.cell, view.reads, view.writes, view.dirty)
                ),
                before
            );
        }
    }
}

impl Plan {
    pub(super) fn new(snapshot: &Snapshot) -> Result<Self, JitError> {
        let pattern = leaf::Pattern::recognize(snapshot)
            .ok_or_else(|| JitError::Compilation("invalid integer activation source".into()))?;
        let constant = match pattern.right {
            leaf::Operand::Register(_) => None,
            leaf::Operand::Constant(index) => {
                let slot = snapshot.constants[usize::from(index)];
                if slot.tag != abi::INTEGER {
                    return Err(JitError::Compilation(
                        "non-integer activation constant".into(),
                    ));
                }
                Some(slot.bits as i64)
            }
        };
        Ok(Self { pattern, constant })
    }

    pub(super) fn function(
        &self,
        name: UserFuncName,
        signature: Signature,
        config: cranelift_codegen::isa::TargetFrontendConfig,
    ) -> Result<Function, JitError> {
        let pointer = config.pointer_type();
        let expected = [pointer, types::I64, types::I32, pointer, pointer];
        if signature.params.len() != 5
            || !signature.returns.is_empty()
            || signature
                .params
                .iter()
                .zip(expected)
                .any(|(param, ty)| param.value_type != ty)
        {
            return Err(JitError::Compilation(
                "invalid integer activation signature".into(),
            ));
        }
        let mut function = Function::with_name_signature(name, signature);
        let mut context = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut function, &mut context);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let p: [_; 5] = b.block_params(entry).try_into().unwrap();
        let decline = b.create_block();
        let version = b.create_block();
        let cell = b.create_block();
        let operands = b.create_block();
        let body = b.create_block();
        let pc = b.ins().icmp_imm_s(IntCC::Equal, p[1], 0);
        let budget = b.ins().icmp_imm_s(IntCC::UnsignedGreaterThan, p[2], 3);
        let view = b.ins().icmp_imm_s(IntCC::NotEqual, p[4], 0);
        let admitted = b.ins().band(pc, budget);
        let admitted = b.ins().band(admitted, view);
        b.ins().brif(admitted, version, &[], decline, &[]);
        b.switch_to_block(version);
        let magic = b.ins().load(types::I64, MemFlagsData::new(), p[4], 0);
        let valid = b
            .ins()
            .icmp_imm_s(IntCC::Equal, magic, leaf::VERSION as i64);
        b.ins().brif(valid, cell, &[], decline, &[]);
        b.switch_to_block(cell);
        let cell_pointer = b.ins().load(pointer, MemFlagsData::new(), p[4], 8);
        let nonnull = b.ins().icmp_imm_s(IntCC::NotEqual, cell_pointer, 0);
        b.ins().brif(nonnull, operands, &[], decline, &[]);
        b.switch_to_block(operands);
        let tag = b
            .ins()
            .load(types::I64, MemFlagsData::new(), cell_pointer, 0);
        let integer = b.ins().icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
        let reads = b.ins().load(types::I32, MemFlagsData::new(), p[4], 16);
        let writes = b.ins().load(types::I32, MemFlagsData::new(), p[4], 20);
        let dirty = b.ins().load(types::I64, MemFlagsData::new(), p[4], 24);
        let reads = b.ins().icmp_imm_s(IntCC::Equal, reads, 0);
        let writes = b.ins().icmp_imm_s(IntCC::Equal, writes, 0);
        let dirty = b.ins().icmp_imm_s(IntCC::Equal, dirty, 0);
        let fresh = b.ins().band(reads, writes);
        let fresh = b.ins().band(fresh, dirty);
        let integer = b.ins().band(integer, fresh);
        let left = b
            .ins()
            .load(types::I64, MemFlagsData::new(), cell_pointer, 8);
        let (right, integer) = match self.pattern.right {
            leaf::Operand::Constant(_) => {
                (b.ins().iconst(types::I64, self.constant.unwrap()), integer)
            }
            leaf::Operand::Register(index) if index == self.pattern.read => (left, integer),
            leaf::Operand::Register(index) => {
                let offset = i32::from(index.0) * 16;
                let tag = b.ins().load(types::I64, MemFlagsData::new(), p[0], offset);
                let valid = b.ins().icmp_imm_s(IntCC::Equal, tag, abi::INTEGER as i64);
                let value = b
                    .ins()
                    .load(types::I64, MemFlagsData::new(), p[0], offset + 8);
                (value, b.ins().band(integer, valid))
            }
        };
        b.ins().brif(integer, body, &[], decline, &[]);
        b.switch_to_block(body);
        let result = match self.pattern.arithmetic {
            leaf::Arithmetic::Add => b.ins().iadd(left, right),
            leaf::Arithmetic::Sub => b.ins().isub(left, right),
            leaf::Arithmetic::Mul => b.ins().imul(left, right),
        };
        let tag = b.ins().iconst(types::I64, abi::INTEGER as i64);
        if self.pattern.read != self.pattern.result {
            let offset = i32::from(self.pattern.read.0) * 16;
            b.ins().store(MemFlagsData::new(), tag, p[0], offset);
            b.ins().store(MemFlagsData::new(), left, p[0], offset + 8);
        }
        let offset = i32::from(self.pattern.result.0) * 16;
        b.ins().store(MemFlagsData::new(), tag, p[0], offset);
        b.ins().store(MemFlagsData::new(), result, p[0], offset + 8);
        b.ins().store(MemFlagsData::new(), tag, cell_pointer, 0);
        b.ins().store(MemFlagsData::new(), result, cell_pointer, 8);
        let one32 = b.ins().iconst(types::I32, 1);
        let one64 = b.ins().iconst(types::I64, 1);
        b.ins().store(MemFlagsData::new(), one32, p[4], 16);
        b.ins().store(MemFlagsData::new(), one32, p[4], 20);
        b.ins().store(MemFlagsData::new(), one64, p[4], 24);
        let pc = b.ins().iconst(types::I64, 3);
        let count = b.ins().iconst(types::I32, 3);
        let reason = b
            .ins()
            .iconst(types::I32, super::exits::Kind::Interpreter as i64);
        b.ins().store(MemFlagsData::new(), pc, p[3], 0);
        b.ins().store(MemFlagsData::new(), count, p[3], 8);
        b.ins().store(MemFlagsData::new(), reason, p[3], 12);
        b.ins().return_(&[]);
        b.switch_to_block(decline);
        let count = b.ins().iconst(types::I32, 0);
        let reason = b
            .ins()
            .iconst(types::I32, super::exits::Kind::Interpreter as i64);
        b.ins().store(MemFlagsData::new(), p[1], p[3], 0);
        b.ins().store(MemFlagsData::new(), count, p[3], 8);
        b.ins().store(MemFlagsData::new(), reason, p[3], 12);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(config);
        Ok(function)
    }

    pub(super) fn verify(
        &self,
        function: &Function,
        signature: Signature,
        config: cranelift_codegen::isa::TargetFrontendConfig,
    ) -> Result<(), JitError> {
        let expected = self.function(function.name.clone(), signature, config)?;
        if function != &expected {
            return Err(JitError::Compilation(
                "integer activation differs from source template".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn numeric_function(
        &self,
        name: UserFuncName,
        signature: Signature,
        config: cranelift_codegen::isa::TargetFrontendConfig,
    ) -> Result<Function, JitError> {
        let pointer = config.pointer_type();
        let expected = [pointer, types::I64, types::I32, pointer, pointer];
        if signature.params.len() != 5
            || !signature.returns.is_empty()
            || signature
                .params
                .iter()
                .zip(expected)
                .any(|(param, ty)| param.value_type != ty)
        {
            return Err(JitError::Compilation(
                "invalid numeric activation signature".into(),
            ));
        }
        let mut function = Function::with_name_signature(name, signature);
        let mut context = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut function, &mut context);
        let first = b.create_block();
        let check = b.create_block();
        let body = b.create_block();
        let decline = b.create_block();
        b.append_block_params_for_function_params(first);
        b.switch_to_block(first);
        let p: [_; 5] = b.block_params(first).try_into().unwrap();
        let pc = b.ins().icmp_imm_s(IntCC::Equal, p[1], 0);
        let budget = b.ins().icmp_imm_s(IntCC::UnsignedGreaterThan, p[2], 3);
        let view = b.ins().icmp_imm_s(IntCC::NotEqual, p[4], 0);
        let lanes = b.ins().icmp_imm_s(IntCC::NotEqual, p[0], 0);
        let valid = b.ins().band(pc, budget);
        let valid = b.ins().band(valid, view);
        let valid = b.ins().band(valid, lanes);
        b.ins().brif(valid, check, &[], decline, &[]);
        b.switch_to_block(check);
        let magic = b.ins().load(types::I64, MemFlagsData::new(), p[4], 0);
        let valid = b
            .ins()
            .icmp_imm_s(IntCC::Equal, magic, leaf::VERSION as i64);
        let cell = b.ins().load(pointer, MemFlagsData::new(), p[4], 8);
        let linked = b.ins().icmp(IntCC::Equal, cell, p[0]);
        let valid = b.ins().band(valid, linked);
        let reads = b.ins().load(types::I32, MemFlagsData::new(), p[4], 16);
        let writes = b.ins().load(types::I32, MemFlagsData::new(), p[4], 20);
        let dirty = b.ins().load(types::I64, MemFlagsData::new(), p[4], 24);
        let reads = b.ins().icmp_imm_s(IntCC::Equal, reads, 0);
        let writes = b.ins().icmp_imm_s(IntCC::Equal, writes, 0);
        let dirty = b.ins().icmp_imm_s(IntCC::Equal, dirty, 0);
        let fresh = b.ins().band(reads, writes);
        let fresh = b.ins().band(fresh, dirty);
        let valid = b.ins().band(valid, fresh);
        b.ins().brif(valid, body, &[], decline, &[]);
        b.switch_to_block(body);
        let left = b.ins().load(
            types::I64,
            MemFlagsData::new(),
            p[0],
            std::mem::offset_of!(abi::Numeric, cell) as i32,
        );
        b.ins().store(
            MemFlagsData::new(),
            left,
            p[0],
            std::mem::offset_of!(abi::Numeric, read) as i32,
        );
        let right = match self.pattern.right {
            leaf::Operand::Constant(_) => b.ins().iconst(types::I64, self.constant.unwrap()),
            leaf::Operand::Register(index) if index == self.pattern.read => left,
            leaf::Operand::Register(_) => b.ins().load(
                types::I64,
                MemFlagsData::new(),
                p[0],
                std::mem::offset_of!(abi::Numeric, right) as i32,
            ),
        };
        let result = match self.pattern.arithmetic {
            leaf::Arithmetic::Add => b.ins().iadd(left, right),
            leaf::Arithmetic::Sub => b.ins().isub(left, right),
            leaf::Arithmetic::Mul => b.ins().imul(left, right),
        };
        b.ins().store(
            MemFlagsData::new(),
            result,
            p[0],
            std::mem::offset_of!(abi::Numeric, result) as i32,
        );
        b.ins().store(
            MemFlagsData::new(),
            result,
            p[0],
            std::mem::offset_of!(abi::Numeric, cell) as i32,
        );
        let one32 = b.ins().iconst(types::I32, 1);
        let one64 = b.ins().iconst(types::I64, 1);
        b.ins().store(MemFlagsData::new(), one32, p[4], 16);
        b.ins().store(MemFlagsData::new(), one32, p[4], 20);
        b.ins().store(MemFlagsData::new(), one64, p[4], 24);
        let pc = b.ins().iconst(types::I64, 3);
        let count = b.ins().iconst(types::I32, 3);
        let reason = b
            .ins()
            .iconst(types::I32, super::exits::Kind::Interpreter as i64);
        b.ins().store(MemFlagsData::new(), pc, p[3], 0);
        b.ins().store(MemFlagsData::new(), count, p[3], 8);
        b.ins().store(MemFlagsData::new(), reason, p[3], 12);
        b.ins().return_(&[]);
        b.switch_to_block(decline);
        let zero = b.ins().iconst(types::I32, 0);
        let reason = b
            .ins()
            .iconst(types::I32, super::exits::Kind::Interpreter as i64);
        b.ins().store(MemFlagsData::new(), p[1], p[3], 0);
        b.ins().store(MemFlagsData::new(), zero, p[3], 8);
        b.ins().store(MemFlagsData::new(), reason, p[3], 12);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(config);
        Ok(function)
    }
}
