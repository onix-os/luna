use allocator_api2::vec::Vec;

use crate::opcode::Operation;

use super::{abi, ir::Snapshot, JitError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Successors(pub [Option<u32>; 2]);

impl Successors {
    pub fn contains(self, pc: usize) -> bool {
        self.0
            .into_iter()
            .flatten()
            .any(|target| target as usize == pc)
    }
}

pub(super) fn successors(op: Operation, pc: usize, length: usize) -> Result<Successors, JitError> {
    use Operation::*;
    if pc >= length {
        return Err(JitError::Compilation(format!("invalid source PC {pc}")));
    }
    let next = pc.checked_add(1);
    let skip = pc.checked_add(2);
    let jump = |offset: i16| next.and_then(|pc| pc.checked_add_signed(isize::from(offset)));
    let (targets, count) = match op {
        Return { .. } | TailCall { .. } => ([None, None], 0),
        Jump { offset, .. } | NumericForPrep { jump: offset, .. } => ([jump(offset), None], 1),
        LoadBool {
            skip_next: true, ..
        } => ([skip, None], 1),
        Test { .. } | TestSet { .. } | Eq { .. } | Less { .. } | LessEq { .. } => ([next, skip], 2),
        NumericForLoop { jump: offset, .. } | GenericForLoop { jump: offset, .. } => {
            ([next, jump(offset)], 2)
        }
        Move { .. }
        | LoadConstant { .. }
        | LoadBool {
            skip_next: false, ..
        }
        | LoadNil { .. }
        | NewTable { .. }
        | GetTable { .. }
        | SetTable { .. }
        | GetUpTable { .. }
        | SetUpTable { .. }
        | SetList { .. }
        | Call { .. }
        | VarArgs { .. }
        | MarkToBeClosed { .. }
        | Closure { .. }
        | GenericForCall { .. }
        | Method { .. }
        | Concat { .. }
        | GetUpValue { .. }
        | SetUpValue { .. }
        | Length { .. }
        | Not { .. }
        | Minus { .. }
        | Add { .. }
        | Sub { .. }
        | Mul { .. }
        | Div { .. }
        | IDiv { .. }
        | Mod { .. }
        | Pow { .. }
        | BitAnd { .. }
        | BitOr { .. }
        | BitXor { .. }
        | ShiftLeft { .. }
        | ShiftRight { .. }
        | BitNot { .. } => ([next, None], 1),
    };
    let mut result = [None, None];
    for index in 0..count {
        result[index] = Some(
            targets[index]
                .filter(|target| *target < length)
                .and_then(|target| u32::try_from(target).ok())
                .ok_or_else(|| JitError::Compilation(format!("invalid successor at PC {pc}")))?,
        );
    }
    Ok(Successors(result))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lowering {
    Direct,
    GuardedScalar,
    ScalarOrHelper(u32),
    Helper(u32),
    Interpreter,
}

impl Lowering {
    pub fn native(self) -> bool {
        self != Self::Interpreter
    }

    pub fn accepts_helper(self, kind: u32) -> bool {
        matches!(self, Self::Helper(expected) | Self::ScalarOrHelper(expected) if kind == expected)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Effects(u16);

impl Effects {
    pub const READ_HEAP: u16 = 1 << 0;
    pub const WRITE_HEAP: u16 = 1 << 1;
    pub const READ_UPVALUE: u16 = 1 << 2;
    pub const WRITE_UPVALUE: u16 = 1 << 3;
    pub const ALLOCATE: u16 = 1 << 4;
    pub const USER_CODE: u16 = 1 << 5;
    pub const ERROR: u16 = 1 << 6;
    pub const RESHAPE_STACK: u16 = 1 << 7;
    pub const CLOSE: u16 = 1 << 8;
    pub const PANIC: u16 = 1 << 9;

    pub fn intersects(self, flags: u16) -> bool {
        self.0 & flags != 0
    }
    pub fn subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }
}

fn description(op: Operation) -> (Lowering, Effects, Effects) {
    use Effects as E;
    use Lowering as L;
    use Operation::*;
    let call = E::USER_CODE | E::RESHAPE_STACK | E::ALLOCATE | E::ERROR;
    let (lowering, whole, fast) = match op {
        Move { .. } => (L::ScalarOrHelper(abi::HELPER_MOVE), 0, E::PANIC),
        LoadConstant { .. } => (
            L::ScalarOrHelper(abi::HELPER_CONSTANT),
            E::READ_HEAP,
            E::READ_HEAP | E::PANIC,
        ),
        LoadBool { .. } | LoadNil { .. } | Not { .. } | Test { .. } => (L::Direct, 0, 0),
        Jump { close_upvalues, .. } if close_upvalues.is_none() => (L::Direct, 0, 0),
        Add { .. }
        | Sub { .. }
        | Mul { .. }
        | Div { .. }
        | Eq { .. }
        | Less { .. }
        | LessEq { .. } => (L::GuardedScalar, call, 0),
        NumericForPrep { .. } | NumericForLoop { .. } => (L::GuardedScalar, E::ERROR, 0),
        NewTable { .. } => (
            L::Helper(abi::HELPER_NEW_TABLE),
            E::ALLOCATE,
            E::ALLOCATE | E::PANIC,
        ),
        GetTable { .. } => (
            L::Helper(abi::HELPER_GET_TABLE),
            E::READ_HEAP | call,
            E::READ_HEAP | E::PANIC,
        ),
        SetTable { .. } => (
            L::Helper(abi::HELPER_SET_TABLE),
            E::READ_HEAP | E::WRITE_HEAP | call,
            E::READ_HEAP | E::WRITE_HEAP | E::ALLOCATE | E::PANIC,
        ),
        GetUpTable { .. } => (
            L::Helper(abi::HELPER_GET_UP_TABLE),
            E::READ_UPVALUE | E::READ_HEAP | call,
            E::READ_UPVALUE | E::READ_HEAP | E::PANIC,
        ),
        SetUpTable { .. } => (
            L::Helper(abi::HELPER_SET_UP_TABLE),
            E::READ_UPVALUE | E::READ_HEAP | E::WRITE_HEAP | call,
            E::READ_UPVALUE | E::READ_HEAP | E::WRITE_HEAP | E::ALLOCATE | E::PANIC,
        ),
        GetUpValue { .. } => (
            L::Helper(abi::HELPER_GET_UPVALUE),
            E::READ_UPVALUE,
            E::READ_UPVALUE | E::PANIC,
        ),
        SetUpValue { .. } => (
            L::Helper(abi::HELPER_SET_UPVALUE),
            E::WRITE_UPVALUE,
            E::WRITE_UPVALUE | E::PANIC,
        ),
        Call { .. } | TailCall { .. } | GenericForCall { .. } => (L::Interpreter, call, 0),
        Return { .. } | Jump { .. } => (
            L::Interpreter,
            call | E::CLOSE | E::WRITE_UPVALUE | E::READ_HEAP,
            0,
        ),
        VarArgs { .. } => (L::Interpreter, E::RESHAPE_STACK | E::ALLOCATE | E::ERROR, 0),
        MarkToBeClosed { .. } => (L::Interpreter, E::CLOSE | E::READ_HEAP | E::ERROR, 0),
        Closure { .. } => (
            L::Interpreter,
            E::READ_HEAP | E::READ_UPVALUE | E::WRITE_UPVALUE | E::ALLOCATE | E::ERROR,
            0,
        ),
        SetList { count, .. } if !count.is_variable() => (
            L::Helper(abi::HELPER_SET_LIST),
            E::READ_HEAP | E::WRITE_HEAP | E::ALLOCATE | E::ERROR,
            E::READ_HEAP | E::WRITE_HEAP | E::ALLOCATE | E::PANIC,
        ),
        SetList { .. } => (
            L::Interpreter,
            E::READ_HEAP | E::WRITE_HEAP | E::ALLOCATE | E::RESHAPE_STACK | E::ERROR,
            0,
        ),
        Method { .. } => (L::Interpreter, E::READ_HEAP | call, 0),
        Concat { .. }
        | Length { .. }
        | Minus { .. }
        | IDiv { .. }
        | Mod { .. }
        | Pow { .. }
        | BitAnd { .. }
        | BitOr { .. }
        | BitXor { .. }
        | ShiftLeft { .. }
        | ShiftRight { .. }
        | BitNot { .. } => (L::Interpreter, call, 0),
        TestSet { .. } | GenericForLoop { .. } => (L::Interpreter, 0, 0),
    };
    let whole = if whole & E::USER_CODE != 0 {
        whole | E::READ_HEAP | E::WRITE_HEAP | E::READ_UPVALUE | E::WRITE_UPVALUE | E::CLOSE
    } else {
        whole
    };
    (lowering, Effects(whole | E::PANIC), Effects(fast))
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Node {
    pub successors: Successors,
    pub lowering: Lowering,
    pub effects: Effects,
    pub native_effects: Effects,
    pub reachable: bool,
    pub block_start: u32,
    pub block_end: u32,
    pub exit: super::exits::Snapshot,
    pub access: super::access::Access,
}

impl Node {
    fn valid_admission(self) -> bool {
        let helper_matches = match self.lowering {
            Lowering::Helper(kind) | Lowering::ScalarOrHelper(kind) => {
                self.access.helper.is_some_and(|helper| helper.kind == kind)
            }
            Lowering::Direct | Lowering::GuardedScalar | Lowering::Interpreter => {
                self.access.helper.is_none()
            }
        };
        helper_matches
            && self.native_effects.subset_of(self.effects)
            && (!self.lowering.native()
                || !self
                    .native_effects
                    .intersects(Effects::USER_CODE | Effects::RESHAPE_STACK | Effects::CLOSE))
    }
}

pub(super) struct FlowGraph {
    pub nodes: Vec<Node, super::resources::BudgetAllocator>,
}

impl FlowGraph {
    pub fn new(snapshot: &Snapshot) -> Result<Self, JitError> {
        snapshot.verify()?;
        let allocator = snapshot.operations.allocator().clone();
        let mut nodes = Vec::new_in(allocator.clone());
        nodes
            .try_reserve_exact(snapshot.operations.len())
            .map_err(|_| JitError::ResourceLimit("control-flow snapshot"))?;
        let mut pending = Vec::new_in(allocator);
        pending
            .try_reserve_exact(snapshot.operations.len())
            .map_err(|_| JitError::ResourceLimit("control-flow snapshot"))?;
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            let (lowering, effects, native_effects) = description(op);
            let node = Node {
                successors: successors(op, pc, snapshot.operations.len())?,
                lowering,
                effects,
                native_effects,
                reachable: false,
                block_start: u32::MAX,
                block_end: 0,
                exit: super::exits::Snapshot::new(pc as u32, snapshot.registers as u16, lowering),
                access: super::access::Access::new(op, snapshot),
            };
            if !node.valid_admission() {
                return Err(JitError::Compilation(format!(
                    "invalid native admission at PC {pc}"
                )));
            }
            nodes.push(node);
        }
        nodes[0].reachable = true;
        pending.push(0u32);
        while let Some(pc) = pending.pop() {
            for target in nodes[pc as usize].successors.0.into_iter().flatten() {
                let node = &mut nodes[target as usize];
                if !node.reachable {
                    node.reachable = true;
                    pending.push(target);
                }
            }
        }
        nodes[0].block_start = 0;
        for (pc, &op) in snapshot.operations.iter().enumerate() {
            let node = nodes[pc];
            if !node.lowering.native() {
                nodes[pc].block_start = pc as u32;
            }
            let linear = node.lowering.native()
                && node.successors == Successors([Some((pc + 1) as u32), None])
                && !matches!(
                    op,
                    Operation::Jump { .. } | Operation::NumericForPrep { .. }
                );
            if !linear {
                if pc + 1 < nodes.len() {
                    nodes[pc + 1].block_start = (pc + 1) as u32;
                }
                for target in node.successors.0.into_iter().flatten() {
                    nodes[target as usize].block_start = target;
                }
            }
        }
        let mut start = 0;
        for node in &mut nodes {
            if node.block_start != u32::MAX {
                start = node.block_start;
            }
            node.block_start = start;
        }
        let mut end = nodes.len() as u32;
        for (pc, node) in nodes.iter_mut().enumerate().rev() {
            node.block_end = end;
            if node.block_start == pc as u32 {
                end = pc as u32;
            }
        }
        let graph = Self { nodes };
        for (pc, node) in graph.nodes.iter().enumerate() {
            if node.block_start as usize > pc
                || node.block_end as usize <= pc
                || node.block_end as usize > graph.nodes.len()
                || (!node.lowering.native()
                    && (node.block_start as usize != pc || node.block_end as usize != pc + 1))
                || node
                    .successors
                    .0
                    .into_iter()
                    .flatten()
                    .any(|target| !graph.permits_edge(pc, target as usize))
            {
                return Err(JitError::Compilation(format!("invalid region at PC {pc}")));
            }
        }
        Ok(graph)
    }

    pub fn permits_edge(&self, from: usize, to: usize) -> bool {
        let (Some(source), Some(target)) = (self.nodes.get(from), self.nodes.get(to)) else {
            return false;
        };
        source.successors.contains(to)
            && (target.block_start as usize == to
                || (source.block_start == target.block_start && to == from + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::super::resources::{BudgetAllocator, Ledger};
    use super::*;
    use crate::types::{Opt254, RegisterIndex, UpValueIndex, VarCount};

    fn ret() -> Operation {
        Operation::Return {
            start: RegisterIndex(0),
            count: VarCount::constant(0),
        }
    }

    fn snapshot(operations: &[Operation], allocator: BudgetAllocator) -> Snapshot {
        let mut owned = Vec::new_in(allocator.clone());
        owned.try_reserve_exact(operations.len()).unwrap();
        owned.extend_from_slice(operations);
        Snapshot {
            operations: owned,
            constants: Vec::new_in(allocator),
            registers: 4,
            upvalues: 1,
            prototypes: 1,
        }
    }

    #[test]
    fn blocks_partition_branches_merges_calls_loops_and_dead_code() {
        let mov = Operation::Move {
            dest: RegisterIndex(0),
            source: RegisterIndex(1),
        };
        let source = snapshot(
            &[
                mov,
                Operation::Test {
                    value: RegisterIndex(0),
                    is_true: true,
                },
                mov,
                mov,
                Operation::Call {
                    func: RegisterIndex(0),
                    args: VarCount::constant(0),
                    returns: VarCount::constant(0),
                },
                mov,
                Operation::Jump {
                    offset: -4,
                    close_upvalues: Opt254::none(),
                },
                mov,
                ret(),
            ],
            BudgetAllocator(Ledger::new(65536)),
        );
        let graph = FlowGraph::new(&source).unwrap();
        let expected = [
            (0, 2),
            (0, 2),
            (2, 3),
            (3, 4),
            (4, 5),
            (5, 7),
            (5, 7),
            (7, 8),
            (8, 9),
        ];
        for (pc, (node, bounds)) in graph.nodes.iter().zip(expected).enumerate() {
            assert_eq!((node.block_start, node.block_end), bounds, "PC {pc}");
            for target in node.successors.0.into_iter().flatten() {
                assert!(graph.permits_edge(pc, target as usize));
            }
        }
        assert!(!graph.nodes[7].reachable);
        assert!(!graph.nodes[8].reachable);
        assert!(graph.nodes[7].lowering.native());
        assert!(!graph.permits_edge(1, 4));
        assert!(!graph.permits_edge(9, 0));
        assert!(!graph.permits_edge(0, 9));
    }

    #[test]
    fn unconditional_next_jumps_and_interpreter_barriers_split_blocks() {
        let source = snapshot(
            &[
                Operation::NumericForPrep {
                    base: RegisterIndex(0),
                    jump: 0,
                },
                Operation::Jump {
                    offset: 0,
                    close_upvalues: Opt254::none(),
                },
                Operation::NewTable {
                    dest: RegisterIndex(0),
                    array_size: 0,
                    map_size: 0,
                },
                Operation::Jump {
                    offset: 0,
                    close_upvalues: Opt254::some(0),
                },
                Operation::LoadBool {
                    dest: RegisterIndex(1),
                    value: true,
                    skip_next: false,
                },
                ret(),
            ],
            BudgetAllocator(Ledger::new(65536)),
        );
        let graph = FlowGraph::new(&source).unwrap();
        for (pc, node) in graph.nodes.iter().enumerate() {
            assert_eq!(
                (node.block_start, node.block_end),
                (pc as u32, pc as u32 + 1)
            );
        }
        assert!(graph.nodes[2].lowering.native());
        assert!(!graph.nodes[3].lowering.native());
    }

    #[test]
    fn a_branch_cannot_enter_an_unmarked_region_interior() {
        let source = snapshot(
            &[
                Operation::Test {
                    value: RegisterIndex(0),
                    is_true: true,
                },
                Operation::LoadBool {
                    dest: RegisterIndex(1),
                    value: true,
                    skip_next: false,
                },
                ret(),
            ],
            BudgetAllocator(Ledger::new(65536)),
        );
        let mut graph = FlowGraph::new(&source).unwrap();
        assert!(graph.permits_edge(0, 2));
        graph.nodes[2].block_start = 1;
        assert!(!graph.permits_edge(0, 2));
    }

    #[test]
    fn seeded_region_partitions_preserve_every_validated_pc_and_edge() {
        let ledger = Ledger::new(65536);
        let mut random = 0x6c75_6e61_7265_6769u64;
        for case in 0..4096 {
            let length = case % 8 + 1;
            let mut operations = std::vec::Vec::new();
            for pc in 0..length {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let target = random as usize % length;
                let jump = (target as isize - pc as isize - 1) as i16;
                let op = match (random >> 32) % 8 {
                    0 => ret(),
                    1 => Operation::Jump {
                        offset: jump,
                        close_upvalues: Opt254::none(),
                    },
                    2 if pc + 1 < length => Operation::Move {
                        dest: RegisterIndex(0),
                        source: RegisterIndex(1),
                    },
                    3 if pc + 2 < length => Operation::Test {
                        value: RegisterIndex(0),
                        is_true: true,
                    },
                    4 if pc + 1 < length => Operation::NumericForLoop {
                        base: RegisterIndex(0),
                        jump,
                    },
                    5 if pc + 1 < length => Operation::Call {
                        func: RegisterIndex(0),
                        args: VarCount::constant(0),
                        returns: VarCount::constant(0),
                    },
                    6 if pc + 2 < length => Operation::LoadBool {
                        dest: RegisterIndex(0),
                        value: false,
                        skip_next: true,
                    },
                    _ => Operation::NumericForPrep {
                        base: RegisterIndex(0),
                        jump,
                    },
                };
                operations.push(op);
            }
            {
                let source = snapshot(&operations, BudgetAllocator(ledger.clone()));
                let graph = FlowGraph::new(&source).unwrap();
                let mut pc = 0;
                while pc < length {
                    let head = graph.nodes[pc];
                    assert_eq!(head.block_start as usize, pc, "case {case}");
                    let end = head.block_end as usize;
                    assert!(end > pc && end <= length);
                    for (index, node) in graph.nodes[pc..end].iter().enumerate() {
                        assert_eq!(node.block_start, head.block_start);
                        assert_eq!(node.block_end, head.block_end);
                        if pc + index + 1 < end {
                            assert!(node.lowering.native());
                            assert_eq!(
                                node.successors,
                                Successors([Some((pc + index + 1) as u32), None])
                            );
                        }
                        for target in node.successors.0.into_iter().flatten() {
                            let destination = graph.nodes[target as usize];
                            assert!(
                                destination.block_start == target
                                    || target as usize == pc + index + 1
                            );
                            if node.reachable {
                                assert!(destination.reachable);
                            }
                        }
                    }
                    pc = end;
                }
            }
            assert_eq!(ledger.current(), 0, "case {case}");
        }
        assert_eq!(ledger.refusals(), 0);
    }

    #[test]
    fn graph_tracks_reachable_loops_dead_cycles_and_terminal_edges() {
        let source = snapshot(
            &[
                Operation::Jump {
                    offset: 2,
                    close_upvalues: Opt254::none(),
                },
                Operation::Move {
                    dest: RegisterIndex(0),
                    source: RegisterIndex(1),
                },
                Operation::Jump {
                    offset: -2,
                    close_upvalues: Opt254::none(),
                },
                Operation::NumericForLoop {
                    base: RegisterIndex(0),
                    jump: -1,
                },
                ret(),
            ],
            BudgetAllocator(Ledger::new(65536)),
        );
        let graph = FlowGraph::new(&source).unwrap();
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|node| node.reachable)
                .collect::<std::vec::Vec<_>>(),
            [true, false, false, true, true]
        );
        assert_eq!(graph.nodes[0].successors, Successors([Some(3), None]));
        assert_eq!(graph.nodes[3].successors, Successors([Some(4), Some(3)]));
        assert_eq!(graph.nodes[4].successors, Successors([None, None]));
        assert!(graph.nodes[1].lowering.native());
        for (pc, node) in graph.nodes.iter().enumerate() {
            assert!(node.valid_admission());
            for target in node.successors.0.into_iter().flatten() {
                assert!(target < graph.nodes.len() as u32, "PC {pc}");
            }
        }
    }

    #[test]
    fn skip_branch_call_continuations_and_unconditional_prep_are_distinct() {
        use Operation::*;
        let conditional = [
            Test {
                value: RegisterIndex(0),
                is_true: true,
            },
            TestSet {
                dest: RegisterIndex(0),
                value: RegisterIndex(1),
                is_true: false,
            },
            Eq {
                left: RegisterIndex(0).into(),
                right: RegisterIndex(1).into(),
                skip_if: true,
            },
            Less {
                left: RegisterIndex(0).into(),
                right: RegisterIndex(1).into(),
                skip_if: false,
            },
            LessEq {
                left: RegisterIndex(0).into(),
                right: RegisterIndex(1).into(),
                skip_if: true,
            },
        ];
        for op in conditional {
            assert_eq!(
                successors(op, 0, 3).unwrap(),
                Successors([Some(1), Some(2)])
            );
            assert!(successors(op, 1, 3).is_err());
        }
        assert_eq!(
            successors(
                LoadBool {
                    dest: RegisterIndex(0),
                    value: true,
                    skip_next: true
                },
                0,
                3
            )
            .unwrap(),
            Successors([Some(2), None])
        );
        for op in [
            Call {
                func: RegisterIndex(0),
                args: VarCount::constant(0),
                returns: VarCount::constant(0),
            },
            GenericForCall {
                base: RegisterIndex(0),
                var_count: 1,
            },
        ] {
            assert_eq!(successors(op, 0, 2).unwrap(), Successors([Some(1), None]));
        }
        assert_eq!(
            successors(
                TailCall {
                    func: RegisterIndex(0),
                    args: VarCount::constant(0)
                },
                0,
                1
            )
            .unwrap(),
            Successors([None, None])
        );
        let source = snapshot(
            &[NumericForPrep {
                base: RegisterIndex(0),
                jump: -1,
            }],
            BudgetAllocator(Ledger::new(65536)),
        );
        source.verify().unwrap();
        assert_eq!(
            FlowGraph::new(&source).unwrap().nodes[0].successors,
            Successors([Some(0), None])
        );
    }

    #[test]
    fn every_instruction_is_validated_even_when_unreachable() {
        for invalid in [
            Operation::Jump {
                offset: i16::MIN,
                close_upvalues: Opt254::none(),
            },
            Operation::Jump {
                offset: i16::MAX,
                close_upvalues: Opt254::none(),
            },
            Operation::Move {
                dest: RegisterIndex(255),
                source: RegisterIndex(0),
            },
            Operation::LoadBool {
                dest: RegisterIndex(0),
                value: true,
                skip_next: true,
            },
        ] {
            let source = snapshot(&[ret(), invalid], BudgetAllocator(Ledger::new(65536)));
            assert!(matches!(
                FlowGraph::new(&source),
                Err(JitError::Compilation(_))
            ));
        }
        assert!(successors(ret(), usize::MAX, 1).is_err());
        assert!(successors(ret(), 0, 0).is_err());
    }

    #[test]
    fn helper_and_interpreter_effects_are_separate() {
        use Operation::*;
        let helpers = [
            (
                Move {
                    dest: RegisterIndex(0),
                    source: RegisterIndex(1),
                },
                abi::HELPER_MOVE,
                0,
            ),
            (
                LoadConstant {
                    dest: RegisterIndex(0),
                    constant: crate::types::ConstantIndex16(0),
                },
                abi::HELPER_CONSTANT,
                Effects::READ_HEAP,
            ),
            (
                NewTable {
                    dest: RegisterIndex(0),
                    array_size: 0,
                    map_size: 0,
                },
                abi::HELPER_NEW_TABLE,
                Effects::ALLOCATE,
            ),
            (
                GetTable {
                    dest: RegisterIndex(0),
                    table: RegisterIndex(1),
                    key: RegisterIndex(2).into(),
                },
                abi::HELPER_GET_TABLE,
                Effects::READ_HEAP,
            ),
            (
                SetTable {
                    table: RegisterIndex(0),
                    key: RegisterIndex(1).into(),
                    value: RegisterIndex(2).into(),
                },
                abi::HELPER_SET_TABLE,
                Effects::WRITE_HEAP | Effects::ALLOCATE,
            ),
            (
                GetUpTable {
                    dest: RegisterIndex(0),
                    table: UpValueIndex(0),
                    key: RegisterIndex(1).into(),
                },
                abi::HELPER_GET_UP_TABLE,
                Effects::READ_UPVALUE | Effects::READ_HEAP,
            ),
            (
                SetUpTable {
                    table: UpValueIndex(0),
                    key: RegisterIndex(1).into(),
                    value: RegisterIndex(2).into(),
                },
                abi::HELPER_SET_UP_TABLE,
                Effects::READ_UPVALUE | Effects::WRITE_HEAP | Effects::ALLOCATE,
            ),
            (
                GetUpValue {
                    dest: RegisterIndex(0),
                    source: UpValueIndex(0),
                },
                abi::HELPER_GET_UPVALUE,
                Effects::READ_UPVALUE,
            ),
            (
                SetUpValue {
                    dest: UpValueIndex(0),
                    source: RegisterIndex(0),
                },
                abi::HELPER_SET_UPVALUE,
                Effects::WRITE_UPVALUE,
            ),
            (
                SetList {
                    base: RegisterIndex(0),
                    count: VarCount::constant(2),
                },
                abi::HELPER_SET_LIST,
                Effects::READ_HEAP | Effects::WRITE_HEAP | Effects::ALLOCATE,
            ),
        ];
        for (op, helper, required) in helpers {
            let (lowering, effects, native) = description(op);
            assert!(lowering.native());
            assert!(lowering.accepts_helper(helper));
            for (_, other, _) in helpers {
                assert_eq!(lowering.accepts_helper(other), helper == other);
            }
            assert!(native.subset_of(effects));
            assert_eq!(native.0 & required, required);
            assert!(native.intersects(Effects::PANIC));
            assert!(
                !native.intersects(Effects::USER_CODE | Effects::CLOSE | Effects::RESHAPE_STACK)
            );
        }
        let (lowering, effects, fast) = description(GetTable {
            dest: RegisterIndex(0),
            table: RegisterIndex(1),
            key: RegisterIndex(2).into(),
        });
        assert!(lowering.native());
        assert!(effects.intersects(Effects::USER_CODE | Effects::ERROR));
        assert!(!fast.intersects(Effects::USER_CODE | Effects::ERROR));
        for op in [
            Call {
                func: RegisterIndex(0),
                args: VarCount::constant(0),
                returns: VarCount::constant(0),
            },
            ret(),
            Jump {
                offset: 0,
                close_upvalues: Opt254::some(0),
            },
        ] {
            let (lowering, effects, fast) = description(op);
            assert_eq!(lowering, Lowering::Interpreter);
            assert!(effects.intersects(Effects::USER_CODE | Effects::RESHAPE_STACK));
            assert_eq!(fast.0, 0);
        }
        let mut node = Node {
            successors: Successors([None, None]),
            lowering: Lowering::Direct,
            effects: Effects(Effects::USER_CODE),
            native_effects: Effects(Effects::USER_CODE),
            reachable: true,
            block_start: 0,
            block_end: 1,
            exit: super::super::exits::Snapshot::new(0, 4, Lowering::Direct),
            access: super::super::access::Access::default(),
        };
        assert!(!node.valid_admission());
        node.native_effects = Effects(Effects::ALLOCATE);
        assert!(!node.valid_admission());
    }

    #[test]
    fn all_scalar_and_fallback_opcodes_keep_their_explicit_lowering_class() {
        use Operation::*;
        macro_rules! binary {
            ($variant:ident) => {
                $variant {
                    dest: RegisterIndex(0),
                    left: RegisterIndex(1).into(),
                    right: RegisterIndex(2).into(),
                }
            };
        }
        for op in [
            LoadBool {
                dest: RegisterIndex(0),
                value: false,
                skip_next: false,
            },
            LoadNil {
                dest: RegisterIndex(0),
                count: 1,
            },
            Test {
                value: RegisterIndex(0),
                is_true: true,
            },
            Not {
                dest: RegisterIndex(0),
                source: RegisterIndex(1),
            },
            Jump {
                offset: 0,
                close_upvalues: Opt254::none(),
            },
        ] {
            assert_eq!(description(op).0, Lowering::Direct);
            assert_eq!(description(op).2 .0, 0);
        }
        for op in [
            binary!(Add),
            binary!(Sub),
            binary!(Mul),
            binary!(Div),
            NumericForPrep {
                base: RegisterIndex(0),
                jump: 0,
            },
            NumericForLoop {
                base: RegisterIndex(0),
                jump: -1,
            },
            Eq {
                left: RegisterIndex(0).into(),
                right: RegisterIndex(1).into(),
                skip_if: false,
            },
            Less {
                left: RegisterIndex(0).into(),
                right: RegisterIndex(1).into(),
                skip_if: false,
            },
            LessEq {
                left: RegisterIndex(0).into(),
                right: RegisterIndex(1).into(),
                skip_if: false,
            },
        ] {
            assert_eq!(description(op).0, Lowering::GuardedScalar);
            assert_eq!(description(op).2 .0, 0);
        }
        for op in [
            SetList {
                base: RegisterIndex(0),
                count: VarCount::variable(),
            },
            Call {
                func: RegisterIndex(0),
                args: VarCount::constant(0),
                returns: VarCount::constant(1),
            },
            TailCall {
                func: RegisterIndex(0),
                args: VarCount::constant(0),
            },
            ret(),
            VarArgs {
                dest: RegisterIndex(0),
                count: VarCount::constant(1),
            },
            MarkToBeClosed {
                source: RegisterIndex(0),
            },
            TestSet {
                dest: RegisterIndex(0),
                value: RegisterIndex(1),
                is_true: false,
            },
            Closure {
                dest: RegisterIndex(0),
                proto: crate::types::PrototypeIndex(0),
            },
            GenericForCall {
                base: RegisterIndex(0),
                var_count: 1,
            },
            GenericForLoop {
                base: RegisterIndex(0),
                jump: -1,
            },
            Method {
                base: RegisterIndex(0),
                table: RegisterIndex(1),
                key: RegisterIndex(2).into(),
            },
            Concat {
                dest: RegisterIndex(0),
                source: RegisterIndex(1),
                count: 2,
            },
            Length {
                dest: RegisterIndex(0),
                source: RegisterIndex(1),
            },
            Minus {
                dest: RegisterIndex(0),
                source: RegisterIndex(1),
            },
            BitNot {
                dest: RegisterIndex(0),
                source: RegisterIndex(1),
            },
            binary!(IDiv),
            binary!(Mod),
            binary!(Pow),
            binary!(BitAnd),
            binary!(BitOr),
            binary!(BitXor),
            binary!(ShiftLeft),
            binary!(ShiftRight),
            Jump {
                offset: 0,
                close_upvalues: Opt254::some(0),
            },
        ] {
            let (lowering, effects, native) = description(op);
            assert_eq!(lowering, Lowering::Interpreter);
            assert_eq!(native.0, 0);
            if effects.intersects(Effects::USER_CODE) {
                assert_eq!(
                    effects.0
                        & (Effects::READ_HEAP
                            | Effects::WRITE_HEAP
                            | Effects::READ_UPVALUE
                            | Effects::WRITE_UPVALUE),
                    Effects::READ_HEAP
                        | Effects::WRITE_HEAP
                        | Effects::READ_UPVALUE
                        | Effects::WRITE_UPVALUE
                );
            }
        }
    }

    #[test]
    fn graph_and_worklist_are_charged_together_and_refusal_rolls_back() {
        let ledger = Ledger::new(65536);
        let source = snapshot(&[ret()], BudgetAllocator(ledger.clone()));
        let before = ledger.current();
        let graph_bytes = std::mem::size_of::<Node>();
        let work_bytes = std::mem::size_of::<u32>();
        for allowance in [graph_bytes - 1, graph_bytes + work_bytes - 1] {
            ledger.set_limit(before + allowance);
            assert!(matches!(
                FlowGraph::new(&source),
                Err(JitError::ResourceLimit("control-flow snapshot"))
            ));
            assert_eq!(ledger.current(), before);
        }
        ledger.set_limit(before + graph_bytes + work_bytes);
        let graph = FlowGraph::new(&source).unwrap();
        assert_eq!(ledger.peak(), before + graph_bytes + work_bytes);
        assert_eq!(ledger.current(), before + graph_bytes);
        drop(graph);
        assert_eq!(ledger.current(), before);
        drop(source);
        assert_eq!(ledger.current(), 0);
        assert_eq!(ledger.refusals(), 2);
    }

    #[test]
    fn duplicate_edges_do_not_duplicate_work_and_owned_graph_outlives_arena() {
        fn owned<T: Send + 'static>() {}
        owned::<Snapshot>();
        owned::<FlowGraph>();
        let source = {
            let mut lua = crate::Lua::empty();
            lua.enter(|ctx| {
                let prototype = crate::FunctionPrototype::compile(
                    ctx,
                    "owned-cfg",
                    b"local t={} for i=1,3 do t[i]=i end return 'done'",
                )
                .unwrap();
                Snapshot::new(&prototype, 4096, 65536).unwrap()
            })
        };
        let graph = FlowGraph::new(&source).unwrap();
        assert!(graph.nodes[0].reachable);
        let duplicate = snapshot(
            &[
                Operation::NumericForLoop {
                    base: RegisterIndex(0),
                    jump: 0,
                },
                ret(),
            ],
            BudgetAllocator(Ledger::new(65536)),
        );
        let graph = FlowGraph::new(&duplicate).unwrap();
        assert_eq!(graph.nodes[0].successors, Successors([Some(1), Some(1)]));
        assert!(graph.nodes.iter().all(|node| node.reachable));
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn host_cfg_quota_refusal_precedes_codegen_and_preserves_interpreter_execution() {
        let mut lua = crate::Lua::empty();
        let (closure, bytes) = lua.enter(|ctx| {
            let closure = crate::Closure::load(
                ctx,
                Some("cfg-quota"),
                b"local total=0 for i=1,200 do total=total+i end return total",
            )
            .unwrap();
            let prototype = closure.prototype();
            let bytes = prototype.opcodes.len() * std::mem::size_of::<Operation>()
                + prototype.constants.len() * std::mem::size_of::<abi::Slot>();
            (ctx.stash(closure), bytes)
        });
        lua.set_jit_config(crate::JitConfig {
            mode: crate::JitMode::Auto,
            hot_threshold: 1,
            max_snapshot_bytes: bytes,
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(
            lua.prepare_jit(),
            Err(JitError::ResourceLimit("control-flow snapshot"))
        ));
        let refused = lua.jit_stats();
        assert_eq!(refused.snapshot_bytes, 0);
        assert_eq!(refused.code_bytes, 0);
        assert_eq!(refused.installed_regions, 0);
        assert_eq!(refused.compilation_failures, 1);
        assert_eq!(refused.snapshot_peak_bytes, bytes);
        let executor =
            lua.enter(|ctx| ctx.stash(crate::Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        let mut done = false;
        for _ in 0..100 {
            done = lua.enter(|ctx| {
                ctx.fetch(&executor)
                    .step(ctx, &mut crate::Fuel::with(64))
                    .unwrap()
            });
            if done {
                break;
            }
        }
        assert!(done);
        assert_eq!(
            lua.enter(|ctx| ctx
                .fetch(&executor)
                .take_result::<i64>(ctx)
                .unwrap()
                .unwrap()),
            20100
        );
        let after = lua.jit_stats();
        assert_eq!(after.native_entries, 0);
        assert_eq!(after.compilation_failures, 1);
        assert!(after.interpreted_instructions > 0);
        drop(executor);
        drop(closure);
        lua.gc_collect();
        lua.gc_collect();
        let retired = lua.jit_stats();
        assert_eq!(retired.registered_prototypes, 0);
        assert_eq!(retired.queued_requests, 0);
        assert_eq!(retired.metadata_bytes, 0);
        assert_eq!(retired.snapshot_bytes, 0);
        assert_eq!(retired.code_bytes, 0);
    }
}
