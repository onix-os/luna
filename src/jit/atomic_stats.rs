use super::JitStats;

impl JitStats {
    pub(super) fn record_atomic_call(&mut self) {
        self.total_dispatches = self.total_dispatches.saturating_add(5);
        self.interpreted_slices = self.interpreted_slices.saturating_add(2);
        self.native_entries = self.native_entries.saturating_add(1);
        self.native_instructions = self.native_instructions.saturating_add(3);
        self.native_interpreter_exits = self.native_interpreter_exits.saturating_add(1);
        self.native_upvalue_reads = self.native_upvalue_reads.saturating_add(1);
        self.native_upvalue_writes = self.native_upvalue_writes.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::{abi::Exit, exits::Kind, Runtime};

    fn compare(initial: JitStats) {
        let runtime = Runtime::new();
        runtime.0.borrow_mut().stats = initial;
        for _ in 0..2 {
            let mut slice = runtime.interpreter_stats();
            slice.dispatches = 1;
            slice.reported_instructions = Some(0);
        }
        let mut manager = runtime.0.borrow_mut();
        manager.stats.native_upvalue_reads = manager.stats.native_upvalue_reads.saturating_add(1);
        manager.stats.native_upvalue_writes = manager.stats.native_upvalue_writes.saturating_add(1);
        manager.stats.record_native_exit(&Exit {
            pc: 3,
            instructions: 3,
            reason: Kind::Interpreter as u32,
        });
        let mut actual = initial;
        actual.record_atomic_call();
        assert_eq!(actual, manager.stats);
    }

    #[test]
    fn atomic_call_counts_match_original_publication_at_saturation() {
        let fields: [fn(&mut JitStats) -> &mut u64; 7] = [
            |s| &mut s.total_dispatches,
            |s| &mut s.interpreted_slices,
            |s| &mut s.native_entries,
            |s| &mut s.native_instructions,
            |s| &mut s.native_interpreter_exits,
            |s| &mut s.native_upvalue_reads,
            |s| &mut s.native_upvalue_writes,
        ];
        let seed = JitStats {
            total_dispatches: 11,
            interpreted_slices: 13,
            native_entries: 17,
            native_instructions: 19,
            native_interpreter_exits: 23,
            native_upvalue_reads: 29,
            native_upvalue_writes: 31,
            interpreted_instructions: 37,
            helper_calls: 41,
            metadata_peak_bytes: 43,
            native_pair_calls: 47,
            native_pair_returns: 53,
            guard_exits: 59,
            ..Default::default()
        };
        for value in [
            0,
            1,
            u64::MAX - 5,
            u64::MAX - 3,
            u64::MAX - 2,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let mut all = seed;
            for field in fields {
                let mut individual = seed;
                *field(&mut individual) = value;
                compare(individual);
                *field(&mut all) = value;
            }
            compare(all);
        }
        let mut random = 0x9312_7382_2129_3731u64;
        for _ in 0..128 {
            let mut initial = seed;
            for field in fields {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                *field(&mut initial) = random;
            }
            compare(initial);
        }
    }
}
