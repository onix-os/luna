use crate::opcode::Operation;

use super::super::{flow::FlowGraph, ir::Snapshot};
use super::{Block, Function, Inst, IntCC, JitError, Paths, Root};
use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{types, InstBuilder, InstructionData, Opcode},
};

pub(in crate::jit) struct Rewrite {
    slow: Block,
    table: Inst,
    resume: u32,
}

impl Rewrite {
    pub(in crate::jit) fn apply(
        plan: Plan,
        function: &mut Function,
        paths: &Paths,
        root: Root,
    ) -> Result<Self, JitError> {
        let point = paths
            .points
            .get(plan.resume as usize)
            .ok_or_else(super::invalid)?;
        let [reduce, table] = super::instructions::<2>(function, root.dispatch)?;
        let index = super::result(function, reduce, types::I32)?;
        let InstructionData::BranchTable {
            opcode: Opcode::BrTable,
            arg,
            table: jump_table,
        } = function.dfg.insts[table]
        else {
            return Err(super::invalid());
        };
        if !super::same(function, arg, index) {
            return Err(super::invalid());
        }
        let destination = function.dfg.jump_tables[jump_table]
            .as_slice()
            .get(plan.resume as usize)
            .copied()
            .ok_or_else(super::invalid)?;
        super::no_args(function, destination, point.trampoline)?;
        let slow = function.dfg.make_block();
        function.layout.append_block(slow);
        function.layout.remove_inst(table);
        function.layout.append_inst(table, slow);
        let mut cursor = FuncCursor::new(function);
        cursor.goto_bottom(root.dispatch);
        let literal = cursor.ins().iconst(types::I32, i64::from(plan.resume));
        let equal = cursor.ins().icmp(IntCC::Equal, index, literal);
        cursor.ins().brif(equal, point.trampoline, &[], slow, &[]);
        Ok(Self {
            slow,
            table,
            resume: plan.resume,
        })
    }

    pub(in crate::jit) fn verify(
        &self,
        original: &Function,
        function: &Function,
        paths: &Paths,
        root: Root,
    ) -> Result<(), JitError> {
        let point = paths
            .points
            .get(self.resume as usize)
            .ok_or_else(super::invalid)?;
        let [reduce, literal, compare, branch] = super::instructions::<4>(function, root.dispatch)?;
        let index = super::result(function, reduce, types::I32)?;
        let value = super::result(function, literal, types::I32)?;
        if !super::constant(function, value, types::I32, u64::from(self.resume))
            || !super::compare(function, compare, IntCC::Equal, index, value)
            || super::instructions::<1>(function, self.slow)? != [self.table]
        {
            return Err(super::invalid());
        }
        let condition = super::result(function, compare, types::I8)?;
        let arms = super::conditional(function, branch, condition)?;
        super::no_args(function, arms[0], point.trampoline)?;
        super::no_args(function, arms[1], self.slow)?;
        let mut restored = function.clone();
        for inst in [literal, compare, branch] {
            restored.layout.remove_inst(inst);
        }
        restored.layout.remove_inst(self.table);
        restored.layout.append_inst(self.table, root.dispatch);
        restored.layout.remove_block(self.slow);
        if restored.display().to_string() != original.display().to_string() {
            return Err(super::invalid());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) struct Plan {
    resume: u32,
}

impl Plan {
    pub(in crate::jit) fn new(source: &Snapshot, graph: &FlowGraph) -> Option<Self> {
        if source.operations.len() != graph.nodes.len() {
            return None;
        }
        let mut calls = source
            .operations
            .iter()
            .enumerate()
            .filter_map(|(pc, op)| matches!(op, Operation::Call { .. }).then_some(pc));
        let call = calls.next()?;
        if calls.next().is_some() || !graph.nodes[call].reachable {
            return None;
        }
        let Operation::Call { args, returns, .. } = source.operations[call] else {
            return None;
        };
        let resume = call.checked_add(1)?;
        if args.is_variable()
            || returns.is_variable()
            || !graph.nodes.get(resume)?.lowering.native()
        {
            return None;
        }
        let enclosed = source.operations.iter().enumerate().any(|(pc, op)| {
            matches!(op, Operation::NumericForLoop { .. })
                && pc >= resume
                && graph.nodes[pc].reachable
                && graph.nodes[pc]
                    .successors
                    .0
                    .into_iter()
                    .flatten()
                    .any(|target| target as usize <= call)
        });
        enclosed.then_some(Self {
            resume: u32::try_from(resume).ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RegisterIndex as R, VarCount};

    #[test]
    fn guarded_route_preserves_every_original_block_and_generic_dispatch() {
        for length in [1, 2, 7, 17] {
            for resume in [0, length - 1] {
                super::super::tests::fixture_with(
                    length,
                    None,
                    |function, _, paths, _, root, _, _| {
                        let original = function.clone();
                        let rewrite = Rewrite::apply(
                            Plan {
                                resume: resume as u32,
                            },
                            function,
                            paths,
                            root,
                        )?;
                        cranelift_codegen::verify_function(
                            function,
                            &cranelift_codegen::settings::Flags::new(
                                cranelift_codegen::settings::builder(),
                            ),
                        )
                        .unwrap();
                        rewrite.verify(&original, function, paths, root)
                    },
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn invalid_preferred_pc_refuses_without_mutating_the_function() {
        for resume in [2, u32::MAX] {
            super::super::tests::fixture_with(2, None, |function, _, paths, _, root, _, _| {
                let original = function.display().to_string();
                assert!(Rewrite::apply(Plan { resume }, function, paths, root).is_err());
                assert_eq!(function.display().to_string(), original);
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn corrupt_pc_polarity_target_and_original_budget_are_refused() {
        for fault in 0..4 {
            super::super::tests::fixture_with(
                2,
                None,
                |function, _, paths, headers, root, _, _| {
                    let original = function.clone();
                    let rewrite = Rewrite::apply(Plan { resume: 1 }, function, paths, root)?;
                    let [_, literal, comparison, branch] =
                        super::super::instructions::<4>(function, root.dispatch)?;
                    match fault {
                        0 => {
                            if let InstructionData::UnaryImm { imm, .. } =
                                &mut function.dfg.insts[literal]
                            {
                                *imm = 0.into();
                            } else {
                                unreachable!()
                            }
                        }
                        1 => {
                            if let InstructionData::IntCompare { cond, .. } =
                                &mut function.dfg.insts[comparison]
                            {
                                *cond = IntCC::NotEqual;
                            } else {
                                unreachable!()
                            }
                        }
                        2 => {
                            let target = cranelift_codegen::ir::BlockCall::new(
                                paths.body(1),
                                [],
                                &mut function.dfg.value_lists,
                            );
                            if let InstructionData::Brif { blocks, .. } =
                                &mut function.dfg.insts[branch]
                            {
                                blocks[0] = target;
                            } else {
                                unreachable!()
                            }
                        }
                        3 => {
                            let [_, comparison, _] =
                                super::super::instructions::<3>(function, headers[1])?;
                            if let InstructionData::IntCompare { cond, .. } =
                                &mut function.dfg.insts[comparison]
                            {
                                *cond = IntCC::UnsignedLessThan;
                            } else {
                                unreachable!()
                            }
                        }
                        _ => unreachable!(),
                    }
                    assert!(
                        rewrite.verify(&original, function, paths, root).is_err(),
                        "fault {fault}"
                    );
                    Ok(())
                },
            )
            .unwrap();
        }
    }

    pub(super) fn source() -> Snapshot {
        use crate::opcode::RCIndex;
        Snapshot {
            prepared_keys: None,
            operations: super::super::super::resources::owned(&[
                Operation::NumericForPrep {
                    base: R(0),
                    jump: 3,
                },
                Operation::Move {
                    dest: R(5),
                    source: R(3),
                },
                Operation::Call {
                    func: R(4),
                    args: VarCount::constant(1),
                    returns: VarCount::constant(1),
                },
                Operation::Add {
                    dest: R(6),
                    left: RCIndex::Register(R(6)),
                    right: RCIndex::Register(R(4)),
                },
                Operation::NumericForLoop {
                    base: R(0),
                    jump: -4,
                },
                Operation::Return {
                    start: R(6),
                    count: VarCount::constant(1),
                },
            ]),
            constants: super::super::super::resources::owned(&[]),
            registers: 7,
            upvalues: 0,
            prototypes: 0,
        }
    }

    #[test]
    fn selects_the_native_resumption_after_a_fixed_call_in_a_numeric_loop() {
        for arguments in [0, 1, 2] {
            for returns in [0, 1, 2] {
                let mut source = source();
                source.operations[2] = Operation::Call {
                    func: R(4),
                    args: VarCount::constant(arguments),
                    returns: VarCount::constant(returns),
                };
                let graph = FlowGraph::new(&source).unwrap();
                let ledger = &source.operations.allocator().0;
                let before = ledger.current();
                assert_eq!(Plan::new(&source, &graph), Some(Plan { resume: 3 }));
                assert_eq!(ledger.current(), before);
            }
        }
    }

    #[test]
    fn refuses_ambiguous_unreachable_and_variable_call_sites() {
        for variant in 0..5 {
            let mut source = source();
            match variant {
                0 => source.operations[1] = source.operations[2],
                1 => {
                    source.operations[0] = Operation::Return {
                        start: R(6),
                        count: VarCount::constant(1),
                    }
                }
                2 => {
                    source.operations[2] = Operation::Move {
                        dest: R(4),
                        source: R(3),
                    }
                }
                3 | 4 => {
                    source.operations[2] = Operation::Call {
                        func: R(4),
                        args: if variant == 3 {
                            VarCount::variable()
                        } else {
                            VarCount::constant(1)
                        },
                        returns: if variant == 4 {
                            VarCount::variable()
                        } else {
                            VarCount::constant(1)
                        },
                    }
                }
                _ => unreachable!(),
            }
            let graph = FlowGraph::new(&source).unwrap();
            assert_eq!(Plan::new(&source, &graph), None, "variant {variant}");
        }
    }

    #[test]
    fn refuses_non_native_resumption_and_non_enclosing_backedges() {
        for variant in 0..3 {
            let mut source = source();
            match variant {
                0 => {
                    source.operations[3] = Operation::Return {
                        start: R(6),
                        count: VarCount::constant(1),
                    }
                }
                1 => {
                    source.operations[0] = Operation::Move {
                        dest: R(0),
                        source: R(0),
                    };
                    source.operations[4] = Operation::NumericForLoop {
                        base: R(0),
                        jump: -1,
                    }
                }
                _ => {}
            }
            let mut graph = FlowGraph::new(&source).unwrap();
            if variant == 1 {
                assert!(graph.nodes[2].reachable);
                assert!(graph.nodes[3].lowering.native());
            }
            if variant == 2 {
                graph.nodes.pop();
            }
            assert_eq!(Plan::new(&source, &graph), None, "variant {variant}");
        }
    }
}

#[cfg(not(miri))]
mod native;
