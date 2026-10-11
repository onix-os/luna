use super::*;

#[repr(C, align(8))]
struct AlignedValues<'gc, const N: usize> {
    prefix: u32,
    values: [Value<'gc>; N],
}

impl<'gc, const N: usize> AlignedValues<'gc, N> {
    fn new(values: [Value<'gc>; N]) -> Self {
        Self {
            prefix: 731,
            values,
        }
    }

    fn check_alignment(&mut self) {
        for value in &mut self.values {
            assert!(Packed::bind(value).is_some());
        }
    }
}

impl<'gc, const N: usize> std::ops::Deref for AlignedValues<'gc, N> {
    type Target = [Value<'gc>];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl<const N: usize> std::ops::DerefMut for AlignedValues<'_, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.values
    }
}

fn equivalent<'gc>(left: Value<'gc>, right: Value<'gc>) {
    let l = Slot::from_value(left);
    let r = Slot::from_value(right);
    assert_eq!((l.tag, l.bits), (r.tag, r.bits));
    if l.tag == REFERENCE {
        match (left, right) {
            (Value::String(a), Value::String(b)) => {
                assert!(ottavino_gc_arena::Gc::ptr_eq(
                    a.into_inner(),
                    b.into_inner()
                ));
            }
            (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
            (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
            (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
            (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
            _ => panic!("reference kind changed"),
        }
    }
}

fn prefixes<const CAPACITY: usize>() {
    for count in [0, CAPACITY.saturating_sub(1), CAPACITY, CAPACITY + 1] {
        let mut values = vec![Value::Integer(17); count + 1];
        let mut entered = false;
        let result = with_scope::<CAPACITY, _>(&mut values, count, |scope| {
            entered = true;
            assert_eq!(scope.count, count);
            for index in 0..count {
                equivalent(scope.read(index).unwrap(), Value::Integer(17));
                assert!(scope
                    .store_numeric(
                        index,
                        Slot {
                            tag: INTEGER,
                            bits: 42
                        }
                    )
                    .unwrap());
            }
            assert!(matches!(scope.read(count), Err(Refusal::Bounds)));
            assert_eq!(
                scope.replace(count, Value::Nil, false),
                Err(Refusal::Bounds)
            );
            if count > 0 {
                scope
                    .replace(count - 1, Value::Boolean(true), false)
                    .unwrap();
                equivalent(scope.read(count - 1).unwrap(), Value::Boolean(true));
                scope.replace(count - 1, Value::Number(-0.0), true).unwrap();
                equivalent(scope.read(count - 1).unwrap(), Value::Number(-0.0));
                assert!(!scope
                    .store_numeric(
                        count - 1,
                        Slot {
                            tag: NUMBER,
                            bits: 0
                        }
                    )
                    .unwrap());
            }
        });
        if CAPACITY > 256 || count > CAPACITY {
            assert_eq!(result, Err(Refusal::Bounds));
            assert!(!entered);
            assert!(values.iter().all(|v| matches!(v, Value::Integer(17))));
        } else {
            match result {
                Ok(()) => assert!(entered),
                Err(Refusal::Alignment) => {
                    assert!(std::mem::align_of::<i64>() < 8);
                    assert!(!entered);
                }
                other => panic!("unexpected admission: {other:?}"),
            }
        }
        equivalent(values[count], Value::Integer(17));
    }
}

#[test]
fn packed_scopes_bound_initialized_prefixes_and_preserve_tails() {
    prefixes::<0>();
    prefixes::<1>();
    prefixes::<7>();
    prefixes::<8>();
    prefixes::<16>();
    prefixes::<64>();
    prefixes::<256>();
    prefixes::<257>();
    let mut values = [Value::Boolean(false)];
    assert_eq!(
        with_scope::<8, _>(&mut values, 2, |_| panic!("entered invalid frame")),
        Err(Refusal::Bounds)
    );
    assert_eq!(
        with_scope::<8, _>(&mut values, usize::MAX, |_| panic!(
            "entered oversized frame"
        )),
        Err(Refusal::Bounds)
    );
}

#[test]
fn packed_destination_refresh_keeps_siblings_live_for_every_kind() {
    crate::Lua::empty().enter(|ctx| {
        for initial in kinds(ctx) {
            for next in kinds(ctx) {
                let mut values =
                    AlignedValues::new([initial, Value::Integer(123), Value::Number(-0.0)]);
                values.check_alignment();
                let result = with_scope::<3, _>(&mut values, 3, |scope| {
                    let siblings = [unsafe { scope.descriptors[1].assume_init() }, unsafe {
                        scope.descriptors[2].assume_init()
                    }];
                    let copied = scope.read(0).unwrap();
                    equivalent(copied, initial);
                    scope
                        .with_destination(0, |value| {
                            equivalent(*value, initial);
                            *value = next;
                        })
                        .unwrap();
                    equivalent(scope.read(0).unwrap(), next);
                    assert!(unsafe {
                        siblings[0].store_numeric(Slot {
                            tag: INTEGER,
                            bits: 731,
                        })
                    });
                    assert!(unsafe {
                        siblings[1].store_numeric(Slot {
                            tag: NUMBER,
                            bits: 42f64.to_bits(),
                        })
                    });
                    equivalent(scope.read(1).unwrap(), Value::Integer(731));
                    equivalent(scope.read(2).unwrap(), Value::Number(42.0));
                    scope.replace(0, next, true).unwrap();
                    equivalent(scope.read(0).unwrap(), next);
                    scope
                        .with_destination(0, |value| equivalent(*value, next))
                        .unwrap();
                    equivalent(scope.read(0).unwrap(), next);
                });
                if result == Err(Refusal::Alignment) {
                    assert!(std::mem::align_of::<i64>() < 8 || std::mem::align_of::<f64>() < 8);
                } else {
                    assert_eq!(result, Ok(()));
                }
            }
        }
    });
}

#[test]
fn packed_full_refresh_observes_boolean_aliases_and_prefix_external_values() {
    crate::Lua::empty().enter(|ctx| {
        for next in kinds(ctx) {
            let mut values = AlignedValues::new([
                Value::Boolean(false),
                Value::Integer(17),
                Value::Number(-0.0),
                next,
            ]);
            values.check_alignment();
            with_scope::<3, _>(&mut values, 3, |scope| {
                assert!(scope
                    .store_numeric(
                        1,
                        Slot {
                            tag: INTEGER,
                            bits: 731
                        }
                    )
                    .unwrap());
                scope
                    .with_canonical(|canonical| {
                        assert_eq!(canonical.len(), 4);
                        equivalent(canonical[1], Value::Integer(731));
                        canonical[0] = Value::Boolean(true);
                        canonical[1] = canonical[3];
                        canonical[2] = Value::Integer(42);
                    })
                    .unwrap();
                equivalent(scope.read(0).unwrap(), Value::Boolean(true));
                equivalent(scope.read(1).unwrap(), next);
                equivalent(scope.read(2).unwrap(), Value::Integer(42));
                scope.replace(1, Value::Boolean(false), false).unwrap();
                scope
                    .with_destination(1, |value| *value = Value::Boolean(true))
                    .unwrap();
                equivalent(scope.read(1).unwrap(), Value::Boolean(true));
            })
            .unwrap();
            equivalent(values[3], next);
        }
    });
}

#[test]
fn packed_helper_panics_stop_reentry_without_losing_writes_or_payloads() {
    for full in [false, true] {
        let mut values = AlignedValues::new([Value::Integer(17), Value::Number(-0.0)]);
        values.check_alignment();
        with_scope::<2, _>(&mut values, 2, |scope| {
            assert!(scope
                .store_numeric(
                    1,
                    Slot {
                        tag: NUMBER,
                        bits: 42f64.to_bits()
                    }
                )
                .unwrap());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if full {
                    scope
                        .with_canonical(|values| {
                            values[0] = Value::Boolean(true);
                            std::panic::panic_any(731u64);
                        })
                        .unwrap();
                } else {
                    scope
                        .with_destination(0, |value| {
                            *value = Value::Boolean(true);
                            std::panic::panic_any(731u64);
                        })
                        .unwrap();
                }
            }));
            assert_eq!(*result.unwrap_err().downcast::<u64>().unwrap(), 731);
            assert!(matches!(scope.read(0), Err(Refusal::Stopped)));
            assert_eq!(
                scope.store_numeric(
                    1,
                    Slot {
                        tag: NUMBER,
                        bits: 0
                    }
                ),
                Err(Refusal::Stopped)
            );
            assert_eq!(scope.replace(0, Value::Nil, false), Err(Refusal::Stopped));
            assert_eq!(
                scope.with_destination(0, |_| panic!("reentered destination")),
                Err(Refusal::Stopped)
            );
            assert_eq!(
                scope.with_canonical(|_| panic!("reentered frame")),
                Err(Refusal::Stopped)
            );
        })
        .unwrap();
        equivalent(values[0], Value::Boolean(true));
        equivalent(values[1], Value::Number(42.0));
    }
}

#[test]
fn packed_alignment_refusal_stops_after_canonical_replacement() {
    #[repr(C, align(8))]
    struct Positioned<'gc> {
        prefix: u32,
        value: Value<'gc>,
    }
    let mut positioned = Positioned {
        prefix: 731,
        value: Value::Integer(17),
    };
    let Value::Integer(field) = &positioned.value else {
        unreachable!()
    };
    let aligned = std::ptr::from_ref(field).addr() & MASK == 0;
    let admission = with_scope::<1, _>(std::slice::from_mut(&mut positioned.value), 1, |scope| {
        equivalent(scope.read(0).unwrap(), Value::Integer(17));
    });
    assert_eq!(
        admission,
        if aligned {
            Ok(())
        } else {
            Err(Refusal::Alignment)
        }
    );
    equivalent(positioned.value, Value::Integer(17));
    positioned.value = Value::Boolean(false);
    with_scope::<1, _>(std::slice::from_mut(&mut positioned.value), 1, |scope| {
        let result = scope.replace(0, Value::Integer(42), false);
        if aligned {
            assert_eq!(result, Ok(()));
            equivalent(scope.read(0).unwrap(), Value::Integer(42));
        } else {
            assert_eq!(result, Err(Refusal::Alignment));
            assert!(matches!(scope.read(0), Err(Refusal::Stopped)));
        }
    })
    .unwrap();
    equivalent(positioned.value, Value::Integer(42));
    assert_eq!(positioned.prefix, 731);
}

#[test]
fn packed_real_frame_helpers_refresh_retired_values_and_canonical_writes() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for next in kinds(ctx) {
            let mut values =
                AlignedValues::new([Value::Nil, Value::Boolean(false), Value::Integer(17), next]);
            values.check_alignment();
            let mut pc = 5;
            crate::thread::LuaRegisters::with_test_frame(
                ctx,
                &mut pc,
                &mut values,
                |mut registers| {
                    let mut frame = helpers::Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: 3,
                        panic: None,
                        projection: None,
                    };
                    with_frame_scope::<3, _>(&mut frame, |bound| {
                        assert!(bound
                            .scope
                            .store_numeric(
                                2,
                                Slot {
                                    tag: INTEGER,
                                    bits: 731
                                }
                            )
                            .unwrap());
                        assert_eq!(
                            bound.helper::<{ abi::HELPER_MOVE }>(0, 2, 0, 7),
                            Ok(abi::HELPER_COMPLETED)
                        );
                        equivalent(bound.scope.read(0).unwrap(), Value::Integer(731));
                        bound.scope.replace(0, next, true).unwrap();
                        assert_eq!(
                            bound.helper::<{ abi::HELPER_MOVE }>(1, 0, 0, 8),
                            Ok(abi::HELPER_COMPLETED)
                        );
                        equivalent(bound.scope.read(0).unwrap(), next);
                        equivalent(bound.scope.read(1).unwrap(), next);
                        assert!(bound
                            .scope
                            .store_numeric(
                                2,
                                Slot {
                                    tag: INTEGER,
                                    bits: 42
                                }
                            )
                            .unwrap());
                    })
                    .unwrap();
                    assert_eq!(
                        (
                            frame.count.calls,
                            frame.count.completed,
                            frame.count.declined
                        ),
                        (2, 2, 0)
                    );
                    assert!(frame.panic.is_none());
                },
            );
            assert_eq!(pc, 9);
            equivalent(values[0], next);
            equivalent(values[1], next);
            equivalent(values[2], Value::Integer(42));
            equivalent(values[3], next);
        }
    });
}

#[test]
fn packed_real_frame_declines_refresh_and_panics_stop_helper_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for already_stopped in [false, true] {
            let mut values = AlignedValues::new([Value::Integer(17), Value::Boolean(false)]);
            values.check_alignment();
            let mut pc = 5;
            crate::thread::LuaRegisters::with_test_frame(
                ctx,
                &mut pc,
                &mut values,
                |mut registers| {
                    let mut frame = helpers::Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: 2,
                        panic: already_stopped
                            .then(|| Box::new(731u64) as Box<dyn std::any::Any + Send>),
                        projection: None,
                    };
                    with_frame_scope::<2, _>(&mut frame, |bound| {
                        if !already_stopped {
                            assert_eq!(
                                bound.helper::<{ abi::HELPER_GET_TABLE }>(1, 0, 0, 18),
                                Ok(abi::HELPER_DECLINED)
                            );
                            equivalent(bound.scope.read(1).unwrap(), Value::Boolean(false));
                            assert!(bound
                                .scope
                                .store_numeric(
                                    0,
                                    Slot {
                                        tag: INTEGER,
                                        bits: 42
                                    }
                                )
                                .unwrap());
                            assert_eq!(
                                bound.helper::<{ abi::HELPER_MOVE }>(99, 0, 0, 19),
                                Ok(abi::HELPER_PANICKED)
                            );
                        }
                        assert_eq!(
                            bound.helper::<{ abi::HELPER_MOVE }>(1, 0, 0, 20),
                            Ok(abi::HELPER_PANICKED)
                        );
                        assert!(matches!(bound.scope.read(0), Err(Refusal::Stopped)));
                        assert_eq!(
                            bound.scope.replace(0, Value::Nil, false),
                            Err(Refusal::Stopped)
                        );
                    })
                    .unwrap();
                    if already_stopped {
                        assert_eq!(frame.count.calls, 0);
                        assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
                    } else {
                        assert_eq!(
                            (
                                frame.count.calls,
                                frame.count.completed,
                                frame.count.declined
                            ),
                            (2, 0, 1)
                        );
                        assert!(frame.panic.is_some());
                    }
                },
            );
            assert_eq!(pc, if already_stopped { 5 } else { 20 });
            equivalent(
                values[0],
                Value::Integer(if already_stopped { 17 } else { 42 }),
            );
            equivalent(values[1], Value::Boolean(false));
        }
    });
}

#[test]
fn packed_stopped_scope_does_not_invent_a_frame_panic() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let mut values = AlignedValues::new([Value::Boolean(false), Value::Integer(17)]);
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
            with_frame_scope::<2, _>(&mut frame, |bound| {
                assert!(bound
                    .scope
                    .store_numeric(
                        1,
                        Slot {
                            tag: INTEGER,
                            bits: 42
                        }
                    )
                    .unwrap());
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    bound
                        .scope
                        .with_destination(0, |value| {
                            *value = Value::Boolean(true);
                            std::panic::panic_any(731u64);
                        })
                        .unwrap();
                }));
                assert_eq!(*panic.unwrap_err().downcast::<u64>().unwrap(), 731);
                assert_eq!(
                    bound.helper::<{ abi::HELPER_MOVE }>(0, 1, 0, 7),
                    Err(Refusal::Stopped)
                );
            })
            .unwrap();
            assert!(frame.panic.is_none());
            assert_eq!(frame.count.calls, 0);
        });
        assert_eq!(pc, 5);
        equivalent(values[0], Value::Boolean(true));
        equivalent(values[1], Value::Integer(42));
    });
}
