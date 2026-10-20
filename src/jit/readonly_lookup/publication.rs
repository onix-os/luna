use super::Plan;
use crate::jit::{helpers::Counts, JitStats};

impl Plan {
    #[inline(always)]
    pub(in crate::jit) fn publish(self, stats: &mut JitStats, counts: Counts) {
        debug_assert_eq!(
            (
                counts.table_writes,
                counts.upvalue_writes,
                counts.allocations
            ),
            (0, 0, 0)
        );
        debug_assert!(counts.calls != 0 || (counts.completed == 0 && counts.declined == 0));
        if counts.calls != 0 {
            stats.helper_calls = stats.helper_calls.saturating_add(counts.calls);
            stats.helper_instructions = stats.helper_instructions.saturating_add(counts.completed);
            stats.helper_declines = stats.helper_declines.saturating_add(counts.declined);
        }
        stats.native_table_reads = stats.native_table_reads.saturating_add(counts.table_reads);
        stats.native_upvalue_reads = stats
            .native_upvalue_reads
            .saturating_add(counts.upvalue_reads);
    }
}

#[cfg(test)]
mod tests;
