use super::*;
use crate::{Closure, Lua};

fn check(plan: Plan, initial: [u64; 8], delta: [u64; 8]) {
    let mut actual = JitStats {
        helper_calls: initial[0],
        helper_instructions: initial[1],
        helper_declines: initial[2],
        native_table_reads: initial[3],
        native_table_writes: initial[4],
        native_upvalue_reads: initial[5],
        native_upvalue_writes: initial[6],
        native_allocations: initial[7],
        total_dispatches: 91,
        native_entries: 37,
        native_panic_exits: 19,
        metadata_bytes: 512,
        ..Default::default()
    };
    let mut expected = actual;
    let next = std::array::from_fn::<_, 8, _>(|i| {
        (u128::from(initial[i]) + u128::from(delta[i])).min(u128::from(u64::MAX)) as u64
    });
    expected.helper_calls = next[0];
    expected.helper_instructions = next[1];
    expected.helper_declines = next[2];
    expected.native_table_reads = next[3];
    expected.native_table_writes = next[4];
    expected.native_upvalue_reads = next[5];
    expected.native_upvalue_writes = next[6];
    expected.native_allocations = next[7];
    plan.publish(
        &mut actual,
        Counts {
            calls: delta[0],
            completed: delta[1],
            declined: delta[2],
            table_reads: delta[3],
            table_writes: delta[4],
            upvalue_reads: delta[5],
            upvalue_writes: delta[6],
            allocations: delta[7],
        },
    );
    assert_eq!(actual, expected, "initial={initial:?} delta={delta:?}");
}

fn with_plan(run: impl FnOnce(Plan)) {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum",
        )
        .unwrap();
        let source = crate::jit::ir::Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
        run(Plan::recognize(&source).unwrap());
    });
}

#[test]
fn full_width_readonly_fields_match_independent_wide_sum_oracle() {
    with_plan(|plan| {
        let values = [0, 1, 63, 64, 255, 256, 1 << 32, u64::MAX - 1, u64::MAX];
        let mut cases = 0;
        for lane in [0, 1, 2, 3, 5] {
            for initial in values {
                for delta in values {
                    let mut before = [17; 8];
                    before[lane] = initial;
                    let mut change = [0; 8];
                    change[lane] = delta;
                    if (lane == 1 || lane == 2) && delta != 0 {
                        change[0] = u64::MAX;
                    }
                    check(plan, before, change);
                    cases += 1;
                }
            }
        }
        let mut seed = 0xc56f_5b23_e149_8831u64;
        for n in 0..4096 {
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let before = std::array::from_fn(|_| next());
            let calls = if n % 2 == 0 { 0 } else { next() | 1 };
            let completed = if calls == 0 { 0 } else { next() % calls };
            let declined = if calls == 0 {
                0
            } else {
                next() % (calls - completed)
            };
            check(
                plan,
                before,
                [calls, completed, declined, next(), 0, next(), 0, 0],
            );
            cases += 1;
        }
        assert_eq!(cases, 4501);
    });
}

#[test]
fn zero_calls_preserve_other_sources_writes_and_allocation_saturation() {
    with_plan(|plan| {
        for initial in [0, 17, u64::MAX - 1, u64::MAX] {
            for delta in [
                [0; 8],
                [0, 0, 0, 1, 0, 1, 0, 0],
                [1, 0, 1, 0, 0, 1, 0, 0],
                [1, 0, 0, 0, 0, 1, 0, 0],
                [64, 64, 0, 1, 0, 1, 0, 0],
            ] {
                check(plan, [initial; 8], delta);
            }
        }
    });
}

#[cfg(debug_assertions)]
#[test]
fn impossible_effects_are_rejected_before_any_publication() {
    with_plan(|plan| {
        for lane in [1, 2, 4, 6, 7] {
            let mut delta = [0; 8];
            delta[lane] = 1;
            let mut stats = JitStats {
                native_table_writes: u64::MAX,
                native_allocations: 17,
                ..Default::default()
            };
            let before = stats;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                plan.publish(
                    &mut stats,
                    Counts {
                        calls: 0,
                        completed: delta[1],
                        declined: delta[2],
                        table_reads: 0,
                        table_writes: delta[4],
                        upvalue_reads: 0,
                        upvalue_writes: delta[6],
                        allocations: delta[7],
                    },
                )
            }));
            assert!(result.is_err());
            assert_eq!(stats, before);
        }
    });
}
