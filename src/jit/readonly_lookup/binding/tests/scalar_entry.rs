use super::*;
use crate::jit::readonly_lookup::scalar_entry::Source;

fn proof(closure: Closure<'_>) -> Source {
    let source = crate::jit::ir::Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
    Source::admit(&source).unwrap()
}

unsafe extern "C" fn scalar_probe(
    slots: *mut abi::Slot,
    pc: u64,
    budget: u32,
    exit: *mut abi::Exit,
    host: *mut abi::Host,
    view: *mut View,
) {
    assert!(host.is_null());
    assert!(budget <= 64);
    unsafe {
        *exit = abi::Exit {
            pc,
            instructions: 0,
            reason: crate::jit::exits::Kind::Budget as u32,
        };
        if budget != 0 {
            let destination = (((*view).binding[1] >> 16) & 255) as usize;
            *slots.add(destination) = (*view).slot;
            *slots = abi::Slot {
                tag: abi::INTEGER,
                bits: 77,
            };
            (*view).executed = 1;
            (*exit).instructions = 1;
            (*exit).pc = pc + 1;
        }
    }
}

unsafe extern "C" fn fault<const MARKER: u64, const INSTRUCTIONS: u32>(
    slots: *mut abi::Slot,
    pc: u64,
    _: u32,
    exit: *mut abi::Exit,
    _: *mut abi::Host,
    view: *mut View,
) {
    unsafe {
        let destination = (((*view).binding[1] >> 16) & 255) as usize;
        *slots.add(destination) = (*view).slot;
        *slots = abi::Slot {
            tag: abi::INTEGER,
            bits: 77,
        };
        (*view).executed = MARKER;
        *exit = abi::Exit {
            pc,
            instructions: INSTRUCTIONS,
            reason: crate::jit::exits::Kind::Budget as u32,
        };
    }
}

#[test]
fn scalar_bridge_has_no_host_and_defers_scalar_pc_and_statistics_publication() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let source = proof(closure);
        let width = usize::from(plan.registers);
        let Constant::String(key) = closure.prototype().constants[usize::from(plan.constant)]
        else {
            panic!()
        };
        let callback: Value<'_> =
            Callback::from_fn(&ctx, |_, _, _| panic!("entry called a user")).into();
        for value in [callback, Value::Integer(71)] {
            environment.set(ctx, key, value).unwrap();
            for budget in [0, 1, 63, 64, 65, u32::MAX] {
                let mut values = vec![Value::Integer(-7); width];
                let mut pc = 17;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                    let mut slots = vec![abi::Slot::from_value(Value::Integer(-7)); width];
                    let outcome = unsafe {
                        source.invoke_scalar_fresh(
                            scalar_probe,
                            closure,
                            &hints,
                            &mut registers,
                            &mut slots,
                            17,
                            budget,
                        )
                    }
                    .unwrap()
                    .unwrap();
                    assert_eq!(*registers.pc, 17);
                    let entered = u64::from(budget != 0);
                    assert_eq!(outcome.reads, entered);
                    assert_eq!(
                        (outcome.exit.pc, outcome.exit.instructions),
                        (17 + entered, entered as u32)
                    );
                    for (index, &actual) in registers.stack_frame.iter().enumerate() {
                        same(
                            actual,
                            if budget != 0 && index == usize::from(plan.destination()) {
                                value
                            } else {
                                Value::Integer(-7)
                            },
                        );
                    }
                    assert_eq!(slots[0].bits as i64, if budget == 0 { -7 } else { 77 });
                });
                assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
            }
        }
    });
}

#[test]
fn marshaled_bridge_matches_fresh_fault_publication_and_consumes_certificates() {
    Lua::empty().enter(|ctx| {
        let (_, closure, _, hints) = prepared(ctx);
        let source = proof(closure);
        let width = usize::from(source.plan().registers);
        let entries: [Entry; 6] = [
            scalar_probe,
            fault::<0, 0>,
            fault::<1, 0>,
            fault::<1, 1>,
            fault::<1, 65>,
            fault::<2, 1>,
        ];
        for entry in entries {
            for budget in [0, 1, 63, 64, 65, u32::MAX] {
                let run = |classified: bool| {
                    let mut values = vec![Value::Integer(-7); width];
                    let mut pc = 17;
                    let (exit, slots) = LuaRegisters::with_test_frame(
                        ctx,
                        &mut pc,
                        &mut values,
                        |mut registers| {
                            let initial = vec![Value::Integer(-7); width];
                            let mut slots: Vec<_> =
                                initial.iter().copied().map(abi::Slot::from_value).collect();
                            let mut storage = vec![std::mem::MaybeUninit::uninit(); width];
                            let mut inputs = abi::transport_inputs::Inputs::marshal::<true>(
                                &mut storage,
                                &initial,
                            );
                            let result = unsafe {
                                if classified {
                                    source.invoke_marshaled(
                                        entry,
                                        closure,
                                        &hints,
                                        &mut registers,
                                        &mut inputs,
                                        17,
                                        budget,
                                    )
                                } else {
                                    source.invoke_scalar_fresh(
                                        entry,
                                        closure,
                                        &hints,
                                        &mut registers,
                                        &mut slots,
                                        17,
                                        budget,
                                    )
                                }
                            };
                            let exit = match result {
                                Ok(Some(outcome)) => Ok((
                                    outcome.exit.pc,
                                    outcome.exit.instructions,
                                    outcome.exit.reason,
                                    outcome.reads,
                                )),
                                Ok(None) => panic!("scalar bridge refused valid inputs"),
                                Err(error) => Err(format!("{error:?}")),
                            };
                            if classified {
                                assert!(inputs.take_scalar().is_none());
                            }
                            let slots = if classified { inputs.slots() } else { &slots };
                            (
                                exit,
                                slots.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>(),
                            )
                        },
                    );
                    (exit, slots, values, pc)
                };
                let a = run(false);
                let b = run(true);
                assert_eq!(a.0, b.0);
                assert_eq!(a.1, b.1);
                assert_eq!(a.3, b.3);
                for (a, b) in a.2.into_iter().zip(b.2) {
                    same(a, b);
                }
            }
        }
    });
}

#[test]
fn scalar_bridge_refusals_are_effect_free_and_release_every_binding() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let source = proof(closure);
        let width = usize::from(plan.registers);
        for case in 0..width + 4 {
            let mut values = vec![Value::Integer(-7); width - usize::from(case == width)];
            let mut pc = 17;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut slots = vec![
                    abi::Slot::from_value(Value::Integer(-7));
                    width - usize::from(case == width + 1)
                ];
                if case < width {
                    slots[case].tag = abi::REFERENCE;
                }
                let previous: Vec<_> = slots.iter().map(|slot| (slot.tag, slot.bits)).collect();
                let result = if case == width + 2 {
                    let _borrow = environment.into_inner().borrow_mut(&ctx);
                    unsafe {
                        source.invoke_scalar_fresh(
                            never,
                            closure,
                            &hints,
                            &mut registers,
                            &mut slots,
                            17,
                            64,
                        )
                    }
                } else if case == width + 3 {
                    hints.hint(plan.helper_pc as usize).unwrap().remember(None);
                    unsafe {
                        source.invoke_scalar_fresh(
                            never,
                            closure,
                            &hints,
                            &mut registers,
                            &mut slots,
                            17,
                            64,
                        )
                    }
                } else {
                    unsafe {
                        source.invoke_scalar_fresh(
                            never,
                            closure,
                            &hints,
                            &mut registers,
                            &mut slots,
                            17,
                            64,
                        )
                    }
                }
                .unwrap();
                assert!(result.is_none());
                assert_eq!(*registers.pc, 17);
                assert_eq!(
                    previous,
                    slots
                        .iter()
                        .map(|slot| (slot.tag, slot.bits))
                        .collect::<Vec<_>>()
                );
                assert!(registers
                    .stack_frame
                    .iter()
                    .all(|v| matches!(v, Value::Integer(-7))));
            });
            assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
        }
    });
}

#[test]
fn scalar_bridge_faults_match_framed_partial_canonical_and_scratch_effects() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let source = proof(closure);
        let width = usize::from(plan.registers);
        let entries: [Entry; 4] = [fault::<2, 1>, fault::<1, 0>, fault::<1, 65>, fault::<0, 65>];
        for entry in entries {
            let run = |scalar: bool| {
                let mut values = vec![Value::Integer(-7); width];
                let mut pc = 17;
                let (error, slots) =
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                        let mut slots = vec![abi::Slot::from_value(Value::Integer(-7)); width];
                        let error = if scalar {
                            unsafe {
                                source.invoke_scalar_fresh(
                                    entry,
                                    closure,
                                    &hints,
                                    &mut registers,
                                    &mut slots,
                                    17,
                                    64,
                                )
                            }
                            .err()
                            .unwrap()
                        } else {
                            let mut frame = helpers::Frame {
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: helpers::Counts::default(),
                                slot_count: width,
                                panic: None,
                                projection: None,
                                read_hints: Some(&hints),
                            };
                            unsafe { plan.invoke_fresh(entry, &mut frame, &mut slots, 17, 64) }
                                .err()
                                .unwrap()
                        };
                        (
                            error,
                            slots
                                .iter()
                                .map(|slot| (slot.tag, slot.bits))
                                .collect::<Vec<_>>(),
                        )
                    });
                (error, slots, values, pc)
            };
            let framed = run(false);
            let scalar = run(true);
            assert_eq!(
                (scalar.0, scalar.1, scalar.3),
                (framed.0, framed.1, framed.3)
            );
            for (&a, &b) in scalar.2.iter().zip(&framed.2) {
                same(a, b);
            }
            assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
        }
    });
}
