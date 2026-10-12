use crate::opcode::Operation;

use super::super::{flow::FlowGraph, ir::Snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    resume: u32,
}

impl Plan {
    fn new(source: &Snapshot, graph: &FlowGraph) -> Option<Self> {
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

    fn source() -> Snapshot {
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
