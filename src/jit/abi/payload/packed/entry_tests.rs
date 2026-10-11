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
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let mut values = [Value::Integer(17), Value::Nil];
        let mut pc = 5;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 2,
                panic: None,
                projection: None,
            };
            body(&mut frame);
        });
    });
}

#[test]
fn packed_entry_contract_clamps_budgets_and_retains_exact_exit_fields() {
    for pc in [0, 7, u32::MAX as usize] {
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
