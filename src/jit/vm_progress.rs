use super::Runtime;

enum Phase {
    InFlight(u32),
    Reported(u32),
}

pub(crate) struct VmProgress<'a> {
    runtime: &'a Runtime,
    pub dispatches: u32,
    phase: Phase,
}

impl Runtime {
    pub(crate) fn vm_progress(&self, native_prefix: u32) -> VmProgress<'_> {
        VmProgress {
            runtime: self,
            dispatches: native_prefix,
            phase: Phase::InFlight(native_prefix),
        }
    }
}

impl VmProgress<'_> {
    #[inline(always)]
    pub(crate) fn add_native(&mut self, instructions: u32) {
        let Phase::InFlight(native) = &mut self.phase else {
            panic!("VM progress already reported");
        };
        let next_native = *native + instructions;
        let next_dispatches = self.dispatches + instructions;
        *native = next_native;
        self.dispatches = next_dispatches;
    }

    #[inline(always)]
    pub(crate) fn finish(&mut self, unreported: u32) -> u32 {
        let Phase::InFlight(native) = self.phase else {
            panic!("VM progress already reported");
        };
        let completed = self.dispatches - unreported;
        let dispatches = self.dispatches - native;
        let report = dispatches - unreported;
        self.dispatches = dispatches;
        self.phase = Phase::Reported(report);
        completed
    }
}

impl Drop for VmProgress<'_> {
    fn drop(&mut self) {
        let (dispatches, report) = match self.phase {
            Phase::InFlight(native) => (self.dispatches - native, None),
            Phase::Reported(instructions) => (self.dispatches, Some(instructions)),
        };
        let mut manager = self.runtime.0.borrow_mut();
        let stats = &mut manager.stats;
        stats.total_dispatches = stats.total_dispatches.saturating_add(u64::from(dispatches));
        if let Some(instructions) = report {
            stats.interpreted_slices = stats
                .interpreted_slices
                .saturating_add(u64::from(dispatches != 0));
            stats.interpreted_instructions = stats
                .interpreted_instructions
                .saturating_add(u64::from(instructions));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::{InterpreterStats, JitStats};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn phase_storage_preserves_original_guard_size() {
        struct HistoricalWrapper<'a> {
            counts: InterpreterStats<'a>,
            native: u32,
        }
        assert_eq!(
            std::mem::size_of::<Phase>(),
            std::mem::size_of::<Option<u32>>()
        );
        assert_eq!(
            std::mem::size_of::<VmProgress<'_>>(),
            std::mem::size_of::<InterpreterStats<'_>>()
        );
        assert!(
            std::mem::size_of::<HistoricalWrapper<'_>>() > std::mem::size_of::<VmProgress<'_>>()
        );
        let runtime = Runtime::new();
        let old = HistoricalWrapper {
            counts: runtime.interpreter_stats(),
            native: 0,
        };
        assert_eq!((old.counts.dispatches, old.native), (0, 0));
    }

    #[test]
    fn phase_progress_matches_original_publication_at_saturation() {
        for initial in [0, u64::MAX - 1, u64::MAX] {
            for prefix in [0, 1, 63] {
                for native in [0, 2, 64] {
                    for interpreted in [0u32, 1, 2, 64] {
                        for reported in [false, true] {
                            for terminal in 0..=u32::from(interpreted != 0) {
                                let seed = JitStats {
                                    total_dispatches: initial,
                                    interpreted_slices: initial,
                                    interpreted_instructions: initial,
                                    native_entries: 17,
                                    native_instructions: 19,
                                    guard_exits: 23,
                                    ..Default::default()
                                };
                                let reference = Runtime::new();
                                reference.0.borrow_mut().stats = seed;
                                {
                                    let mut original = reference.interpreter_stats();
                                    original.dispatches = interpreted;
                                    original.reported_instructions =
                                        reported.then_some(interpreted - terminal);
                                }
                                let actual = Runtime::new();
                                actual.0.borrow_mut().stats = seed;
                                {
                                    let mut progress = actual.vm_progress(prefix);
                                    progress.dispatches += interpreted;
                                    progress.add_native(native);
                                    assert_eq!(progress.dispatches, prefix + native + interpreted);
                                    assert_eq!(actual.0.borrow().stats, seed);
                                    if reported {
                                        assert_eq!(
                                            progress.finish(terminal),
                                            prefix + native + interpreted - terminal
                                        );
                                        assert_eq!(progress.dispatches, interpreted);
                                        assert_eq!(actual.0.borrow().stats, seed);
                                    }
                                }
                                assert_eq!(actual.0.borrow().stats, reference.0.borrow().stats);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn phase_progress_preserves_rollback_unwinding_and_borrow_refusal() {
        for prefix in [0, 1, 63] {
            for interpreted in [0, 1, 64] {
                let runtime = Runtime::new();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut progress = runtime.vm_progress(prefix);
                    progress.dispatches += interpreted + 1;
                    progress.dispatches -= 1;
                    progress.add_native(3);
                    std::panic::panic_any(83u64);
                }));
                assert_eq!(*result.unwrap_err().downcast::<u64>().unwrap(), 83);
                assert_eq!(
                    runtime.0.borrow().stats,
                    JitStats {
                        total_dispatches: u64::from(interpreted),
                        ..Default::default()
                    }
                );
            }
        }
        let runtime = Runtime::new();
        let mut progress = runtime.vm_progress(3);
        progress.dispatches += 2;
        progress.finish(1);
        let manager = runtime.0.borrow();
        let before = manager.stats;
        assert!(catch_unwind(AssertUnwindSafe(|| drop(progress))).is_err());
        assert_eq!(manager.stats, before);
    }

    #[test]
    fn phase_progress_preserves_full_width_native_and_interpreted_counts() {
        for native in [0, 1, u32::MAX - 1, u32::MAX] {
            let runtime = Runtime::new();
            {
                let mut progress = runtime.vm_progress(native);
                let interpreted = u32::MAX - native;
                progress.dispatches += interpreted;
                assert_eq!(
                    progress.finish(u32::from(interpreted != 0)),
                    u32::MAX - u32::from(interpreted != 0)
                );
            }
            let interpreted = u32::MAX - native;
            let stats = runtime.0.borrow().stats;
            assert_eq!(stats.total_dispatches, u64::from(interpreted));
            assert_eq!(stats.interpreted_slices, u64::from(interpreted != 0));
            assert_eq!(
                stats.interpreted_instructions,
                u64::from(interpreted - u32::from(interpreted != 0))
            );
        }
    }
}
