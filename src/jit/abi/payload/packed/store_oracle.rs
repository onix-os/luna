use super::*;
use writer_tests::{expected, inputs, same_value, scalar};

pub(crate) type Entry = unsafe extern "C" fn(*mut Packed, *const Bridge, u64, u64, *mut u64) -> u32;

fn model(
    slots: *mut Packed,
    bridge: &Bridge,
    index: usize,
    terminal: bool,
    tag: u64,
    bits: u64,
    marker: &mut u64,
) -> u32 {
    unsafe {
        if !slots.add(index).read().store_numeric(Slot { tag, bits }) {
            let writer = if terminal {
                bridge.functions.terminal
            } else {
                bridge.functions.write
            };
            let result = writer(bridge.control, index as u32, tag, bits);
            if result != abi::HELPER_COMPLETED {
                return result;
            }
        }
    }
    *marker = 999;
    entry::RETURNED
}

fn check(
    mut call: impl FnMut(*mut Packed, &Bridge, u64, u64, &mut u64) -> u32,
    index: usize,
    terminal: bool,
) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for (kind, initial) in kinds(ctx).into_iter().enumerate() {
            if index != 0 && ![0, 3, 5, 9].contains(&kind) {
                continue;
            }
            for (tag, bits) in inputs() {
                if index != 0
                    && ![
                        (NIL, 0),
                        (BOOLEAN, 1),
                        (BOOLEAN, 2),
                        (INTEGER, u64::MAX),
                        (NUMBER, (-0.0f64).to_bits()),
                        (REFERENCE, 0),
                    ]
                    .contains(&(tag, bits))
                {
                    continue;
                }
                for retired in [false, true] {
                    let count = (index + 1).max(3);
                    let integer = usize::from(index == 0);
                    let number = (0..count)
                        .find(|&other| other != index && other != integer)
                        .unwrap();
                    let mut values = vec![Value::Integer(731); count + 1];
                    values[index] = initial;
                    values[integer] = Value::Integer(123);
                    values[number] = Value::Number(-0.0);
                    values[count] = Value::Integer(900);
                    let mut pc = 5;
                    let mut fuel = crate::Fuel::with(47);
                    crate::thread::LuaRegisters::with_test_frame_state(
                        ctx,
                        &mut pc,
                        &mut values,
                        &mut fuel,
                        false,
                        |mut registers| {
                            let mut frame = helpers::Frame {
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: helpers::Counts::default(),
                                slot_count: count,
                                panic: None,
                                read_hints: None,
                                projection: None,
                            };
                            with_bridge::<256, _>(&mut frame, |slots, bridge| unsafe {
                                if retired {
                                    (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 256>>())
                                        .scope
                                        .retire(index);
                                }
                                let original = slots.add(index).read();
                                let sibling_i = slots.add(integer).read();
                                let sibling_n = slots.add(number).read();
                                let fast = !retired
                                    && matches!(tag, INTEGER | NUMBER)
                                    && scalar(initial).0 == tag;
                                let mut marker = 777;
                                let status = call(slots, bridge, tag, bits, &mut marker);
                                let bound =
                                    &*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 256>>();
                                assert_eq!(bound.materializations, u64::from(!fast));
                                if let Some(value) = expected(tag, bits) {
                                    assert_eq!((status, marker), (entry::RETURNED, 999));
                                    assert_eq!(scalar(bound.scope.read(index).unwrap()), value);
                                    if terminal && !fast {
                                        assert_eq!(slots.add(index).read().0.addr(), EMPTY);
                                    } else {
                                        let loaded = slots.add(index).read().load().unwrap();
                                        assert_eq!((loaded.tag, loaded.bits), value);
                                    }
                                } else {
                                    assert_eq!((status, marker), (abi::HELPER_DECLINED, 777));
                                    assert_eq!(slots.add(index).read().0, original.0);
                                    same_value(bound.scope.read(index).unwrap(), initial);
                                }
                                assert!(sibling_i.store_numeric(Slot {
                                    tag: INTEGER,
                                    bits: 731
                                }));
                                assert!(sibling_n.store_numeric(Slot {
                                    tag: NUMBER,
                                    bits: 42f64.to_bits()
                                }));
                            })
                            .unwrap();
                            assert_eq!(tests::counters(&frame.count), [0; 8]);
                            assert!(frame.panic.is_none());
                        },
                    );
                    assert_eq!(pc, 5);
                    assert_eq!(fuel.remaining(), 47);
                    if let Some(value) = expected(tag, bits) {
                        assert_eq!(scalar(values[index]), value);
                    } else {
                        same_value(values[index], initial);
                    }
                    assert!(matches!(values[integer], Value::Integer(731)));
                    assert!(matches!(values[number], Value::Number(42.0)));
                    assert!(matches!(values[count], Value::Integer(900)));
                }
            }
        }
    });
}

#[cfg(not(miri))]
pub(crate) fn check_native(entry: Entry, index: usize, terminal: bool) {
    check(
        |slots, bridge, tag, bits, marker| unsafe { entry(slots, bridge, tag, bits, marker) },
        index,
        terminal,
    );
    if index == 0 {
        check_failures(|slots, bridge, tag, bits, marker| unsafe {
            entry(slots, bridge, tag, bits, marker)
        });
    }
}

unsafe extern "C" fn aborting<const PANIC: bool>(
    control: *mut (),
    index: u32,
    _: u64,
    _: u64,
) -> u32 {
    unsafe {
        boundary::<1>(control, |bound| {
            bound.materializations += 1;
            bound
                .scope
                .replace(index as usize, Value::Integer(42), true)?;
            if PANIC {
                std::panic::panic_any(731u64)
            }
            Err(Refusal::Bounds)
        })
    }
}

struct Aborting<const PANIC: bool>;

impl<const PANIC: bool> Aborting<PANIC> {
    const TABLE: Functions = Functions {
        helpers: Gateways::<1>::TABLE.helpers,
        write: aborting::<PANIC>,
        terminal: aborting::<PANIC>,
    };
}

fn check_failures(call: impl Fn(*mut Packed, &Bridge, u64, u64, &mut u64) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let original = Value::Table(crate::Table::new(&ctx));
        for mode in 0..4 {
            let mut values = [original];
            let mut pc = 5;
            crate::thread::LuaRegisters::with_test_frame(
                ctx,
                &mut pc,
                &mut values,
                |mut registers| {
                    let mut frame = helpers::Frame {
                        read_hints: None,
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: 1,
                        panic: (mode == 1)
                            .then(|| Box::new(731u64) as Box<dyn std::any::Any + Send>),
                        projection: None,
                    };
                    let result = with_bridge::<1, _>(&mut frame, |slots, bridge| unsafe {
                        if mode == 0 {
                            (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 1>>())
                                .scope
                                .stopped = true;
                        }
                        let bridge = Bridge {
                            control: bridge.control,
                            functions: match mode {
                                2 => &Aborting::<true>::TABLE,
                                3 => &Aborting::<false>::TABLE,
                                _ => bridge.functions,
                            },
                        };
                        let mut marker = 777;
                        let result = call(slots, &bridge, INTEGER, 99, &mut marker);
                        assert_eq!(
                            result,
                            if mode == 1 || mode == 2 {
                                abi::HELPER_PANICKED
                            } else {
                                REFUSED
                            }
                        );
                        assert_eq!(marker, 777);
                        assert_eq!(
                            (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 1>>())
                                .materializations,
                            u64::from(mode >= 2)
                        );
                    });
                    assert_eq!(
                        result,
                        match mode {
                            0 => Err(Refusal::Stopped),
                            3 => Err(Refusal::Bounds),
                            _ => Ok(()),
                        }
                    );
                    assert_eq!(tests::counters(&frame.count), [0; 8]);
                    if mode == 1 || mode == 2 {
                        assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
                    } else {
                        assert!(frame.panic.is_none());
                    }
                },
            );
            assert_eq!(pc, 5);
            same_value(
                values[0],
                if mode >= 2 {
                    Value::Integer(42)
                } else {
                    original
                },
            );
        }
    });
}

#[test]
fn packed_store_failures_stop_continuations_without_discarding_partial_effects() {
    for terminal in [false, true] {
        check_failures(|slots, bridge, tag, bits, marker| {
            model(slots, bridge, 0, terminal, tag, bits, marker)
        });
    }
}

#[test]
fn packed_store_model_preserves_numeric_loans_and_materializes_retired_nil_destinations() {
    for index in [0, 1, 255] {
        for terminal in [false, true] {
            check(
                |slots, bridge, tag, bits, marker| {
                    model(slots, bridge, index, terminal, tag, bits, marker)
                },
                index,
                terminal,
            );
        }
    }
}

fn check_copy(call: impl Fn(*mut Packed, &Bridge, u64, u64, &mut u64) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        let destination = Value::Table(crate::Table::new(&ctx));
        for initial in kinds(ctx) {
            for (tag, bits) in inputs() {
                for retired in [false, true] {
                    let mut values = [initial, destination, Value::Integer(900)];
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
                                panic: None,
                                read_hints: None,
                                projection: None,
                            };
                            with_bridge::<3, _>(&mut frame, |slots, bridge| unsafe {
                                if retired {
                                    (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 3>>())
                                        .scope
                                        .retire(0);
                                }
                                let mut marker = 777;
                                let result = call(slots, bridge, tag, bits, &mut marker);
                                let fast = !retired
                                    && matches!(tag, INTEGER | NUMBER)
                                    && scalar(initial).0 == tag;
                                let bound =
                                    &*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 3>>();
                                if let Some(value) = expected(tag, bits) {
                                    assert_eq!((result, marker), (entry::RETURNED, 999));
                                    assert_eq!(scalar(bound.scope.read(0).unwrap()), value);
                                    assert_eq!(scalar(bound.scope.read(1).unwrap()), value);
                                    assert_eq!(slots.add(1).read().0.addr(), EMPTY);
                                    assert_eq!(bound.materializations, u64::from(!fast) + 1);
                                } else {
                                    assert_eq!((result, marker), (abi::HELPER_DECLINED, 777));
                                    same_value(bound.scope.read(0).unwrap(), initial);
                                    same_value(bound.scope.read(1).unwrap(), destination);
                                    assert_eq!(bound.materializations, 1);
                                }
                            })
                            .unwrap();
                            assert_eq!(tests::counters(&frame.count), [0; 8]);
                            assert!(frame.panic.is_none());
                        },
                    );
                    assert_eq!(pc, 5);
                    assert!(matches!(values[2], Value::Integer(900)));
                    if let Some(value) = expected(tag, bits) {
                        assert_eq!(scalar(values[0]), value);
                        assert_eq!(scalar(values[1]), value);
                    } else {
                        same_value(values[0], initial);
                        same_value(values[1], destination);
                    }
                }
            }
        }
    });
}

#[cfg(not(miri))]
pub(crate) fn check_native_copy(entry: Entry) {
    check_copy(|slots, bridge, tag, bits, marker| unsafe {
        entry(slots, bridge, tag, bits, marker)
    });
}

#[test]
fn packed_store_read_copy_model_preserves_canonical_data_and_terminal_retirement() {
    check_copy(|slots, bridge, tag, bits, marker| {
        let mut ignored = 777;
        let result = model(slots, bridge, 0, false, tag, bits, &mut ignored);
        if result != entry::RETURNED {
            return result;
        }
        let source = unsafe { slots.read().load().unwrap() };
        model(slots, bridge, 1, true, source.tag, source.bits, marker)
    });
}
