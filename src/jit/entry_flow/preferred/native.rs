use super::*;
use crate::jit::{
    abi::{self, Slot},
    backend::{self, Code, Failure},
    exits::Kind,
    resources::{BudgetAllocator, Ledger, MappingCounter},
    work, JitConfig,
};

mod frames;

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
fn required_route_refuses_ineligible_sources_without_retained_resources() {
    let mut source = super::tests::source();
    source.operations[2] = source.operations[5];
    let metadata = Ledger::new(2 << 20);
    let mapped = Ledger::new(usize::MAX);
    let result = backend::compile_in(
        &source,
        MappingCounter::new(mapped.clone()),
        8 << 20,
        BudgetAllocator(metadata.clone()),
        work::Limits::from(&JitConfig::default()),
        Failure::RequirePreferredEntry,
    );
    assert!(matches!(result, Err(JitError::Compilation(ref message))
        if message == "preferred entry unavailable"));
    drop(result);
    assert_eq!(metadata.current(), 0);
    assert_eq!(mapped.current(), 0);
}

#[test]
fn native_route_matches_all_entries_budgets_and_scalar_tags() {
    let source = super::tests::source();
    source.verify().unwrap();
    let ordinary = compile(&source, Failure::None);
    let preferred = compile(&source, Failure::RequirePreferredEntry);
    assert_eq!(ordinary.entries, preferred.entries);
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
        Slot {
            tag: abi::INTEGER,
            bits: i64::MIN as u64,
        },
        Slot {
            tag: abi::INTEGER,
            bits: i64::MAX as u64,
        },
        Slot {
            tag: abi::INTEGER,
            bits: 0,
        },
        Slot {
            tag: abi::INTEGER,
            bits: 1,
        },
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
    let integer = |value: i64| Slot {
        tag: abi::INTEGER,
        bits: value as u64,
    };
    let mut executions = 0;
    for first in values {
        for second in values {
            for lane in 0..4 {
                let mut before = [integer(0); 7];
                before[1] = integer(3);
                before[2] = integer(1);
                let (a, b) = [(0, 1), (1, 2), (3, 5), (4, 6)][lane];
                before[a] = first;
                before[b] = second;
                for pc in (0..=source.operations.len()).chain([
                    u32::MAX as usize,
                    (1u64 << 32) as usize,
                    ((1u64 << 32) | 3) as usize,
                    usize::MAX,
                ]) {
                    for budget in [0, 1, 2, 3, 63, 64, u32::MAX] {
                        let mut expected = before;
                        let mut actual = before;
                        let left = ordinary.invoke(&mut expected, pc, budget);
                        let right = preferred.invoke(&mut actual, pc, budget);
                        assert_eq!(
                            (left.pc, left.instructions, left.reason),
                            (right.pc, right.instructions, right.reason),
                            "pc={pc} budget={budget} lane={lane} first={first:?} second={second:?}",
                        );
                        assert_eq!(
                            expected.map(|s| (s.tag, s.bits)),
                            actual.map(|s| (s.tag, s.bits))
                        );
                        if pc >= source.operations.len() || budget == 0 || pc == 2 || pc == 5 {
                            let reason = if pc < source.operations.len() && budget == 0 {
                                Kind::Budget
                            } else {
                                Kind::Interpreter
                            };
                            assert_eq!(
                                (right.pc, right.instructions, right.reason),
                                (pc as u64, 0, reason as u32)
                            );
                            assert_eq!(
                                actual.map(|s| (s.tag, s.bits)),
                                before.map(|s| (s.tag, s.bits))
                            );
                        }
                        executions += 2;
                    }
                }
            }
        }
    }
    assert_eq!(executions, 104_104);
}
