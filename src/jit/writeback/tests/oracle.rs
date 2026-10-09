use super::*;
use crate::{
    opcode::RCIndex,
    types::{Opt254, UpValueIndex},
};

fn walk(source: &Snapshot, start: usize, budget: usize) -> u8 {
    let mut seen = vec![false; source.operations.len() * 65];
    let mut pending = vec![(start, budget)];
    let mut written = 0;
    while let Some((pc, left)) = pending.pop() {
        if left == 0 || std::mem::replace(&mut seen[pc * 65 + left], true) {
            continue;
        }
        let mut successors = [Some(pc + 1), None];
        let bits = match source.operations[pc] {
            Operation::Move { dest, .. }
            | Operation::LoadBool {
                dest,
                skip_next: false,
                ..
            }
            | Operation::Not { dest, .. }
            | Operation::Add { dest, .. } => 1 << dest.0,
            Operation::LoadBool {
                dest,
                skip_next: true,
                ..
            } => {
                successors[0] = Some(pc + 2);
                1 << dest.0
            }
            Operation::LoadNil { dest, count } => (((1u16 << count) - 1) as u8) << dest.0,
            Operation::SetUpValue { .. } => 255,
            Operation::SetList { base, count } if !count.is_variable() => 1 << (base.0 + 1),
            Operation::Jump {
                offset,
                close_upvalues,
            } if close_upvalues.is_none() => {
                successors[0] = Some((pc as isize + 1 + offset as isize) as usize);
                0
            }
            Operation::Test { .. } => {
                successors[1] = Some(pc + 2);
                0
            }
            Operation::NumericForLoop { base, jump } => {
                successors[1] = Some((pc as isize + 1 + jump as isize) as usize);
                (1 << base.0) | (1 << (base.0 + 3))
            }
            Operation::Call { .. } | Operation::Return { .. } => continue,
            op => panic!("unmodeled oracle operation {op:?}"),
        };
        written |= bits;
        pending.extend(
            successors
                .into_iter()
                .flatten()
                .map(|next| (next, left - 1)),
        );
    }
    written
}

#[test]
fn independently_walked_paths_match_masks_for_loops_branches_and_aliases() {
    let mut seed = 0x15436194u32;
    for case in 0..32 {
        let mut operations = Vec::new();
        for pc in 0..12 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let a = R((seed % 8) as u8);
            let b = R(((seed >> 4) % 8) as u8);
            let jump = -(pc as i16) - 1;
            operations.push(match (seed >> 8) % 12 {
                0 => Operation::Move { dest: a, source: b },
                1 => Operation::LoadNil {
                    dest: a,
                    count: 8 - a.0,
                },
                2 => Operation::Not { dest: a, source: b },
                3 => Operation::Add {
                    dest: a,
                    left: RCIndex::Register(a),
                    right: RCIndex::Register(b),
                },
                4 => Operation::Test {
                    value: a,
                    is_true: false,
                },
                5 => Operation::SetUpValue {
                    dest: UpValueIndex(0),
                    source: a,
                },
                6 => Operation::Jump {
                    offset: jump,
                    close_upvalues: Opt254::none(),
                },
                7 => Operation::NumericForLoop { base: R(0), jump },
                8 => Operation::Call {
                    func: a,
                    args: VarCount::constant(0),
                    returns: VarCount::constant(0),
                },
                9 => Operation::SetList {
                    base: R(0),
                    count: VarCount::constant(3),
                },
                _ => Operation::LoadBool {
                    dest: a,
                    value: true,
                    skip_next: seed % 2 == 0,
                },
            });
        }
        operations.extend(
            [Operation::Return {
                start: R(0),
                count: VarCount::constant(0),
            }; 2],
        );
        let source = snapshot(&operations, 8);
        let actual = masks(&source, &FlowGraph::new(&source).unwrap()).unwrap();
        for pc in 0..source.operations.len() {
            assert_eq!(actual[pc], walk(&source, pc, 64), "case={case} pc={pc}");
            for budget in [0, 1, 2, 17, 63] {
                assert_eq!(walk(&source, pc, budget) & !actual[pc], 0);
            }
        }
    }
}
