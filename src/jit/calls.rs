use crate::{opcode::Operation, types::RegisterIndex};

use super::{abi, ir::Snapshot, leaf, work, JitError};

pub(super) struct Plan<'a> {
    caller: &'a Snapshot,
    callee: &'a Snapshot,
    pc: usize,
    pub function: RegisterIndex,
    pub arguments: u8,
    pub pattern: leaf::Pattern,
    pub expansion: work::Expansion,
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[repr(C)]
pub(super) struct NativeFrame {
    pub slots: *mut abi::Slot,
    pub view: *mut leaf::View,
    pub exit: abi::Exit,
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) struct Program {
    pub entry: cranelift_codegen::ir::Function,
    pub callee: cranelift_codegen::ir::Function,
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl Plan<'_> {
    pub fn allocator(&self) -> super::resources::BudgetAllocator {
        self.caller.operations.allocator().clone()
    }

    pub fn program(
        &self,
        config: cranelift_codegen::isa::TargetFrontendConfig,
        convention: cranelift_codegen::isa::CallConv,
    ) -> Result<Program, JitError> {
        use cranelift_codegen::ir::{
            condcodes::IntCC, types, AbiParam, ExtFuncData, ExternalName, Function, InstBuilder,
            MemFlagsData, Signature, UserFuncName,
        };
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

        self.verify(self.caller, self.callee)?;
        let pointer = config.pointer_type();
        let signature = |parameters: &[cranelift_codegen::ir::Type],
                         result: Option<cranelift_codegen::ir::Type>| {
            let mut signature = Signature::new(convention);
            signature
                .params
                .extend(parameters.iter().copied().map(AbiParam::new));
            signature.returns.extend(result.map(AbiParam::new));
            signature
        };
        let leaf_signature = signature(&[pointer, types::I64, types::I32, pointer, pointer], None);
        let callee = super::integer::Plan::new(self.callee)?.function(
            UserFuncName::user(0, 1),
            leaf_signature.clone(),
            config,
        )?;
        let mut entry = Function::with_name_signature(
            UserFuncName::user(0, 0),
            signature(&[pointer, types::I32], Some(types::I32)),
        );
        let enter_signature = entry.import_signature(signature(
            &[pointer, types::I64, types::I32, types::I32],
            Some(pointer),
        ));
        let enter = entry.import_function(ExtFuncData {
            name: ExternalName::testcase("activation_enter"),
            signature: enter_signature,
            colocated: false,
            patchable: false,
        });
        let leaf_signature = entry.import_signature(leaf_signature);
        let leaf = entry.import_function(ExtFuncData {
            name: ExternalName::testcase("activation_leaf"),
            signature: leaf_signature,
            colocated: true,
            patchable: false,
        });
        let leave_signature = entry.import_signature(signature(
            &[pointer, pointer, types::I64, types::I32],
            Some(types::I32),
        ));
        let leave = entry.import_function(ExtFuncData {
            name: ExternalName::testcase("activation_leave"),
            signature: leave_signature,
            colocated: false,
            patchable: false,
        });
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut entry, &mut context);
        let first = builder.create_block();
        let body = builder.create_block();
        let decline = builder.create_block();
        builder.switch_to_block(first);
        builder.append_block_params_for_function_params(first);
        let parameters: [_; 2] = builder.block_params(first).try_into().unwrap();
        let pc = builder.ins().iconst(types::I64, self.pc as i64);
        let function = builder.ins().iconst(types::I32, i64::from(self.function.0));
        let arguments = builder.ins().iconst(types::I32, i64::from(self.arguments));
        let request = builder
            .ins()
            .call(enter, &[parameters[0], pc, function, arguments]);
        let frame = builder.inst_results(request)[0];
        let valid = builder.ins().icmp_imm_s(IntCC::NotEqual, frame, 0);
        builder.ins().brif(valid, body, &[], decline, &[]);
        builder.switch_to_block(body);
        let slots = builder.ins().load(
            pointer,
            MemFlagsData::new(),
            frame,
            std::mem::offset_of!(NativeFrame, slots) as i32,
        );
        let view = builder.ins().load(
            pointer,
            MemFlagsData::new(),
            frame,
            std::mem::offset_of!(NativeFrame, view) as i32,
        );
        let exit = builder
            .ins()
            .iadd_imm_s(frame, std::mem::offset_of!(NativeFrame, exit) as i64);
        let zero = builder.ins().iconst(types::I64, 0);
        builder
            .ins()
            .call(leaf, &[slots, zero, parameters[1], exit, view]);
        let Operation::Return { start, .. } = self.callee.operations[3] else {
            unreachable!()
        };
        let return_pc = builder.ins().iconst(types::I64, 3);
        let start = builder.ins().iconst(types::I32, i64::from(start.0));
        let result = builder
            .ins()
            .call(leave, &[parameters[0], frame, return_pc, start]);
        let result = builder.inst_results(result)[0];
        builder.ins().return_(&[result]);
        builder.switch_to_block(decline);
        let zero = builder.ins().iconst(types::I32, 0);
        builder.ins().return_(&[zero]);
        builder.seal_all_blocks();
        builder.finalize(config);
        let functions = [&entry, &callee];
        let instructions = functions
            .iter()
            .flat_map(|function| {
                function
                    .layout
                    .blocks()
                    .map(|block| function.layout.block_insts(block).count())
            })
            .sum();
        let blocks = functions
            .iter()
            .map(|function| function.layout.blocks().count())
            .sum();
        self.expansion.verify_actual(instructions, blocks)?;
        Ok(Program { entry, callee })
    }

    pub fn verify_program(
        &self,
        program: &Program,
        config: cranelift_codegen::isa::TargetFrontendConfig,
        convention: cranelift_codegen::isa::CallConv,
    ) -> Result<(), JitError> {
        let expected = self.program(config, convention)?;
        if program.entry != expected.entry || program.callee != expected.callee {
            return Err(JitError::Compilation(
                "aggregate program differs from source".into(),
            ));
        }
        super::integer::Plan::new(self.callee)?.verify(
            &program.callee,
            expected.callee.signature.clone(),
            config,
        )
    }
}

impl<'a> Plan<'a> {
    pub fn new(
        caller: &'a Snapshot,
        callee: &'a Snapshot,
        pc: usize,
        limits: work::Limits,
    ) -> Result<Self, JitError> {
        caller.verify()?;
        callee.verify()?;
        if limits.relocations < 3 {
            return Err(JitError::ResourceLimit("aggregate relocation imports"));
        }
        let Some(Operation::Call {
            func,
            args,
            returns,
        }) = caller.operations.get(pc).copied()
        else {
            return Err(JitError::Compilation(
                "aggregate source is not a call".into(),
            ));
        };
        let Some(arguments) = args.to_constant() else {
            return Err(JitError::Compilation("variable aggregate arguments".into()));
        };
        if returns.to_constant() != Some(0) {
            return Err(JitError::Compilation(
                "aggregate call returns values".into(),
            ));
        }
        let pattern = leaf::Pattern::recognize(callee)
            .ok_or_else(|| JitError::Compilation("invalid aggregate callee".into()))?;
        let Operation::Return { count, .. } = callee.operations[3] else {
            unreachable!()
        };
        if count.to_constant().is_none() {
            return Err(JitError::Compilation("variable aggregate return".into()));
        }
        match pattern.right {
            leaf::Operand::Constant(index) => {
                if callee.constants[usize::from(index)].tag != abi::INTEGER {
                    return Err(JitError::Compilation(
                        "non-integer aggregate constant".into(),
                    ));
                }
            }
            leaf::Operand::Register(register)
                if register != pattern.read && register.0 >= arguments =>
            {
                return Err(JitError::Compilation(
                    "aggregate operand is not an argument".into(),
                ));
            }
            _ => {}
        }
        let caller_bound = work::Expansion::admit(caller, limits)?;
        let remaining = work::Limits {
            instructions: limits.instructions - caller_bound.instructions,
            blocks: limits.blocks - caller_bound.blocks,
            relocations: limits.relocations,
        };
        let callee_bound = work::Expansion::admit(callee, remaining)?;
        let expansion = work::Expansion {
            instructions: caller_bound.instructions + callee_bound.instructions,
            blocks: caller_bound.blocks + callee_bound.blocks,
        };
        Ok(Self {
            caller,
            callee,
            pc,
            function: func,
            arguments,
            pattern,
            expansion,
        })
    }

    pub fn verify(&self, caller: &Snapshot, callee: &Snapshot) -> Result<(), JitError> {
        if !std::ptr::eq(caller, self.caller) || !std::ptr::eq(callee, self.callee) {
            return Err(JitError::Compilation("aggregate source mismatch".into()));
        }
        let rebuilt = Plan::new(
            caller,
            callee,
            self.pc,
            work::Limits {
                instructions: self.expansion.instructions,
                blocks: self.expansion.blocks,
                relocations: usize::MAX,
            },
        )?;
        if rebuilt.function != self.function
            || rebuilt.arguments != self.arguments
            || rebuilt.pattern != self.pattern
            || rebuilt.expansion != self.expansion
        {
            return Err(JitError::Compilation("aggregate plan mismatch".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{opcode::RCIndex, types::VarCount, Closure, JitConfig, Lua};

    fn sources(test: impl FnOnce(Snapshot, Snapshot, usize)) {
        sources_with(b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum", test);
    }

    fn sources_with(source: &[u8], test: impl FnOnce(Snapshot, Snapshot, usize)) {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, source).unwrap();
            let prototype = closure.prototype();
            let caller = Snapshot::new(&prototype, 4096, 1024 * 1024).unwrap();
            let callee = Snapshot::new(&prototype.prototypes[0], 4096, 1024 * 1024).unwrap();
            let pc = caller
                .operations
                .iter()
                .position(|op| matches!(op, Operation::Call { .. }))
                .unwrap();
            test(caller, callee, pc);
        });
    }

    #[test]
    fn admits_both_real_sources_and_exact_aggregate_bounds() {
        sources(|caller, callee, pc| {
            let limits = work::Limits::from(&JitConfig::default());
            let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
            plan.verify(&caller, &callee).unwrap();
            let first = work::Expansion::admit(&caller, limits).unwrap();
            let second = work::Expansion::admit(&callee, limits).unwrap();
            assert_eq!(
                plan.expansion.instructions,
                first.instructions + second.instructions
            );
            assert_eq!(plan.expansion.blocks, first.blocks + second.blocks);
            for (instructions, blocks) in [
                (plan.expansion.instructions - 1, plan.expansion.blocks),
                (plan.expansion.instructions, plan.expansion.blocks - 1),
            ] {
                assert!(Plan::new(
                    &caller,
                    &callee,
                    pc,
                    work::Limits {
                        instructions,
                        blocks,
                        ..limits
                    }
                )
                .is_err());
            }
            assert!(Plan::new(&caller, &callee, usize::MAX, limits).is_err());
            assert!(Plan::new(&caller, &callee, 0, limits).is_err());
            assert!(Plan::new(
                &caller,
                &callee,
                pc,
                work::Limits {
                    relocations: 2,
                    ..limits
                }
            )
            .is_err());
            Plan::new(
                &caller,
                &callee,
                pc,
                work::Limits {
                    relocations: 3,
                    ..limits
                },
            )
            .unwrap();
        });
    }

    #[test]
    fn rejects_call_operands_and_callee_mutations() {
        sources(|mut caller, mut callee, pc| {
            let limits = work::Limits::from(&JitConfig::default());
            let original = caller.operations[pc];
            let Operation::Call { func, args, .. } = original else {
                unreachable!()
            };
            for changed in [
                Operation::Call {
                    func,
                    args: VarCount::variable(),
                    returns: VarCount::constant(0),
                },
                Operation::Call {
                    func,
                    args: VarCount::constant(0),
                    returns: VarCount::constant(0),
                },
                Operation::Call {
                    func,
                    args,
                    returns: VarCount::constant(1),
                },
                Operation::TailCall { func, args },
            ] {
                caller.operations[pc] = changed;
                assert!(Plan::new(&caller, &callee, pc, limits).is_err());
            }
            caller.operations[pc] = original;
            let original_return = callee.operations[3];
            let Operation::Return { start, .. } = original_return else {
                unreachable!()
            };
            callee.operations[3] = Operation::Return {
                start,
                count: VarCount::variable(),
            };
            assert!(Plan::new(&caller, &callee, pc, limits).is_err());
            callee.operations[3] = original_return;
            let original = callee.operations[1];
            if let Operation::Add { dest, left, .. } = original {
                callee.operations[1] = Operation::Add {
                    dest,
                    left,
                    right: RCIndex::Register(RegisterIndex(250)),
                };
                assert!(Plan::new(&caller, &callee, pc, limits).is_err());
            } else {
                panic!("fixture has no add")
            }
        });
    }

    #[test]
    fn verification_rejects_forged_call_plan_fields() {
        sources(|caller, callee, pc| {
            let limits = work::Limits::from(&JitConfig::default());
            for fault in 0..5 {
                let mut plan = Plan::new(&caller, &callee, pc, limits).unwrap();
                match fault {
                    0 => plan.function.0 ^= 1,
                    1 => plan.arguments ^= 1,
                    2 => plan.pattern.upvalue ^= 1,
                    3 => plan.expansion.instructions -= 1,
                    4 => plan.pc = 0,
                    _ => unreachable!(),
                }
                assert!(plan.verify(&caller, &callee).is_err());
            }
        });
    }

    #[test]
    fn snapshot_roots_are_not_interchangeable_even_for_identical_operations() {
        sources(|caller, callee, pc| {
            let copy = Snapshot {
                operations: super::super::resources::owned(&caller.operations),
                constants: super::super::resources::owned(&caller.constants),
                registers: caller.registers,
                upvalues: caller.upvalues,
                prototypes: caller.prototypes,
            };
            let plan = Plan::new(
                &caller,
                &callee,
                pc,
                work::Limits::from(&JitConfig::default()),
            )
            .unwrap();
            assert!(plan.verify(&copy, &callee).is_err());
            assert!(plan.verify(&caller, &caller).is_err());
        });
    }

    #[test]
    fn argument_constant_and_post_read_alias_patterns_have_distinct_admission() {
        for (source, admitted) in [
            (
                "local n=0 local function f(v) n=n-v end f(1) return n",
                true,
            ),
            (
                "local n=1 local function f(v) n=n*v end f(2) return n",
                true,
            ),
            ("local n=0 local function f() n=n+7 end f() return n", true),
            ("local n=1 local function f() n=n+n end f() return n", false),
            (
                "local n=0 local function f() n=n+0.5 end f() return n",
                false,
            ),
            (
                "local n=0 local function f(v) n=n+v return n end local x=f(1) return x",
                false,
            ),
        ] {
            sources_with(source.as_bytes(), |caller, callee, pc| {
                assert_eq!(
                    Plan::new(
                        &caller,
                        &callee,
                        pc,
                        work::Limits::from(&JitConfig::default())
                    )
                    .is_ok(),
                    admitted,
                    "{source}"
                );
            });
        }
    }

    #[test]
    fn post_read_alias_does_not_require_an_argument_slot() {
        sources(|mut caller, mut callee, pc| {
            let pattern = leaf::Pattern::recognize(&callee).unwrap();
            let Operation::Call { func, returns, .. } = caller.operations[pc] else {
                unreachable!()
            };
            caller.operations[pc] = Operation::Call {
                func,
                args: VarCount::constant(0),
                returns,
            };
            let Operation::Add { dest, left, .. } = callee.operations[1] else {
                unreachable!()
            };
            callee.operations[1] = Operation::Add {
                dest,
                left,
                right: RCIndex::Register(pattern.read),
            };
            let plan = Plan::new(
                &caller,
                &callee,
                pc,
                work::Limits::from(&JitConfig::default()),
            )
            .unwrap();
            assert_eq!(plan.arguments, 0);
            plan.verify(&caller, &callee).unwrap();
        });
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn aggregate_entry_and_callee_ir_match_both_sources_and_reject_mutations() {
        use cranelift_codegen::{
            ir::{ExternalName, InstructionData},
            settings,
        };
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        sources(|caller, callee, pc| {
            let plan = Plan::new(
                &caller,
                &callee,
                pc,
                work::Limits::from(&JitConfig::default()),
            )
            .unwrap();
            let program = plan
                .program(isa.frontend_config(), isa.default_call_conv())
                .unwrap();
            cranelift_codegen::verify_function(&program.entry, isa.as_ref()).unwrap();
            cranelift_codegen::verify_function(&program.callee, isa.as_ref()).unwrap();
            plan.verify_program(&program, isa.frontend_config(), isa.default_call_conv())
                .unwrap();
            let mut tested = 0;
            for block in program.entry.layout.blocks() {
                for inst in program.entry.layout.block_insts(block) {
                    let mut changed = Program {
                        entry: program.entry.clone(),
                        callee: program.callee.clone(),
                    };
                    let mutated = match &mut changed.entry.dfg.insts[inst] {
                        InstructionData::UnaryImm { imm, .. } => {
                            *imm = (i64::from(*imm) ^ 1).into();
                            true
                        }
                        InstructionData::Brif { blocks, .. } => {
                            blocks.swap(0, 1);
                            true
                        }
                        InstructionData::Load { offset, .. } => {
                            *offset = (i32::from(*offset) + 1).into();
                            true
                        }
                        InstructionData::Call { func_ref, .. } => {
                            use cranelift_codegen::entity::EntityRef;
                            *func_ref =
                                cranelift_codegen::ir::FuncRef::new((func_ref.index() + 1) % 3);
                            true
                        }
                        _ => false,
                    };
                    if mutated {
                        tested += 1;
                        assert!(plan
                            .verify_program(
                                &changed,
                                isa.frontend_config(),
                                isa.default_call_conv()
                            )
                            .is_err());
                    }
                }
            }
            assert!(tested >= 12);
            for reference in program.entry.dfg.ext_funcs.keys() {
                let mut changed = Program {
                    entry: program.entry.clone(),
                    callee: program.callee.clone(),
                };
                changed.entry.dfg.ext_funcs[reference].name =
                    ExternalName::testcase("wrong_activation_target");
                assert!(plan
                    .verify_program(&changed, isa.frontend_config(), isa.default_call_conv())
                    .is_err());
            }
            let mut changed = Program {
                entry: program.entry.clone(),
                callee: program.callee.clone(),
            };
            changed.callee.signature.params[2].value_type = cranelift_codegen::ir::types::I64;
            assert!(plan
                .verify_program(&changed, isa.frontend_config(), isa.default_call_conv())
                .is_err());
        });
    }
}
