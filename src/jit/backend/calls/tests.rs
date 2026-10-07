use super::*;
use crate::{
    jit::{
        leaf, owner,
        resources::{BudgetAllocator, Ledger},
        work,
    },
    Closure, JitConfig, Lua, Value,
};

type Driver = unsafe fn(&CallCode, *mut c_void, u32) -> u32;
const DRIVERS: [Driver; 2] = [CallCode::invoke, CallCode::invoke_direct];

struct Bridge {
    frame: NativeFrame,
    slots: [Slot; 3],
    cell: Slot,
    view: leaf::View,
    expected: (u64, u32, u32),
    return_start: u32,
    entered: usize,
    left: usize,
    valid: bool,
    accept: bool,
    alias: Option<usize>,
}

impl Bridge {
    fn new(expected: (u64, u32, u32)) -> Self {
        Self {
            frame: NativeFrame {
                slots: std::ptr::null_mut(),
                view: std::ptr::null_mut(),
                exit: Exit::default(),
            },
            slots: [
                Slot::from_value(Value::Integer(2)),
                Slot::from_value(Value::Nil),
                Slot::from_value(Value::Nil),
            ],
            cell: Slot::from_value(Value::Integer(7)),
            view: leaf::View {
                version: leaf::VERSION,
                cell: std::ptr::null_mut(),
                reads: 0,
                writes: 0,
                dirty: 0,
            },
            expected,
            return_start: 0,
            entered: 0,
            left: 0,
            valid: true,
            accept: true,
            alias: None,
        }
    }
}

unsafe extern "C" fn enter(
    data: *mut c_void,
    pc: u64,
    function: u32,
    arguments: u32,
) -> *mut NativeFrame {
    let bridge = unsafe { &mut *data.cast::<Bridge>() };
    bridge.entered += 1;
    bridge.valid &= bridge.expected == (pc, function, arguments);
    if !bridge.valid || !bridge.accept {
        return std::ptr::null_mut();
    }
    bridge.frame.slots = bridge.slots.as_mut_ptr();
    bridge.frame.view = std::ptr::addr_of_mut!(bridge.view);
    bridge.view.cell = if let Some(index) = bridge.alias {
        let Some(slot) = bridge.slots.get_mut(index) else {
            return std::ptr::null_mut();
        };
        slot
    } else {
        std::ptr::addr_of_mut!(bridge.cell)
    };
    std::ptr::addr_of_mut!(bridge.frame)
}

unsafe extern "C" fn leave(data: *mut c_void, frame: *mut NativeFrame, pc: u64, start: u32) -> u32 {
    let bridge = unsafe { &mut *data.cast::<Bridge>() };
    bridge.left += 1;
    bridge.valid &=
        frame == std::ptr::addr_of_mut!(bridge.frame) && pc == 3 && start == bridge.return_start;
    u32::from(bridge.frame.exit.pc == 3 && bridge.frame.exit.instructions == 3) + 1
}

fn hooks() -> Hooks {
    Hooks { enter, leave }
}

fn fixture(
    source: &[u8],
    test: impl FnOnce(Snapshot, Snapshot, usize, BudgetAllocator, MappingCounter),
) {
    Lua::empty().enter(|ctx| {
        let root = Ledger::new(8 * 1024 * 1024);
        let workspace = BudgetAllocator(Ledger::child(2 * 1024 * 1024, root.clone()));
        let metadata = BudgetAllocator(Ledger::child(2 * 1024 * 1024, root.clone()));
        let mappings = MappingCounter::new(Ledger::child(2 * 1024 * 1024, root));
        let closure = Closure::load(ctx, None, source).unwrap();
        let prototype = closure.prototype();
        let caller = Snapshot::new_in(&prototype, 4096, workspace.clone()).unwrap();
        let callee = Snapshot::new_in(&prototype.prototypes[0], 4096, workspace).unwrap();
        let pc = caller
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Call { .. }))
            .unwrap();
        test(caller, callee, pc, metadata, mappings);
    });
}

const ADD: &[u8] = b"local n=7 local function f(v) n=n+v end f(2) return n";

#[test]
fn both_drivers_use_nonzero_source_return_operand() {
    fixture(
        b"local n=7 local function f(v,w) n=n+v end f(2,99) return n",
        |caller, mut callee, pc, metadata, mappings| {
            callee.operations[3] = Operation::Return {
                start: crate::types::RegisterIndex(1),
                count: crate::types::VarCount::constant(1),
            };
            callee.verify().unwrap();
            let limits = work::Limits::from(&JitConfig::default());
            let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
            assert_eq!(plan.arguments, 2);
            assert_eq!(plan.operands().3, 1);
            let code = compile(
                &plan,
                hooks(),
                mappings,
                2 * 1024 * 1024,
                metadata,
                limits,
                Failure::None,
                LinkFault::None,
            )
            .unwrap();
            for invoke in DRIVERS {
                let mut bridge = Bridge::new((pc as u64, plan.function.0.into(), 2));
                bridge.return_start = 1;
                bridge.slots[1] = Slot::from_value(Value::Integer(99));
                assert_eq!(
                    unsafe { invoke(&code, std::ptr::addr_of_mut!(bridge).cast(), 64) },
                    2
                );
                assert!(bridge.valid);
                assert_eq!((bridge.entered, bridge.left), (1, 1));
                assert_eq!(bridge.cell.bits, 9);
                assert_eq!(bridge.slots[1].bits, 99);
            }
        },
    );
}

#[test]
fn native_entry_calls_verified_callee_and_both_typed_hooks() {
    for invoke in DRIVERS {
        exercise_entries(invoke);
    }
}

fn exercise_entries(invoke: Driver) {
    for (source, expected) in [
        (ADD, 9),
        (
            &b"local n=7 local function f(v) n=n-v end f(2) return n"[..],
            5,
        ),
        (
            &b"local n=7 local function f(v) n=n*v end f(2) return n"[..],
            14,
        ),
    ] {
        fixture(source, |caller, callee, pc, metadata, mappings| {
            let workspace = caller.operations.allocator().0.clone();
            let baseline = (
                workspace.current(),
                metadata.0.current(),
                mappings.load(Ordering::Relaxed),
            );
            let limits = work::Limits::from(&JitConfig::default());
            let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
            let code = compile(
                &plan,
                hooks(),
                mappings.clone(),
                2 * 1024 * 1024,
                metadata.clone(),
                limits,
                Failure::None,
                LinkFault::None,
            )
            .unwrap();
            assert!(code.bytes > 0 && code.relocations >= 2);
            let mut bridge = Bridge::new((
                pc as u64,
                u32::from(plan.function.0),
                u32::from(plan.arguments),
            ));
            assert_eq!(
                unsafe { invoke(&code, std::ptr::addr_of_mut!(bridge).cast(), 64) },
                2
            );
            assert!(bridge.valid);
            assert_eq!((bridge.entered, bridge.left), (1, 1));
            assert_eq!(
                (bridge.cell.tag, bridge.cell.bits),
                (abi::INTEGER, expected)
            );
            assert_eq!(
                (
                    bridge.frame.exit.pc,
                    bridge.frame.exit.instructions,
                    bridge.view.reads,
                    bridge.view.writes,
                    bridge.view.dirty
                ),
                (3, 3, 1, 1, 1)
            );
            drop(code);
            assert_eq!(
                (
                    workspace.current(),
                    metadata.0.current(),
                    mappings.load(Ordering::Relaxed)
                ),
                baseline
            );
            assert_eq!(mappings.requested(), 0);
        });
    }
}

#[test]
fn native_entry_refusal_and_callee_guards_preserve_buffers() {
    for invoke in DRIVERS {
        exercise_refusals(invoke);
    }
}

fn exercise_refusals(invoke: Driver) {
    fixture(ADD, |caller, callee, pc, metadata, mappings| {
        let limits = work::Limits::from(&JitConfig::default());
        let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
        let code = compile(
            &plan,
            hooks(),
            mappings,
            2 * 1024 * 1024,
            metadata,
            limits,
            Failure::None,
            LinkFault::None,
        )
        .unwrap();
        for fault in 0..5 {
            let mut bridge = Bridge::new((
                pc as u64,
                u32::from(plan.function.0),
                u32::from(plan.arguments),
            ));
            match fault {
                0 => bridge.accept = false,
                1 => bridge.expected.0 += 1,
                2 => bridge.view.version ^= 1,
                3 => bridge.slots[0] = Slot::from_value(Value::Number(2.0)),
                _ => {}
            }
            let before = bridge.slots.map(|slot| (slot.tag, slot.bits));
            let budget = if fault == 4 { 3 } else { 64 };
            assert_eq!(
                unsafe { invoke(&code, std::ptr::addr_of_mut!(bridge).cast(), budget) },
                if fault < 2 { 0 } else { 1 }
            );
            assert_eq!(bridge.entered, 1);
            assert_eq!(bridge.left, usize::from(fault >= 2));
            assert_eq!(bridge.slots.map(|slot| (slot.tag, slot.bits)), before);
            assert_eq!((bridge.cell.tag, bridge.cell.bits), (abi::INTEGER, 7));
            assert_eq!(
                (
                    bridge.frame.exit.instructions,
                    bridge.view.reads,
                    bridge.view.writes,
                    bridge.view.dirty
                ),
                (0, 0, 0, 0)
            );
        }
    });
}

#[test]
fn native_budgets_aliases_and_wrapping_extremes_are_source_exact() {
    for invoke in DRIVERS {
        exercise_budgets(invoke);
    }
}

fn exercise_budgets(invoke: Driver) {
    fixture(ADD, |caller, callee, pc, metadata, mappings| {
        let limits = work::Limits::from(&JitConfig::default());
        let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
        let code = compile(
            &plan,
            hooks(),
            mappings,
            2 * 1024 * 1024,
            metadata,
            limits,
            Failure::None,
            LinkFault::None,
        )
        .unwrap();
        for budget in [0, 1, 2, 3, 4, 64] {
            for alias in [
                None,
                Some(0),
                Some(usize::from(plan.pattern.read.0)),
                Some(usize::from(plan.pattern.result.0)),
            ] {
                for left in [i64::MIN, -1, 0, i64::MAX] {
                    let mut bridge = Bridge::new((
                        pc as u64,
                        u32::from(plan.function.0),
                        u32::from(plan.arguments),
                    ));
                    bridge.cell = Slot::from_value(Value::Integer(left));
                    bridge.alias = alias;
                    if let Some(index) = alias {
                        bridge.slots[index] = bridge.cell;
                    }
                    let before = bridge.slots.map(|slot| (slot.tag, slot.bits));
                    let result =
                        unsafe { invoke(&code, std::ptr::addr_of_mut!(bridge).cast(), budget) };
                    assert!(bridge.valid);
                    assert_eq!((bridge.entered, bridge.left), (1, 1));
                    if budget <= 3 {
                        assert_eq!(result, 1);
                        assert_eq!(bridge.slots.map(|slot| (slot.tag, slot.bits)), before);
                        assert_eq!(bridge.cell.bits, left as u64);
                        assert_eq!(
                            (
                                bridge.frame.exit.pc,
                                bridge.frame.exit.instructions,
                                bridge.view.reads,
                                bridge.view.writes,
                                bridge.view.dirty
                            ),
                            (0, 0, 0, 0, 0)
                        );
                    } else {
                        let right = if alias == Some(0) { left } else { 2 };
                        let expected = left.wrapping_add(right) as u64;
                        assert_eq!(result, 2);
                        let mut expected_slots = before;
                        expected_slots[usize::from(plan.pattern.read.0)] =
                            (abi::INTEGER, left as u64);
                        expected_slots[usize::from(plan.pattern.result.0)] =
                            (abi::INTEGER, expected);
                        if let Some(index) = alias {
                            expected_slots[index] = (abi::INTEGER, expected);
                        }
                        assert_eq!(
                            bridge.slots.map(|slot| (slot.tag, slot.bits)),
                            expected_slots
                        );
                        assert_eq!(
                            if let Some(index) = alias {
                                bridge.slots[index].bits
                            } else {
                                bridge.cell.bits
                            },
                            expected
                        );
                        assert_eq!(
                            (
                                bridge.frame.exit.pc,
                                bridge.frame.exit.instructions,
                                bridge.view.reads,
                                bridge.view.writes,
                                bridge.view.dirty
                            ),
                            (3, 3, 1, 1, 1)
                        );
                    }
                }
            }
        }
    });
}

#[test]
fn owned_native_code_outlives_both_compiler_snapshots() {
    for invoke in DRIVERS {
        exercise_snapshot_lifetime(invoke);
    }
}

fn exercise_snapshot_lifetime(invoke: Driver) {
    fixture(ADD, |caller, callee, pc, metadata, mappings| {
        let workspace = caller.operations.allocator().0.clone();
        let limits = work::Limits::from(&JitConfig::default());
        let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
        let expected = (
            pc as u64,
            u32::from(plan.function.0),
            u32::from(plan.arguments),
        );
        let code = compile(
            &plan,
            hooks(),
            mappings.clone(),
            2 * 1024 * 1024,
            metadata.clone(),
            limits,
            Failure::None,
            LinkFault::None,
        )
        .unwrap();
        drop(plan);
        drop(caller);
        drop(callee);
        assert_eq!(workspace.current(), 0);
        let mut bridge = Bridge::new(expected);
        assert_eq!(
            unsafe { invoke(&code, std::ptr::addr_of_mut!(bridge).cast(), 64) },
            2
        );
        assert!(bridge.valid);
        assert_eq!(bridge.cell.bits, 9);
        drop(code);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(mappings.load(Ordering::Relaxed), 0);
        assert_eq!(mappings.requested(), 0);
    });
}

#[test]
fn linked_target_mutations_and_late_refusals_keep_peer_mappings_and_leases() {
    for invoke in DRIVERS {
        exercise_refusal_lifetime(invoke);
    }
}

fn exercise_refusal_lifetime(invoke: Driver) {
    fixture(ADD, |caller, callee, pc, metadata, mappings| {
        let limits = work::Limits::from(&JitConfig::default());
        let plan = Plan::new(&caller, &callee, pc, limits).unwrap();
        let build = |failure, fault, limits, image_limit| {
            compile(
                &plan,
                hooks(),
                mappings.clone(),
                image_limit,
                metadata.clone(),
                limits,
                failure,
                fault,
            )
        };
        let code = build(Failure::None, LinkFault::None, limits, 2 * 1024 * 1024).unwrap();
        let actual_relocations = code.relocations;
        let before_owner = metadata.0.current();
        let peer = owner::Shared::try_new(code, metadata.clone()).unwrap();
        assert_eq!(
            metadata.0.current() - before_owner,
            owner::Shared::<CallCode>::allocation_bytes()
        );
        let lease = peer.clone();
        drop(peer);
        let workspace = caller.operations.allocator().0.clone();
        let baseline = (
            workspace.current(),
            metadata.0.current(),
            mappings.load(Ordering::Relaxed),
            mappings.requested(),
        );
        for fault in [
            LinkFault::CalleeTarget,
            LinkFault::EnterTarget,
            LinkFault::LeaveTarget,
            LinkFault::Signature,
        ] {
            assert!(build(Failure::None, fault, limits, 2 * 1024 * 1024).is_err());
            assert_eq!(
                (
                    workspace.current(),
                    metadata.0.current(),
                    mappings.load(Ordering::Relaxed),
                    mappings.requested()
                ),
                baseline
            );
        }
        for failure in [
            Failure::Allocate,
            Failure::Protect,
            Failure::ProtectAfterFirst,
            Failure::RefuseRelocationCopy,
            Failure::RefuseSymbols,
            Failure::RefuseSignatures,
        ] {
            let previous = workspace.limit();
            assert!(build(failure, LinkFault::None, limits, 2 * 1024 * 1024).is_err());
            workspace.set_limit(previous);
            assert_eq!(
                (
                    workspace.current(),
                    metadata.0.current(),
                    mappings.load(Ordering::Relaxed),
                    mappings.requested()
                ),
                baseline
            );
        }
        assert!(build(
            Failure::None,
            LinkFault::None,
            work::Limits {
                relocations: actual_relocations - 1,
                ..limits
            },
            2 * 1024 * 1024
        )
        .is_err());
        assert!(build(Failure::None, LinkFault::None, limits, baseline.2).is_err());
        let code = build(Failure::None, LinkFault::None, limits, 2 * 1024 * 1024).unwrap();
        let previous = metadata.0.limit();
        metadata.0.set_limit(metadata.0.current());
        assert!(owner::Shared::try_new(code, metadata.clone()).is_err());
        metadata.0.set_limit(previous);
        assert_eq!(
            (
                workspace.current(),
                metadata.0.current(),
                mappings.load(Ordering::Relaxed),
                mappings.requested()
            ),
            baseline
        );
        let mut bridge = Bridge::new((
            pc as u64,
            u32::from(plan.function.0),
            u32::from(plan.arguments),
        ));
        assert_eq!(
            unsafe { invoke(&lease, std::ptr::addr_of_mut!(bridge).cast(), 64) },
            2
        );
        assert_eq!(bridge.cell.bits, 9);
        drop(lease);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(mappings.load(Ordering::Relaxed), 0);
        assert_eq!(mappings.requested(), 0);
    });
}
