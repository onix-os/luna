use super::*;

fn check(initial: [u64; 8], delta: [u64; 8]) {
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
    let counts = helpers::Counts {
        calls: delta[0],
        completed: delta[1],
        declined: delta[2],
        table_reads: delta[3],
        table_writes: delta[4],
        upvalue_reads: delta[5],
        upvalue_writes: delta[6],
        allocations: delta[7],
    };
    record_helper_counts!(actual, counts);
    assert_eq!(actual, expected, "initial={initial:?} delta={delta:?}");
}

#[test]
fn independent_full_width_fields_match_wide_sum_oracle() {
    let values = [0, 1, 63, 64, 255, 256, 1 << 32, u64::MAX - 1, u64::MAX];
    for lane in 0..8 {
        for initial in values {
            for delta in values {
                let mut before = [17; 8];
                before[lane] = initial;
                let mut counts = [0; 8];
                counts[lane] = delta;
                check(before, counts);
            }
        }
    }
    let mut seed = 0x139a_42e1_7335_9a91u64;
    for _ in 0..4096 {
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        check(
            std::array::from_fn(|_| next()),
            std::array::from_fn(|_| next()),
        );
    }
}

#[test]
fn readonly_and_each_write_failure_lane_preserve_saturation() {
    for initial in [0, 17, u64::MAX - 1, u64::MAX] {
        check([initial; 8], [1, 1, 0, 1, 0, 1, 0, 0]);
        check([initial; 8], [0; 8]);
        for lane in [2, 4, 6, 7] {
            let mut delta = [0; 8];
            delta[lane] = u64::MAX;
            check([initial; 8], delta);
        }
    }
}
