use super::{flow::FlowGraph, ir::Snapshot};

const MAX_OPERATIONS: usize = 128;

fn masks(source: &Snapshot, graph: &FlowGraph) -> Option<[u8; MAX_OPERATIONS]> {
    let length = source.operations.len();
    if !(1..=8).contains(&source.registers)
        || length > MAX_OPERATIONS
        || graph.nodes.len() != length
    {
        return None;
    }
    let mut writes = [0; MAX_OPERATIONS];
    for (pc, node) in graph.nodes.iter().enumerate() {
        if !node.lowering.native() {
            continue;
        }
        if node
            .native_effects
            .intersects(super::flow::Effects::WRITE_UPVALUE)
        {
            writes[pc] = u8::MAX;
        } else {
            for register in 0..source.registers {
                if node.access.writes.contains(register as u8) {
                    writes[pc] |= 1 << register;
                }
            }
        }
    }
    let mut result = writes;
    for _ in 1..64 {
        let previous = result;
        for (pc, node) in graph.nodes.iter().enumerate() {
            if !node.lowering.native() {
                continue;
            }
            for next in node.successors.0.into_iter().flatten() {
                result[pc] |= previous[next as usize];
            }
        }
        if result == previous {
            break;
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    mod oracle;
    use super::*;
    use crate::{
        opcode::Operation,
        types::{RegisterIndex as R, VarCount},
    };

    fn snapshot(operations: &[Operation], registers: usize) -> Snapshot {
        Snapshot {
            operations: super::super::resources::owned(operations),
            constants: super::super::resources::owned(&[]),
            registers,
            upvalues: 1,
            prototypes: 0,
        }
    }

    #[test]
    fn paths_stop_at_interpreted_calls_and_preserve_both_branch_arms() {
        let source = snapshot(
            &[
                Operation::Test {
                    value: R(0),
                    is_true: true,
                },
                Operation::LoadBool {
                    dest: R(1),
                    value: true,
                    skip_next: false,
                },
                Operation::LoadBool {
                    dest: R(2),
                    value: false,
                    skip_next: false,
                },
                Operation::Call {
                    func: R(3),
                    args: VarCount::constant(0),
                    returns: VarCount::constant(0),
                },
                Operation::LoadBool {
                    dest: R(4),
                    value: true,
                    skip_next: false,
                },
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
            ],
            5,
        );
        let graph = FlowGraph::new(&source).unwrap();
        let actual = masks(&source, &graph).unwrap();
        assert_eq!(&actual[..6], &[6, 6, 4, 0, 16, 0]);
    }

    #[test]
    fn native_upvalue_writes_conservatively_include_all_aliases() {
        let source = snapshot(
            &[
                Operation::LoadBool {
                    dest: R(0),
                    value: true,
                    skip_next: false,
                },
                Operation::SetUpValue {
                    dest: crate::types::UpValueIndex(0),
                    source: R(0),
                },
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
            ],
            8,
        );
        let graph = FlowGraph::new(&source).unwrap();
        assert_eq!(&masks(&source, &graph).unwrap()[..3], &[255, 255, 0]);
    }

    #[test]
    fn masks_cover_exactly_the_maximum_native_slice_without_unbounded_walks() {
        let mut operations = vec![
            Operation::LoadBool {
                dest: R(0),
                value: true,
                skip_next: false
            };
            65
        ];
        operations[64] = Operation::LoadBool {
            dest: R(7),
            value: false,
            skip_next: false,
        };
        operations.push(Operation::Return {
            start: R(0),
            count: VarCount::constant(0),
        });
        let source = snapshot(&operations, 8);
        let actual = masks(&source, &FlowGraph::new(&source).unwrap()).unwrap();
        assert_eq!(actual[0], 1);
        assert_eq!(actual[1], 129);
        assert_eq!(actual[64], 128);
        assert_eq!(actual[65], 0);
        for registers in [0, 9, 256] {
            let source = snapshot(
                &[Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                }],
                registers,
            );
            assert!(masks(&source, &FlowGraph::new(&source).unwrap()).is_none());
        }
        operations.resize(
            129,
            Operation::Return {
                start: R(0),
                count: VarCount::constant(0),
            },
        );
        let source = snapshot(&operations, 8);
        assert!(masks(&source, &FlowGraph::new(&source).unwrap()).is_none());
    }

    #[test]
    fn frozen_callback_workload_has_entry_specific_write_sets() {
        crate::Lua::empty().enter(|ctx| {
            let closure = crate::Closure::load(
                ctx,
                None,
                b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum",
            )
            .unwrap();
            let source = Snapshot::new(&closure.prototype(), 128, 1024 * 1024).unwrap();
            let actual = masks(&source, &FlowGraph::new(&source).unwrap()).unwrap();
            let full = ((1u16 << source.registers) - 1) as u8;
            assert!(actual[..source.operations.len()]
                .iter()
                .any(|mask| *mask != 0 && *mask != full));
            assert_eq!(source.registers, 7);
            assert_eq!(actual[5], 0b1100000);
            assert_eq!(actual[8], 0b1110011);
        });
    }
}
