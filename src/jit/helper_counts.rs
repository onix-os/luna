use super::{helpers::Counts, JitStats};

pub(super) fn publish(stats: &mut JitStats, counts: Counts, read_only: bool) {
    stats.helper_calls = stats.helper_calls.saturating_add(counts.calls);
    stats.helper_instructions = stats.helper_instructions.saturating_add(counts.completed);
    stats.helper_declines = stats.helper_declines.saturating_add(counts.declined);
    stats.native_table_reads = stats.native_table_reads.saturating_add(counts.table_reads);
    stats.native_upvalue_reads = stats
        .native_upvalue_reads
        .saturating_add(counts.upvalue_reads);
    if !read_only {
        stats.native_table_writes = stats
            .native_table_writes
            .saturating_add(counts.table_writes);
        stats.native_upvalue_writes = stats
            .native_upvalue_writes
            .saturating_add(counts.upvalue_writes);
        stats.native_allocations = stats.native_allocations.saturating_add(counts.allocations);
    } else {
        debug_assert_eq!(
            (
                counts.table_writes,
                counts.upvalue_writes,
                counts.allocations
            ),
            (0, 0, 0)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_preserves_whole_statistics_at_saturation() {
        let fields: [fn(&mut JitStats) -> &mut u64; 8] = [
            |s| &mut s.helper_calls,
            |s| &mut s.helper_instructions,
            |s| &mut s.helper_declines,
            |s| &mut s.native_table_reads,
            |s| &mut s.native_table_writes,
            |s| &mut s.native_upvalue_reads,
            |s| &mut s.native_upvalue_writes,
            |s| &mut s.native_allocations,
        ];
        for read_only in [false, true] {
            for mask in 0u16..256 {
                for delta in [0, 1, 64, u64::MAX] {
                    let mut actual = JitStats {
                        native_entries: 13,
                        interpreted_instructions: 17,
                        metadata_bytes: 23,
                        metadata_peak_bytes: 29,
                        guard_exits: 31,
                        ..Default::default()
                    };
                    let mut increments = [0; 8];
                    for (index, field) in fields.iter().enumerate() {
                        *field(&mut actual) = if mask & (1 << index) != 0 {
                            u64::MAX - 1
                        } else {
                            index as u64
                        };
                        if !read_only || !matches!(index, 4 | 6 | 7) {
                            increments[index] = delta;
                        }
                    }
                    let mut expected = actual;
                    for (field, increment) in fields.into_iter().zip(increments) {
                        let value = field(&mut expected);
                        *value = value.saturating_add(increment);
                    }
                    publish(
                        &mut actual,
                        Counts {
                            calls: increments[0],
                            completed: increments[1],
                            declined: increments[2],
                            table_reads: increments[3],
                            table_writes: increments[4],
                            upvalue_reads: increments[5],
                            upvalue_writes: increments[6],
                            allocations: increments[7],
                        },
                        read_only,
                    );
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}
