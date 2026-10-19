use super::*;
use abi::payload::packed::scoped::bridge::entry::{Failure, Fault};

impl Runtime {
    pub(super) fn packed_failure(
        &self,
        frame: &mut helpers::Frame<'_, '_, '_, '_>,
        fault: Fault,
        prefix: Option<&array_window::Outcome>,
    ) -> ! {
        if frame.panic.is_none() {
            if let Some(progress) = fault.progress {
                *frame.registers.pc =
                    usize::try_from(progress.pc).expect("native PC exceeds host range");
            }
        }
        let instructions = u64::from(fault.progress.map_or(0, |p| p.instructions))
            + u64::from(prefix.map_or(0, |p| p.exit.instructions));
        let entered = prefix.is_some() || !matches!(fault.cause, Failure::Admission(_));
        let mut manager = self.0.borrow_mut();
        helper_counts::publish(&mut manager.stats, std::mem::take(&mut frame.count), false);
        manager.stats.total_dispatches =
            manager.stats.total_dispatches.saturating_add(instructions);
        manager.stats.native_instructions = manager
            .stats
            .native_instructions
            .saturating_add(instructions);
        manager.stats.native_entries = manager
            .stats
            .native_entries
            .saturating_add(u64::from(entered));
        if let Some(prefix) = prefix {
            manager.array_native_counts.0 = manager
                .array_native_counts
                .0
                .saturating_add(u64::from(prefix.counts.reads));
            manager.array_native_counts.1 = manager
                .array_native_counts
                .1
                .saturating_add(u64::from(prefix.counts.writes));
        }
        if let Some(payload) = frame.panic.take() {
            manager.stats.native_panic_exits = manager
                .stats
                .native_panic_exits
                .saturating_add(u64::from(entered));
            drop(manager);
            std::panic::resume_unwind(payload);
        }
        drop(manager);
        std::panic::panic_any(fault);
    }
}

#[test]
fn packed_failure_publishes_only_known_work_and_preserves_original_panic() {
    use abi::payload::packed::scoped::bridge::entry::Progress;
    use abi::payload::packed::scoped::Refusal;
    for original_panic in [false, true] {
        for progress in [
            None,
            Some(Progress {
                pc: 7,
                instructions: 3,
            }),
        ] {
            for admission in [false, true] {
                if admission && progress.is_some() {
                    continue;
                }
                for array in [false, true] {
                    for baseline in [0, u64::MAX - 1] {
                        crate::Lua::empty().enter(|ctx| {
                            let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
                            let mut values = [crate::Value::Integer(731)];
                            let mut pc = 9;
                            crate::thread::LuaRegisters::with_test_frame(
                                ctx,
                                &mut pc,
                                &mut values,
                                |mut registers| {
                                    let runtime = Runtime::new();
                                    runtime.0.borrow_mut().stats = JitStats {
                                        total_dispatches: baseline,
                                        native_instructions: baseline,
                                        native_entries: baseline,
                                        helper_calls: baseline,
                                        helper_instructions: baseline,
                                        helper_declines: baseline,
                                        native_table_reads: baseline,
                                        native_table_writes: baseline,
                                        native_upvalue_reads: baseline,
                                        native_upvalue_writes: baseline,
                                        native_allocations: baseline,
                                        ..Default::default()
                                    };
                                    let mut frame = helpers::Frame {
                                        ctx,
                                        closure,
                                        registers: &mut registers,
                                        count: helpers::Counts {
                                            calls: 9,
                                            completed: 8,
                                            declined: 1,
                                            table_reads: 2,
                                            table_writes: 3,
                                            upvalue_reads: 4,
                                            upvalue_writes: 5,
                                            allocations: 6,
                                        },
                                        slot_count: 1,
                                        panic: original_panic.then(|| {
                                            Box::new(731u64) as Box<dyn std::any::Any + Send>
                                        }),
                                        projection: None,
                                    };
                                    let fault = Fault {
                                        cause: if admission {
                                            Failure::Admission(Refusal::Bounds)
                                        } else {
                                            Failure::WriterDeclined
                                        },
                                        progress,
                                    };
                                    let prefix = array.then_some(array_window::Outcome {
                                        exit: abi::Exit {
                                            pc: 9,
                                            instructions: 11,
                                            reason: exits::Kind::Budget as u32,
                                        },
                                        counts: array_window::Counts {
                                            reads: 2,
                                            writes: 3,
                                        },
                                    });
                                    let result = std::panic::catch_unwind(
                                        std::panic::AssertUnwindSafe(|| {
                                            runtime.packed_failure(
                                                &mut frame,
                                                fault,
                                                prefix.as_ref(),
                                            );
                                        }),
                                    )
                                    .unwrap_err();
                                    if original_panic {
                                        assert_eq!(*result.downcast::<u64>().unwrap(), 731);
                                    } else {
                                        assert_eq!(*result.downcast::<Fault>().unwrap(), fault);
                                    }
                                    assert_eq!(
                                        *frame.registers.pc,
                                        if original_panic {
                                            9
                                        } else {
                                            progress.map_or(9, |p| p.pc as usize)
                                        }
                                    );
                                    assert!(matches!(
                                        frame.registers.stack_frame[0],
                                        crate::Value::Integer(731)
                                    ));
                                    assert!(frame.panic.is_none());
                                    assert_eq!(frame.count.calls, 0);
                                    let manager = runtime.0.borrow();
                                    let stats = manager.stats;
                                    let work = u64::from(progress.map_or(0, |p| p.instructions))
                                        + if array { 11 } else { 0 };
                                    assert_eq!(
                                        stats.total_dispatches,
                                        baseline.saturating_add(work)
                                    );
                                    assert_eq!(
                                        stats.native_instructions,
                                        baseline.saturating_add(work)
                                    );
                                    assert_eq!(
                                        stats.native_entries,
                                        baseline.saturating_add(u64::from(array || !admission))
                                    );
                                    assert_eq!(
                                        stats.native_panic_exits,
                                        u64::from(original_panic && (array || !admission))
                                    );
                                    assert_eq!(
                                        (
                                            stats.guard_exits,
                                            stats.native_interpreter_exits,
                                            stats.native_budget_exits
                                        ),
                                        (0, 0, 0)
                                    );
                                    assert_eq!(
                                        (
                                            stats.interpreted_slices,
                                            stats.interpreted_instructions,
                                            stats.hook_exits
                                        ),
                                        (0, 0, 0)
                                    );
                                    for (actual, delta) in [
                                        (stats.helper_calls, 9),
                                        (stats.helper_instructions, 8),
                                        (stats.helper_declines, 1),
                                        (stats.native_table_reads, 2),
                                        (stats.native_table_writes, 3),
                                        (stats.native_upvalue_reads, 4),
                                        (stats.native_upvalue_writes, 5),
                                        (stats.native_allocations, 6),
                                    ] {
                                        assert_eq!(actual, baseline.saturating_add(delta));
                                    }
                                    assert_eq!(
                                        manager.array_native_counts,
                                        if array { (2, 3) } else { (0, 0) }
                                    );
                                },
                            );
                        });
                    }
                }
            }
        }
    }
}
