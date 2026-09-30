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
}

impl Node {
    fn valid_effects(self) -> bool {
        self.native_effects.subset_of(self.effects)
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
            };
            if !node.valid_effects() {
                return Err(JitError::Compilation(format!(
                    "invalid native effects at PC {pc}"
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
        Ok(Self { nodes })
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
            assert!(node.valid_effects());
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
        };
        assert!(!node.valid_effects());
        node.native_effects = Effects(Effects::ALLOCATE);
        assert!(!node.valid_effects());
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
                count: VarCount::constant(1),
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
