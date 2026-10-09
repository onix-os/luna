use crate::jit::{InterpreterStats, Runtime};

pub(super) struct Progress<'a> {
    pub counts: InterpreterStats<'a>,
    pub native: u32,
}

impl<'a> Progress<'a> {
    pub fn new(runtime: &'a Runtime, native: u32) -> Self {
        let mut counts = runtime.interpreter_stats();
        counts.dispatches = native;
        Self { counts, native }
    }
}

impl Drop for Progress<'_> {
    fn drop(&mut self) {
        self.counts.dispatches -= self.native;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::JitStats;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn combined_progress_publishes_only_interpreted_dispatches() {
        for initial in [0, u64::MAX - 1, u64::MAX] {
            for prefix in [0, 1, 63] {
                for native in [0, 2, 64] {
                    for interpreted in [0u32, 1, 2, 64] {
                        for report in [None, Some(interpreted), Some(interpreted.saturating_sub(1))]
                        {
                            let seed = JitStats {
                                total_dispatches: initial,
                                interpreted_instructions: initial,
                                interpreted_slices: initial,
                                native_instructions: 1234,
                                native_entries: 123,
                                guard_exits: 98,
                                ..Default::default()
                            };
                            let reference = Runtime::new();
                            reference.0.borrow_mut().stats = seed;
                            {
                                let mut counts = reference.interpreter_stats();
                                counts.dispatches = interpreted;
                                counts.reported_instructions = report;
                            }
                            let candidate = Runtime::new();
                            candidate.0.borrow_mut().stats = seed;
                            {
                                let mut progress = Progress::new(&candidate, prefix);
                                for _ in 0..interpreted {
                                    progress.counts.dispatches += 1;
                                }
                                progress.counts.dispatches += native;
                                progress.native += native;
                                assert_eq!(
                                    progress.counts.dispatches,
                                    prefix + native + interpreted
                                );
                                progress.counts.reported_instructions = report;
                            }
                            assert_eq!(candidate.0.borrow().stats, reference.0.borrow().stats);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn combined_progress_preserves_unwinding_and_provisional_rollback() {
        for prefix in [0, 1, 63] {
            for interpreted in [0, 1, 64] {
                let runtime = Runtime::new();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut progress = Progress::new(&runtime, prefix);
                    progress.counts.dispatches += interpreted + 1;
                    progress.counts.dispatches -= 1;
                    progress.counts.dispatches += 3;
                    progress.native += 3;
                    panic!("VM progress unwind sentinel");
                }));
                assert!(result.is_err());
                assert_eq!(
                    runtime.0.borrow().stats,
                    JitStats {
                        total_dispatches: u64::from(interpreted),
                        ..Default::default()
                    }
                );
            }
        }
    }
}
