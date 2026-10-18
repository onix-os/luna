use crate::{
    jit::{
        abi::{self, Slot},
        backend::{self, Code, Failure},
        exits::Kind,
        ir::Snapshot,
        resources::{self, BudgetAllocator, Ledger, MappingCounter},
        work, JitConfig,
    },
    opcode::{Operation, RCIndex},
    types::{RegisterIndex as R, VarCount},
};

fn source() -> Snapshot {
    Snapshot {
        prepared_keys: None,
        operations: resources::owned(&[
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
        constants: resources::owned(&[]),
        registers: 7,
        upvalues: 0,
        prototypes: 0,
    }
}

fn compile(snapshot: &Snapshot, failure: Failure) -> Code {
    backend::compile_in(
        snapshot,
        MappingCounter::new(Ledger::new(usize::MAX)),
        8 << 20,
        BudgetAllocator(Ledger::new(2 << 20)),
        work::Limits::from(&JitConfig::default()),
        failure,
    )
    .unwrap()
}

#[test]
fn native_bound_entries_match_ordinary_execution_and_refuse_other_pcs() {
    let snapshot = source();
    snapshot.verify().unwrap();
    let ordinary = compile(&snapshot, Failure::None);
    let integer = |value: i64| Slot {
        tag: abi::INTEGER,
        bits: value as u64,
    };
    let values = [
        Slot {
            tag: abi::NIL,
            bits: 0,
        },
        Slot {
            tag: abi::BOOLEAN,
            bits: 0,
        },
        Slot {
            tag: abi::BOOLEAN,
            bits: 1,
        },
        integer(i64::MIN),
        integer(i64::MAX),
        integer(0),
        integer(1),
        Slot {
            tag: abi::NUMBER,
            bits: (-0.0f64).to_bits(),
        },
        Slot {
            tag: abi::NUMBER,
            bits: 1.5f64.to_bits(),
        },
        Slot {
            tag: abi::NUMBER,
            bits: f64::INFINITY.to_bits(),
        },
        Slot {
            tag: abi::NUMBER,
            bits: f64::NEG_INFINITY.to_bits(),
        },
        Slot {
            tag: abi::NUMBER,
            bits: 0x7ff8_0000_0000_4321,
        },
        Slot::canonical(),
    ];
    let mut matched = 0;
    let mut refused = 0;
    for bound in 0..snapshot.operations.len() {
        let specialized = compile(&snapshot, Failure::RequireBoundEntry(bound as u32));
        for (pc, eligible) in specialized.entries.iter().enumerate() {
            assert_eq!(*eligible, ordinary.entries[pc] && pc == bound);
        }
        eprintln!(
            "bound={bound} ordinary_bytes={} specialized_bytes={}",
            ordinary.byte_len, specialized.byte_len
        );
        for first in values {
            for second in values {
                for (a, b) in [(0, 1), (1, 2), (3, 5), (4, 6)] {
                    let mut before = [integer(0); 7];
                    before[1] = integer(3);
                    before[2] = integer(1);
                    before[a] = first;
                    before[b] = second;
                    for pc in (0..=snapshot.operations.len()).chain([
                        u32::MAX as usize,
                        (1u64 << 32) as usize,
                        ((1u64 << 32) | bound as u64) as usize,
                        usize::MAX,
                    ]) {
                        for budget in [0, 1, 2, 3, 63, 64, u32::MAX] {
                            let mut actual = before;
                            let exit = specialized.invoke(&mut actual, pc, budget);
                            if pc == bound {
                                let mut expected = before;
                                let control = ordinary.invoke(&mut expected, pc, budget);
                                assert_eq!(
                                    (exit.pc, exit.instructions, exit.reason),
                                    (control.pc, control.instructions, control.reason)
                                );
                                assert_eq!(
                                    actual.map(|s| (s.tag, s.bits)),
                                    expected.map(|s| (s.tag, s.bits))
                                );
                                matched += 1;
                            } else {
                                assert_eq!(
                                    (exit.pc, exit.instructions, exit.reason),
                                    (pc as u64, 0, Kind::Interpreter as u32)
                                );
                                assert_eq!(
                                    actual.map(|s| (s.tag, s.bits)),
                                    before.map(|s| (s.tag, s.bits))
                                );
                                refused += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(matched, 28_392);
    assert_eq!(refused, 283_920);
}

#[test]
fn invalid_entries_and_ir_limits_release_resources() {
    use std::sync::atomic::Ordering;
    let snapshot = source();
    let admitted =
        work::Expansion::admit(&snapshot, work::Limits::from(&JitConfig::default())).unwrap();
    let source_ledger = &snapshot.operations.allocator().0;
    let baseline = source_ledger.current();
    for fault in 0..5 {
        let metadata = Ledger::new(2 << 20);
        let memory = MappingCounter::new(Ledger::new(usize::MAX));
        let result = backend::compile_in(
            &snapshot,
            memory.clone(),
            8 << 20,
            BudgetAllocator(metadata.clone()),
            work::Limits {
                instructions: admitted.instructions - usize::from(fault == 0),
                blocks: admitted.blocks - usize::from(fault == 1),
                relocations: JitConfig::default().max_relocations,
            },
            Failure::RequireBoundEntry(match fault {
                2 => 6,
                3 => u32::MAX,
                _ => 3,
            }),
        );
        match fault {
            0 | 1 => assert!(matches!(result, Err(super::JitError::ResourceLimit(_)))),
            2 | 3 => assert!(matches!(result, Err(super::JitError::Compilation(_)))),
            _ => assert!(result.is_ok(), "{:?}", result.as_ref().err()),
        }
        drop(result);
        assert_eq!(metadata.current(), 0);
        assert_eq!(memory.load(Ordering::Relaxed), 0);
        assert_eq!(memory.requested(), 0);
        assert_eq!(source_ledger.current(), baseline);
    }
}

#[test]
fn prepared_helper_and_integer_loop_sources_keep_ordinary_lowering() {
    crate::Lua::empty().enter(|ctx| {
        for script in [
            "local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum",
            "local total=0 for i=1,3 do host_increment(i); total=target end return total",
            "local sum=0 for i=1,80 do sum=sum+i end return sum",
        ] {
            let closure = crate::Closure::load(ctx, None, script.as_bytes()).unwrap();
            let snapshot = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
            let ordinary = compile(&snapshot, Failure::None);
            let mut pcs = vec![0];
            pcs.extend(
                snapshot
                    .operations
                    .iter()
                    .enumerate()
                    .filter_map(|(pc, op)| matches!(op, Operation::Call { .. }).then_some(pc + 1)),
            );
            for bound in pcs {
                let specialized = compile(&snapshot, Failure::RequireBoundEntry(bound as u32));
                eprintln!(
                    "script={script:?} bound={bound} ordinary_bytes={} specialized_bytes={}",
                    ordinary.byte_len, specialized.byte_len
                );
                for budget in [0, 1, 2, 3, 63, 64, u32::MAX] {
                    let mut actual = vec![Slot::canonical(); snapshot.registers];
                    let mut expected = actual.clone();
                    let right = specialized.invoke(&mut actual, bound, budget);
                    let left = ordinary.invoke(&mut expected, bound, budget);
                    assert_eq!(
                        (right.pc, right.instructions, right.reason),
                        (left.pc, left.instructions, left.reason)
                    );
                    assert_eq!(
                        actual.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>(),
                        expected.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>()
                    );
                }
            }
        }
    });
}

mod frames;
mod profile;
