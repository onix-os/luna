use super::*;

unsafe extern "C" fn echo(
    _: *mut Packed,
    pc: u64,
    budget: u32,
    exit: *mut abi::Exit,
    _: *const Bridge,
) -> u32 {
    unsafe {
        exit.write(abi::Exit {
            pc,
            instructions: budget,
            reason: crate::jit::exits::Kind::Budget as u32,
        });
    }
    RETURNED
}

unsafe extern "C" fn forbidden(
    _: *mut Packed,
    _: u64,
    _: u32,
    _: *mut abi::Exit,
    _: *const Bridge,
) -> u32 {
    std::process::abort()
}

unsafe extern "C" fn aborting(
    slots: *mut Packed,
    mode: u64,
    _: u32,
    exit: *mut abi::Exit,
    bridge: *const Bridge,
) -> u32 {
    unsafe {
        let bridge = &*bridge;
        assert!(slots.read().store_numeric(Slot {
            tag: INTEGER,
            bits: 731
        }));
        match mode {
            0 => (bridge.functions.write)(bridge.control, 0, BOOLEAN, 2),
            1 => boundary::<3>(bridge.control, |_| std::panic::panic_any(731u64)),
            2 | 3 => {
                (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 3>>())
                    .scope
                    .count = 3;
                let status = (bridge.functions.helpers[0])(bridge.control, 1, 0, 0, 7);
                if mode == 3 {
                    echo(slots, 8, 1, exit, bridge)
                } else {
                    status
                }
            }
            4 => abi::HELPER_PANICKED,
            5 => REFUSED,
            6 => abi::HELPER_COMPLETED,
            _ => 999,
        }
    }
}

fn with_frame(mut body: impl FnMut(&mut helpers::Frame<'_, '_, '_, '_>)) {
    with_frame_slots(2, &mut body);
}

fn with_frame_slots(count: usize, mut body: impl FnMut(&mut helpers::Frame<'_, '_, '_, '_>)) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let mut values = vec![Value::Nil; count];
        values[0] = Value::Integer(17);
        let mut pc = 5;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: count,
                panic: None,
                projection: None,
            };
            body(&mut frame);
        });
    });
}

pub(crate) unsafe extern "C" fn failed_progress(
    _: *mut Packed,
    mode: u64,
    _: u32,
    exit: *mut abi::Exit,
    bridge: *const Bridge,
) -> u32 {
    unsafe {
        let bridge = &*bridge;
        let status = match mode {
            0 => (bridge.functions.write)(bridge.control, 0, BOOLEAN, 2),
            1 => boundary::<3>(bridge.control, |bound| {
                bound.scope.replace(0, Value::Integer(731), false)?;
                std::panic::panic_any(731u64)
            }),
            2 => {
                (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 3>>())
                    .scope
                    .count = 3;
                (bridge.functions.helpers[0])(bridge.control, 1, 0, 0, 7)
            }
            3 => {
                assert_eq!(
                    (bridge.functions.helpers[0])(bridge.control, 1, 0, 0, 7),
                    abi::HELPER_COMPLETED
                );
                (bridge.functions.write)(bridge.control, 0, BOOLEAN, 2)
            }
            _ => boundary::<3>(bridge.control, |bound| {
                assert_eq!(bound.helper::<1>(1, 0, 0, 7)?, abi::HELPER_COMPLETED);
                std::panic::panic_any(731u64)
            }),
        };
        exit.write(abi::Exit {
            pc: 7,
            instructions: 3,
            reason: match mode {
                2 => HELPER_FAILURE,
                4 => crate::jit::exits::Kind::Panic as u32,
                _ => STORE_FAILURE,
            },
        });
        if mode == 4 {
            RETURNED
        } else {
            status
        }
    }
}

#[test]
fn packed_failure_reports_preserve_prefixes_and_post_helper_completion() {
    for mode in 0..5 {
        for baseline in [0, 1_000_000] {
            with_frame(|frame| {
                frame.count.calls = baseline;
                frame.count.completed = baseline / 2;
                let result = unsafe { invoke_report::<3>(frame, failed_progress, 2, mode, 64) };
                if mode == 4 {
                    let exit = result.unwrap();
                    assert_eq!(
                        (exit.pc, exit.instructions, exit.reason),
                        (8, 4, crate::jit::exits::Kind::Panic as u32)
                    );
                } else {
                    let fault = result.unwrap_err();
                    assert_eq!(
                        fault.cause,
                        match mode {
                            0 | 3 => Failure::WriterDeclined,
                            1 => Failure::Panicked,
                            _ => Failure::Refused(Refusal::Bounds),
                        }
                    );
                    assert_eq!(
                        fault.progress,
                        Some(Progress {
                            pc: if mode == 2 { 8 } else { 7 },
                            instructions: if mode == 2 { 4 } else { 3 }
                        })
                    );
                }
                let called = mode >= 2;
                assert_eq!(frame.count.calls, baseline + u64::from(called));
                assert_eq!(frame.count.completed, baseline / 2 + u64::from(called));
                assert_eq!(*frame.registers.pc, if called { 8 } else { 5 });
                assert_eq!(frame.panic.is_some(), matches!(mode, 1 | 4));
                if let Some(payload) = frame.panic.take() {
                    assert_eq!(*payload.downcast::<u64>().unwrap(), 731);
                }
                assert!(
                    matches!(frame.registers.stack_frame[0], Value::Integer(v) if v == if mode == 1 { 731 } else { 17 })
                );
                if called {
                    assert!(matches!(frame.registers.stack_frame[1], Value::Integer(17)));
                }
            });
        }
    }
}

#[test]
fn packed_checkpoint_progress_accepts_exact_budget_without_relaxing_preop_failures() {
    for budget in [0, 1, 63, 64] {
        for count in [0, 1, 63, 64, 65, u32::MAX] {
            for status in [
                abi::HELPER_DECLINED,
                abi::HELPER_PANICKED,
                REFUSED,
                RETURNED,
                999,
            ] {
                for helper in [None, Some((7, abi::HELPER_COMPLETED))] {
                    let exit = abi::Exit {
                        pc: 7,
                        instructions: count,
                        reason: CHECKPOINT_FAILURE,
                    };
                    let expected = if count <= budget
                        && helper.is_none()
                        && matches!(
                            status,
                            abi::HELPER_DECLINED | abi::HELPER_PANICKED | REFUSED
                        ) {
                        Some(Progress {
                            pc: 7,
                            instructions: count,
                        })
                    } else {
                        None
                    };
                    assert_eq!(progress(&exit, status, helper, budget, u64::MAX), expected);
                    assert!(progress(&exit, status, helper, budget, 7).is_none());
                    for kind in [STORE_FAILURE, HELPER_FAILURE] {
                        let preop = abi::Exit {
                            pc: 7,
                            instructions: budget,
                            reason: kind,
                        };
                        assert!(progress(&preop, status, helper, budget, u64::MAX).is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn packed_failure_progress_refuses_missing_wrong_or_overbudget_metadata() {
    let valid = abi::Exit {
        pc: 7,
        instructions: 3,
        reason: HELPER_FAILURE,
    };
    assert_eq!(
        progress(
            &valid,
            REFUSED,
            Some((7, abi::HELPER_COMPLETED)),
            4,
            u64::MAX
        ),
        Some(Progress {
            pc: 8,
            instructions: 4
        })
    );
    assert!(progress(
        &valid,
        REFUSED,
        Some((8, abi::HELPER_COMPLETED)),
        4,
        u64::MAX
    )
    .is_none());
    assert!(progress(&valid, REFUSED, Some((7, REFUSED)), 4, u64::MAX).is_none());
    assert!(progress(
        &valid,
        REFUSED,
        Some((7, abi::HELPER_COMPLETED)),
        3,
        u64::MAX
    )
    .is_none());
    assert!(progress(&valid, REFUSED, Some((7, abi::HELPER_COMPLETED)), 4, 7).is_none());
    for status in [RETURNED, abi::HELPER_COMPLETED, 999] {
        assert!(progress(&valid, status, None, 4, u64::MAX).is_none());
    }
}

#[cfg(not(miri))]
pub(crate) fn check_native_failure_progress(entry: Entry, helper_failure: bool, registers: usize) {
    with_frame_slots(registers, |frame| {
        let mut exit = abi::Exit {
            pc: u64::MAX,
            instructions: u32::MAX,
            reason: u32::MAX,
        };
        let (status, refusal, helper) = with_bridge_report::<4, _>(frame, |slots, bridge| unsafe {
            (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 4>>())
                .scope
                .count = if helper_failure { registers + 1 } else { 1 };
            entry(slots, 0, 64, &mut exit, bridge)
        })
        .unwrap();
        if !helper_failure {
            assert_eq!(
                (status, refusal, helper),
                (abi::HELPER_DECLINED, None, None)
            );
            assert_eq!(
                (exit.pc, exit.instructions, exit.reason),
                (1, 1, STORE_FAILURE)
            );
            assert_eq!(
                progress(&exit, status, helper, 64, u64::MAX),
                Some(Progress {
                    pc: 1,
                    instructions: 1
                })
            );
            assert!(matches!(frame.registers.stack_frame[0], Value::Integer(42)));
            assert!(matches!(frame.registers.stack_frame[1], Value::Nil));
            assert_eq!(*frame.registers.pc, 5);
            assert_eq!(super::super::tests::counters(&frame.count), [0; 8]);
            assert!(frame.panic.is_none());
            return;
        }
        assert_eq!(
            (status, refusal, helper),
            (
                REFUSED,
                Some(Refusal::Bounds),
                Some((1, abi::HELPER_COMPLETED))
            )
        );
        assert_eq!(
            (exit.pc, exit.instructions, exit.reason),
            (1, 1, HELPER_FAILURE)
        );
        assert_eq!(
            progress(&exit, status, helper, 64, u64::MAX),
            Some(Progress {
                pc: 2,
                instructions: 2
            })
        );
        assert!(matches!(
            frame.registers.stack_frame[0],
            Value::Boolean(true)
        ));
        assert!(matches!(frame.registers.stack_frame[1], Value::Integer(17)));
        assert_eq!(*frame.registers.pc, 2);
        assert_eq!(
            super::super::tests::counters(&frame.count),
            [1, 1, 0, 0, 0, 0, 0, 0]
        );
        assert!(frame.panic.is_none());
    });
}

#[test]
fn packed_entry_contract_clamps_budgets_and_retains_exact_exit_fields() {
    for pc in [0, 7, u32::MAX as usize, usize::MAX - 1, usize::MAX] {
        for budget in [0, 1, 63, 64, 65, u32::MAX] {
            with_frame(|frame| {
                let exit = unsafe { invoke::<2>(frame, echo, 2, pc, budget) }.unwrap();
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (
                        pc as u64,
                        budget.min(64),
                        crate::jit::exits::Kind::Budget as u32
                    )
                );
                assert_eq!(super::super::tests::counters(&frame.count), [0; 8]);
                assert_eq!(*frame.registers.pc, 5);
                assert!(matches!(frame.registers.stack_frame[0], Value::Integer(17)));
            });
        }
    }
}

#[test]
fn packed_entry_admission_and_pending_panics_never_execute_native_code() {
    with_frame(|frame| {
        assert!(matches!(
            unsafe { invoke::<2>(frame, forbidden, 1, 5, 64) },
            Err(Failure::Admission(Refusal::Bounds))
        ));
        assert!(matches!(
            unsafe { invoke::<1>(frame, forbidden, 2, 5, 64) },
            Err(Failure::Admission(Refusal::Bounds))
        ));
        assert!(matches!(
            unsafe { invoke::<257>(frame, forbidden, 2, 5, 64) },
            Err(Failure::Admission(Refusal::Bounds))
        ));
        frame.panic = Some(Box::new(731u64));
        assert!(matches!(
            unsafe { invoke::<2>(frame, forbidden, 2, 5, 64) },
            Err(Failure::Panicked)
        ));
        assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
        assert_eq!(super::super::tests::counters(&frame.count), [0; 8]);
        assert_eq!(*frame.registers.pc, 5);
    });
}

#[test]
fn packed_entry_failures_preserve_effects_without_returning_an_interpreter_exit() {
    for (mode, expected) in [
        (0, Failure::WriterDeclined),
        (1, Failure::Panicked),
        (2, Failure::Refused(Refusal::Bounds)),
        (3, Failure::Refused(Refusal::Bounds)),
        (4, Failure::Status(abi::HELPER_PANICKED)),
        (5, Failure::Status(REFUSED)),
        (6, Failure::Status(abi::HELPER_COMPLETED)),
        (7, Failure::Status(999)),
    ] {
        with_frame(|frame| {
            let result = unsafe { invoke::<3>(frame, aborting, 2, mode, 64) };
            assert_eq!(result.unwrap_err(), expected);
            assert!(matches!(
                frame.registers.stack_frame[0],
                Value::Integer(731)
            ));
            let moved = matches!(mode, 2 | 3);
            assert_eq!(
                super::super::tests::counters(&frame.count),
                [u64::from(moved), u64::from(moved), 0, 0, 0, 0, 0, 0]
            );
            assert_eq!(*frame.registers.pc, if moved { 8 } else { 5 });
            if moved {
                assert!(matches!(
                    frame.registers.stack_frame[1],
                    Value::Integer(731)
                ));
            } else {
                assert!(matches!(frame.registers.stack_frame[1], Value::Nil));
            }
            if mode == 1 {
                assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
            } else {
                assert!(frame.panic.is_none());
            }
        });
    }
}

unsafe extern "C" fn malformed(
    _: *mut Packed,
    mode: u64,
    budget: u32,
    exit: *mut abi::Exit,
    bridge: *const Bridge,
) -> u32 {
    unsafe {
        match mode {
            0 => {}
            1 => {
                (*exit).instructions = 0;
                (*exit).reason = 0;
            }
            2 => {
                (*exit).pc = 5;
                (*exit).reason = 0;
            }
            3 => {
                (*exit).pc = 5;
                (*exit).instructions = 0;
            }
            _ => {
                exit.write(abi::Exit {
                    pc: 5,
                    instructions: if mode == 4 { budget + 1 } else { 0 },
                    reason: match mode {
                        5 => 4,
                        6 => 3,
                        _ => 0,
                    },
                });
                if mode == 7 {
                    boundary::<2>((*bridge).control, |_| std::panic::panic_any(731u64));
                }
            }
        }
    }
    RETURNED
}

#[test]
fn packed_entry_rejects_unwritten_exits_overbudget_counts_and_mismatched_panics() {
    for mode in 0..8 {
        with_frame(|frame| {
            assert_eq!(
                unsafe { invoke::<2>(frame, malformed, 2, mode, 64) }.unwrap_err(),
                Failure::Exit
            );
            assert_eq!(*frame.registers.pc, 5);
            assert!(matches!(frame.registers.stack_frame[0], Value::Integer(17)));
            assert_eq!(super::super::tests::counters(&frame.count), [0; 8]);
            if mode == 7 {
                assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
            }
        });
    }
    unsafe extern "C" fn partial(
        _: *mut Packed,
        _: u64,
        _: u32,
        exit: *mut abi::Exit,
        _: *const Bridge,
    ) -> u32 {
        unsafe {
            (*exit).instructions = 0;
            (*exit).reason = 0;
        }
        RETURNED
    }
    for pc in [usize::MAX - 1, usize::MAX] {
        with_frame(|frame| {
            assert_eq!(
                unsafe { invoke::<2>(frame, partial, 2, pc, 64) }.err(),
                Some(Failure::Exit)
            );
        });
    }
}

#[cfg(not(miri))]
pub(crate) fn check_native_helper(entry: Entry, kind: u32, destination: u32) {
    for budget in [0, 1, 63, 64, 65, u32::MAX] {
        with_frame(|frame| {
            let exit = unsafe { invoke::<3>(frame, entry, 2, 7, budget) }.unwrap();
            let called = budget != 0;
            let completed = called && kind == 1 && destination == 1;
            let declined = called && kind == 4;
            let panicked = called && destination == 99;
            assert_eq!(
                (exit.pc, exit.instructions, exit.reason),
                (
                    if completed { 8 } else { 7 },
                    u32::from(completed),
                    if !called {
                        crate::jit::exits::Kind::Budget as u32
                    } else if panicked {
                        crate::jit::exits::Kind::Panic as u32
                    } else {
                        crate::jit::exits::Kind::Interpreter as u32
                    },
                )
            );
            assert_eq!(
                super::super::tests::counters(&frame.count),
                [
                    u64::from(called),
                    u64::from(completed),
                    u64::from(declined),
                    0,
                    0,
                    0,
                    0,
                    0
                ]
            );
            assert_eq!(
                *frame.registers.pc,
                if completed || panicked {
                    8
                } else if declined {
                    7
                } else {
                    5
                }
            );
            assert_eq!(frame.panic.is_some(), panicked);
            assert!(matches!(frame.registers.stack_frame[0], Value::Integer(17)));
            if completed {
                assert!(matches!(frame.registers.stack_frame[1], Value::Integer(17)));
            } else {
                assert!(matches!(frame.registers.stack_frame[1], Value::Nil));
            }
        });
    }
    if kind == 1 && destination == 1 {
        with_frame(|frame| {
            let mut exit = abi::Exit {
                pc: 999,
                instructions: 123,
                reason: 555,
            };
            let result = with_bridge::<3, _>(frame, |slots, bridge| unsafe {
                (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 3>>())
                    .scope
                    .count = 3;
                assert_eq!(entry(slots, 7, 64, &mut exit, bridge), REFUSED);
            });
            assert_eq!(result, Err(Refusal::Bounds));
            assert_eq!((exit.pc, exit.instructions, exit.reason), (999, 123, 555));
            assert!(matches!(frame.registers.stack_frame[1], Value::Integer(17)));
            assert_eq!(*frame.registers.pc, 8);
            assert_eq!(
                super::super::tests::counters(&frame.count),
                [1, 1, 0, 0, 0, 0, 0, 0]
            );
            assert!(frame.panic.is_none());
        });
    }
}
