use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{types, Block, Function, InstBuilder, Value},
};

use super::{CompileError, IntCC, Paths};

pub(in crate::jit) struct Rewrite {
    argument: usize,
    constant: u64,
    body: Block,
    fallback: Block,
}

impl Rewrite {
    pub(in crate::jit) fn apply(
        pc: u32,
        function: &mut Function,
        paths: &Paths,
        fallback: Block,
    ) -> Result<Self, CompileError> {
        paths.points.get(pc as usize).ok_or_else(super::invalid)?;
        Self::bind(function, 1, u64::from(pc), fallback)
    }

    pub(in crate::jit) fn budget(
        budget: u32,
        function: &mut Function,
        fallback: Block,
    ) -> Result<Self, CompileError> {
        if budget > 64 {
            return Err(super::invalid());
        }
        Self::bind(function, 2, u64::from(budget), fallback)
    }

    fn bind(
        function: &mut Function,
        argument: usize,
        constant: u64,
        fallback: Block,
    ) -> Result<Self, CompileError> {
        let entry = function.layout.entry_block().ok_or_else(super::invalid)?;
        let parameters: [Value; 5] = function
            .dfg
            .block_params(entry)
            .try_into()
            .map_err(|_| super::invalid())?;
        let exit_parameters = function.dfg.block_params(fallback);
        if parameters.map(|value| function.dfg.value_type(value))
            != [types::I64, types::I64, types::I32, types::I64, types::I64]
            || exit_parameters.len() != 2
            || function.dfg.value_type(exit_parameters[0]) != types::I64
            || function.dfg.value_type(exit_parameters[1]) != types::I32
            || fallback == entry
        {
            return Err(super::invalid());
        }
        let body = function.dfg.make_block();
        function.layout.append_block(body);
        while let Some(inst) = function.layout.first_inst(entry) {
            function.layout.remove_inst(inst);
            function.layout.append_inst(inst, body);
        }
        let mut cursor = FuncCursor::new(function);
        cursor.goto_bottom(entry);
        let ty = cursor.func.dfg.value_type(parameters[argument]);
        let literal = cursor.ins().iconst(ty, constant as i64);
        let zero = cursor.ins().iconst(types::I32, 0);
        let equal = cursor
            .ins()
            .icmp(IntCC::Equal, parameters[argument], literal);
        cursor.ins().brif(
            equal,
            body,
            &[],
            fallback,
            &[parameters[1].into(), zero.into()],
        );
        let stencil = &mut **function;
        for block in stencil.layout.blocks().filter(|&block| block != entry) {
            for inst in stencil.layout.block_insts(block) {
                stencil.dfg.map_inst_values(inst, |value| {
                    if value == parameters[argument] {
                        literal
                    } else {
                        value
                    }
                });
            }
        }
        Ok(Self {
            argument,
            constant,
            body,
            fallback,
        })
    }

    pub(in crate::jit) fn verify(
        &self,
        original: &Function,
        function: &Function,
    ) -> Result<(), CompileError> {
        let entry = function.layout.entry_block().ok_or_else(super::invalid)?;
        let [literal, zero, comparison, branch] = super::instructions::<4>(function, entry)?;
        let parameters = function.dfg.block_params(entry);
        let pc = *parameters.get(1).ok_or_else(super::invalid)?;
        let parameter = *function
            .dfg
            .block_params(entry)
            .get(self.argument)
            .ok_or_else(super::invalid)?;
        let ty = match self.argument {
            1 => types::I64,
            2 => types::I32,
            _ => return Err(super::invalid()),
        };
        let value = super::result(function, literal, ty)?;
        let count = super::result(function, zero, types::I32)?;
        if function.dfg.value_type(pc) != types::I64
            || function.dfg.value_type(parameter) != ty
            || !super::constant(function, value, ty, self.constant)
            || !super::constant(function, count, types::I32, 0)
            || !super::compare(function, comparison, IntCC::Equal, parameter, value)
            || !function.dfg.block_params(self.body).is_empty()
        {
            return Err(super::invalid());
        }
        let condition = super::result(function, comparison, types::I8)?;
        let arms = super::conditional(function, branch, condition)?;
        super::no_args(function, arms[0], self.body)?;
        super::destination(function, arms[1], self.fallback, &[pc, count])?;
        let mut restored = function.clone();
        let stencil = &mut *restored;
        for block in stencil.layout.blocks().filter(|&block| block != entry) {
            for inst in stencil.layout.block_insts(block) {
                stencil
                    .dfg
                    .map_inst_values(inst, |arg| if arg == value { parameter } else { arg });
            }
        }
        for inst in [literal, zero, comparison, branch] {
            restored.layout.remove_inst(inst);
        }
        while let Some(inst) = restored.layout.first_inst(self.body) {
            restored.layout.remove_inst(inst);
            restored.layout.append_inst(inst, entry);
        }
        restored.layout.remove_block(self.body);
        if restored.display().to_string() != original.display().to_string() {
            return Err(super::invalid());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::{BlockCall, InstructionData};

    #[test]
    fn budget_binding_composes_without_changing_existing_control_flow() {
        for budget in [0, 1, 2, 3, 63, 64] {
            for pc in 0..2 {
                super::super::tests::fixture_with(
                    2,
                    None,
                    |function, _, paths, _, _, fallback, _| {
                        let original = function.clone();
                        let pc_binding = Rewrite::apply(pc, function, paths, fallback)?;
                        pc_binding.verify(&original, function)?;
                        let before_budget = function.clone();
                        let rewrite = Rewrite::budget(budget, function, fallback)?;
                        rewrite.verify(&before_budget, function)?;
                        cranelift_codegen::verify_function(
                            function,
                            &cranelift_codegen::settings::Flags::new(
                                cranelift_codegen::settings::builder(),
                            ),
                        )
                        .unwrap();
                        for invalid in [65, u32::MAX] {
                            let before = function.display().to_string();
                            assert!(Rewrite::budget(invalid, function, fallback).is_err());
                            assert_eq!(function.display().to_string(), before);
                        }
                        Ok(())
                    },
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn budget_guard_and_body_substitution_corruptions_are_refused() {
        for fault in 0..7 {
            super::super::tests::fixture_with(2, None, |function, _, paths, _, _, fallback, _| {
                Rewrite::apply(1, function, paths, fallback)?;
                let original = function.clone();
                let rewrite = Rewrite::budget(64, function, fallback)?;
                let entry = function.layout.entry_block().unwrap();
                let [literal, zero, comparison, branch] =
                    super::super::instructions::<4>(function, entry)?;
                let parameters = function.dfg.block_params(entry).to_vec();
                match fault {
                    0 | 1 => {
                        let inst = if fault == 0 { literal } else { zero };
                        let InstructionData::UnaryImm { imm, .. } = &mut function.dfg.insts[inst]
                        else {
                            unreachable!()
                        };
                        *imm = 63.into();
                    }
                    2 | 3 => {
                        let InstructionData::IntCompare { cond, args, .. } =
                            &mut function.dfg.insts[comparison]
                        else {
                            unreachable!()
                        };
                        if fault == 2 {
                            *cond = IntCC::UnsignedLessThanOrEqual;
                        } else {
                            args[0] = parameters[1];
                        }
                    }
                    4 => {
                        let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[branch]
                        else {
                            unreachable!()
                        };
                        blocks.swap(0, 1);
                    }
                    5 => {
                        let zero = super::super::result(function, zero, types::I32)?;
                        let target = BlockCall::new(
                            fallback,
                            [parameters[0].into(), zero.into()],
                            &mut function.dfg.value_lists,
                        );
                        let InstructionData::Brif { blocks, .. } = &mut function.dfg.insts[branch]
                        else {
                            unreachable!()
                        };
                        blocks[1] = target;
                    }
                    6 => {
                        let inst = function.layout.first_inst(rewrite.body).unwrap();
                        let InstructionData::UnaryImm { imm, .. } = &mut function.dfg.insts[inst]
                        else {
                            unreachable!()
                        };
                        *imm = 0.into();
                    }
                    _ => unreachable!(),
                }
                assert!(
                    rewrite.verify(&original, function).is_err(),
                    "fault {fault}"
                );
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn specialization_changes_only_the_guard_and_pc_uses() {
        for length in [1, 2, 7, 17] {
            for pc in 0..length {
                super::super::tests::fixture_with(
                    length,
                    None,
                    |function, _, paths, _, _, fallback, _| {
                        let original = function.clone();
                        let rewrite = Rewrite::apply(pc as u32, function, paths, fallback)?;
                        rewrite.verify(&original, function)?;
                        cranelift_codegen::verify_function(
                            function,
                            &cranelift_codegen::settings::Flags::new(
                                cranelift_codegen::settings::builder(),
                            ),
                        )
                        .unwrap();
                        let once = function.display().to_string();
                        rewrite.verify(&original, function)?;
                        assert_eq!(function.display().to_string(), once);
                        Ok(())
                    },
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn invalid_pc_refuses_without_mutation() {
        for pc in [2, u32::MAX] {
            super::super::tests::fixture_with(2, None, |function, _, paths, _, _, fallback, _| {
                let before = function.display().to_string();
                assert!(Rewrite::apply(pc, function, paths, fallback).is_err());
                assert_eq!(function.display().to_string(), before);
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn guard_target_budget_and_unknown_exit_mutations_are_refused() {
        for fault in 0..12 {
            super::super::tests::fixture_with(
                2,
                None,
                |function, _, paths, headers, root, fallback, exhausted| {
                    let original = function.clone();
                    let rewrite = Rewrite::apply(1, function, paths, fallback)?;
                    let entry = function.layout.entry_block().unwrap();
                    let [literal, zero, comparison, branch] =
                        super::super::instructions::<4>(function, entry)?;
                    match fault {
                        0 | 6 | 8 => {
                            let inst = match fault {
                                0 => literal,
                                8 => zero,
                                _ => function.layout.first_inst(root.unknown).unwrap(),
                            };
                            let InstructionData::UnaryImm { imm, .. } =
                                &mut function.dfg.insts[inst]
                            else {
                                unreachable!()
                            };
                            *imm = 2.into();
                        }
                        1 | 2 | 3 => {
                            let other = function.dfg.block_params(entry)[0];
                            let InstructionData::IntCompare { cond, args, .. } =
                                &mut function.dfg.insts[comparison]
                            else {
                                unreachable!()
                            };
                            match fault {
                                1 => *cond = IntCC::NotEqual,
                                2 => args[0] = other,
                                _ => args.swap(0, 1),
                            }
                        }
                        4 | 7 | 9 | 10 | 11 => {
                            let pc = function.dfg.block_params(entry)[1];
                            let count = super::super::result(function, zero, types::I32)?;
                            let bound = super::super::result(function, literal, types::I64)?;
                            let target = match fault {
                                4 => BlockCall::new(
                                    paths.points[0].trampoline,
                                    [],
                                    &mut function.dfg.value_lists,
                                ),
                                7 => BlockCall::new(
                                    paths.points[1].body,
                                    [],
                                    &mut function.dfg.value_lists,
                                ),
                                9 => BlockCall::new(
                                    exhausted,
                                    [pc.into(), count.into()],
                                    &mut function.dfg.value_lists,
                                ),
                                10 => BlockCall::new(
                                    fallback,
                                    [bound.into(), count.into()],
                                    &mut function.dfg.value_lists,
                                ),
                                _ => BlockCall::new(
                                    rewrite.body,
                                    [pc.into()],
                                    &mut function.dfg.value_lists,
                                ),
                            };
                            let InstructionData::Brif { blocks, .. } =
                                &mut function.dfg.insts[branch]
                            else {
                                unreachable!()
                            };
                            blocks[usize::from(matches!(fault, 9 | 10))] = target;
                        }
                        5 => {
                            let [_, compare, _] =
                                super::super::instructions::<3>(function, headers[1])?;
                            let InstructionData::IntCompare { cond, .. } =
                                &mut function.dfg.insts[compare]
                            else {
                                unreachable!()
                            };
                            *cond = IntCC::UnsignedLessThan;
                        }
                        _ => unreachable!(),
                    }
                    assert!(
                        rewrite.verify(&original, function).is_err(),
                        "fault {fault}"
                    );
                    Ok(())
                },
            )
            .unwrap();
        }
    }
}

#[cfg(not(miri))]
mod native;
