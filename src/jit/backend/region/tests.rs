use super::*;
use crate::jit::{resources::Ledger, work::Limits};

struct Resources {
    root: super::super::super::resources::LedgerRef,
    metadata: BudgetAllocator,
    workspace: BudgetAllocator,
    mappings: MappingCounter,
}

impl Resources {
    fn new() -> Self {
        let root = Ledger::new(8 * 1024 * 1024);
        Self {
            metadata: BudgetAllocator(Ledger::child(2 * 1024 * 1024, root.clone())),
            workspace: BudgetAllocator(Ledger::child(2 * 1024 * 1024, root.clone())),
            mappings: MappingCounter::new(Ledger::child(2 * 1024 * 1024, root.clone())),
            root,
        }
    }

    fn usage(&self) -> [usize; 4] {
        [
            self.root.current(),
            self.metadata.0.current(),
            self.workspace.0.current(),
            self.mappings.load(Ordering::Relaxed),
        ]
    }

    fn compile(&self, limits: Limits, bytes: usize, fault: Fault) -> Result<Driver, JitError> {
        compile(
            caller,
            boundary,
            self.mappings.clone(),
            bytes,
            self.metadata.clone(),
            self.workspace.clone(),
            limits,
            fault,
        )
    }
}

fn limits() -> Limits {
    Limits {
        instructions: 64,
        blocks: 4,
        relocations: 64,
    }
}

struct State {
    slots: [*mut Slot; 2],
    hosts: [*mut abi::Host; 2],
    exits: Vec<(u64, u32, u32)>,
    stop_after: usize,
    status: u32,
}

unsafe extern "C" fn caller(
    slots: *mut Slot,
    pc: u64,
    budget: u32,
    host: *mut abi::Host,
) -> abi::return_words::Words {
    unsafe {
        (*slots).bits += 1;
        abi::return_words::Words::from_exit(Exit {
            pc,
            instructions: budget,
            reason: *(*host).data.cast::<u32>(),
        })
    }
}

unsafe extern "C" fn boundary(view: *mut View) -> u32 {
    let view = unsafe { &mut *view };
    let state = unsafe { &mut *view.data.cast::<State>() };
    state
        .exits
        .push((view.exit.pc, view.exit.instructions, view.exit.reason));
    let next = state.exits.len() % 2;
    view.slots = state.slots[next];
    view.host = state.hosts[next];
    view.pc += 7;
    view.budget += 1;
    if state.exits.len() >= state.stop_after {
        state.status
    } else {
        1
    }
}

#[test]
fn generated_driver_is_hard_bounded_and_reloads_published_arguments() {
    let resources = Resources::new();
    let baseline = resources.usage();
    let driver = resources
        .compile(limits(), 2 * 1024 * 1024, Fault::None)
        .unwrap();
    assert!(resources.mappings.load(Ordering::Relaxed) > 0);
    assert_eq!(resources.workspace.0.current(), baseline[2]);
    for limit in [0, 1, 2, 37] {
        for status in [0, 1, 2, u32::MAX] {
            let mut slots = [Slot {
                tag: abi::INTEGER,
                bits: 0,
            }; 2];
            let mut tags = [31_u32, 47];
            let mut hosts = tags.each_mut().map(|tag| abi::Host {
                data: std::ptr::from_mut(tag).cast(),
                projection: std::ptr::null_mut(),
            });
            let mut state = State {
                slots: slots.each_mut().map(std::ptr::from_mut),
                hosts: hosts.each_mut().map(std::ptr::from_mut),
                exits: Vec::with_capacity(limit as usize),
                stop_after: 3,
                status,
            };
            let mut view = View {
                slots: state.slots[0],
                host: state.hosts[0],
                pc: 11,
                budget: 2,
                exit: Exit::default(),
                data: std::ptr::from_mut(&mut state).cast(),
            };
            unsafe { driver.invoke(&mut view, limit) };
            let expected = if status == 1 {
                limit as usize
            } else {
                (limit as usize).min(3)
            };
            assert_eq!(state.exits.len(), expected);
            for (index, actual) in state.exits.iter().enumerate() {
                assert_eq!(
                    *actual,
                    (11 + index as u64 * 7, 2 + index as u32, tags[index % 2])
                );
            }
            assert_eq!(slots[0].bits, expected.div_ceil(2) as u64);
            assert_eq!(slots[1].bits, (expected / 2) as u64);
        }
    }
    drop(driver);
    assert_eq!(resources.usage(), baseline);
}

#[test]
fn corrupted_bound_caller_slots_and_status_are_refused_before_mapping() {
    let resources = Resources::new();
    let baseline = resources.usage();
    for fault in [Fault::Bound, Fault::Caller, Fault::Slots, Fault::Status] {
        assert!(matches!(
            resources.compile(limits(), 2 * 1024 * 1024, fault),
            Err(JitError::Compilation(message)) if message == "native region source mismatch"
        ));
        assert_eq!(resources.usage(), baseline);
    }
}

#[test]
fn refused_ir_relocations_mapping_and_allocations_release_all_charges() {
    for case in 0..6 {
        let resources = Resources::new();
        let baseline = resources.usage();
        let mut limits = limits();
        let mut bytes = 2 * 1024 * 1024;
        match case {
            0 => limits.instructions = 63,
            1 => limits.blocks = 3,
            2 => limits.relocations = 0,
            3 => bytes = 0,
            4 => resources.metadata.0.fail_after(0),
            5 => resources.workspace.0.fail_after(0),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                resources.compile(limits, bytes, Fault::None),
                Err(JitError::ResourceLimit(_))
            ),
            "case {case}"
        );
        assert_eq!(resources.usage(), baseline, "case {case}");
    }
}
