use super::*;
use crate::{
    jit::{abi, helpers},
    thread::LuaRegisters,
};

#[repr(C)]
pub(crate) struct Bridge {
    pub base: abi::Host,
    pub read: unsafe extern "C" fn(*mut Payload, u32) -> u64,
    pub write: unsafe extern "C" fn(*mut Bridge, *mut Payload, u32, u64, u64),
    pub helpers: [abi::HelperEntry; 10],
}

unsafe extern "C" fn read_bits(slots: *mut Payload, index: u32) -> u64 {
    unsafe { load(slots.add(index as usize).read()) }.map_or(0, |slot| slot.bits)
}

unsafe extern "C" fn write_scalar(
    host: *mut Bridge,
    slots: *mut Payload,
    index: u32,
    tag: u64,
    bits: u64,
) {
    let result = unsafe { materialize((*host).base.data.cast(), slots, index, tag, bits) };
    assert_eq!(result, abi::HELPER_COMPLETED);
}

unsafe extern "C" fn helper<const KIND: u32>(
    host: *mut abi::Host,
    slots: *mut Slot,
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    unsafe { call::<KIND>((*host).data.cast(), slots.cast(), a, b, c, pc) }
}

pub(crate) fn with_bridge(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    body: impl FnOnce(*mut Payload, *mut Bridge),
) {
    with_session(frame, |session, slots| {
        let mut bridge = Bridge {
            base: abi::Host {
                data: session.cast(),
                projection: std::ptr::null_mut(),
            },
            read: read_bits,
            write: write_scalar,
            helpers: [
                helper::<1>,
                helper::<2>,
                helper::<3>,
                helper::<4>,
                helper::<5>,
                helper::<6>,
                helper::<7>,
                helper::<8>,
                helper::<9>,
                helper::<10>,
            ],
        };
        body(slots, &mut bridge);
    });
}

#[test]
fn bridge_function_table_preserves_original_payload_and_host_pointers() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let mut values = [Value::Nil; 2];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 2,
                panic: None,
                projection: None,
            };
            with_bridge(&mut frame, |slots, host| unsafe {
                ((*host).write)(host, slots, 0, INTEGER, 42);
                assert_eq!(((*host).read)(slots, 0), 42);
                assert_eq!(
                    ((*host).helpers[(abi::HELPER_MOVE - 1) as usize])(
                        host.cast(),
                        slots.cast(),
                        1,
                        0,
                        0,
                        7
                    ),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(((*host).read)(slots, 1), 42);
            });
            assert_eq!((frame.count.calls, frame.count.completed), (1, 1));
        });
        assert_eq!(pc, 8);
        assert!(values
            .iter()
            .all(|value| matches!(value, Value::Integer(42))));
    });
}

struct Session<'s, 'gc, 'a, 'b, 'p> {
    frame: NonNull<helpers::Frame<'gc, 'a, 'b, 'p>>,
    borrow: PhantomData<&'s mut helpers::Frame<'gc, 'a, 'b, 'p>>,
}

impl Session<'_, '_, '_, '_, '_> {
    fn rebind(&mut self, slots: &mut [Payload]) {
        let frame = unsafe { self.frame.as_mut() };
        assert_eq!(slots.len(), frame.slot_count);
        let mut canonical = Frame::new(&mut frame.registers.stack_frame[..slots.len()]);
        for (index, slot) in slots.iter_mut().enumerate() {
            *slot = canonical.bind(index).unwrap();
        }
    }
}

fn with_session(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    body: impl FnOnce(*mut (), *mut Payload),
) {
    let count = frame.slot_count;
    assert!(count <= 256 && count <= frame.registers.stack_frame.len());
    assert!(frame.projection.is_none());
    let mut session = Session {
        frame: NonNull::from(frame),
        borrow: PhantomData,
    };
    let mut slots = [Payload {
        tag: REFERENCE,
        pointer: std::ptr::null_mut(),
    }; 256];
    session.rebind(&mut slots[..count]);
    body(std::ptr::from_mut(&mut session).cast(), slots.as_mut_ptr());
}

unsafe extern "C" fn call<const KIND: u32>(
    host: *mut (),
    slots: *mut Payload,
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    let session = unsafe { &mut *host.cast::<Session<'_, '_, '_, '_, '_>>() };
    let frame = unsafe { session.frame.as_ref() };
    if frame.panic.is_some() {
        return abi::HELPER_PANICKED;
    }
    let count = frame.slot_count;
    let mut scratch = [Slot::canonical(); 256];
    let mut host = abi::Host {
        data: session.frame.as_ptr().cast(),
        projection: std::ptr::null_mut(),
    };
    let result = unsafe { helpers::call::<KIND>(&mut host, scratch.as_mut_ptr(), a, b, c, pc) };
    if result != abi::HELPER_PANICKED {
        session.rebind(unsafe { std::slice::from_raw_parts_mut(slots, count) });
    }
    result
}

unsafe extern "C" fn materialize(
    host: *mut (),
    slots: *mut Payload,
    index: u32,
    tag: u64,
    bits: u64,
) -> u32 {
    let session = unsafe { &mut *host.cast::<Session<'_, '_, '_, '_, '_>>() };
    let frame = unsafe { session.frame.as_mut() };
    if frame.panic.is_some() {
        return abi::HELPER_PANICKED;
    }
    let count = frame.slot_count;
    let value = match tag {
        NIL => Some(Value::Nil),
        BOOLEAN if bits <= 1 => Some(Value::Boolean(bits != 0)),
        INTEGER => Some(Value::Integer(bits as i64)),
        NUMBER => Some(Value::Number(f64::from_bits(bits))),
        _ => None,
    };
    let result = if let Some(value) = value.filter(|_| (index as usize) < count) {
        frame.registers.stack_frame[index as usize] = value;
        abi::HELPER_COMPLETED
    } else {
        abi::HELPER_DECLINED
    };
    session.rebind(unsafe { std::slice::from_raw_parts_mut(slots, count) });
    result
}

#[test]
fn scalar_materialization_preserves_current_aliases_without_transport_counters() {
    crate::Lua::empty().enter(|ctx| {
        for count in [2, 256] {
            let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
            let mut values = vec![Value::Nil; count];
            let mut pc = 17;
            let mut fuel = crate::Fuel::with(67);
            LuaRegisters::with_test_frame_state(
                ctx,
                &mut pc,
                &mut values,
                &mut fuel,
                false,
                |mut registers| {
                    let cell = registers
                        .open_test_upvalue(&ctx, crate::types::RegisterIndex((count - 1) as u8));
                    closure.set_upvalue(&ctx, 0, cell);
                    let mut frame = helpers::Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: count,
                        panic: None,
                        projection: None,
                    };
                    with_session(&mut frame, |host, slots| unsafe {
                        for value in [
                            Value::Integer(i64::MIN),
                            Value::Number(f64::from_bits(0x7ff8000000000042)),
                            Value::Boolean(true),
                            Value::Nil,
                            Value::Number(-0.0),
                            Value::Integer(i64::MAX),
                        ] {
                            let expected = Slot::from_value(value);
                            assert_eq!(
                                materialize(
                                    host,
                                    slots,
                                    (count - 1) as u32,
                                    expected.tag,
                                    expected.bits
                                ),
                                abi::HELPER_COMPLETED
                            );
                            let actual = load(slots.add(count - 1).read()).unwrap();
                            assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                            assert_eq!(
                                call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, 0, 0, 0, 17),
                                abi::HELPER_COMPLETED
                            );
                            let actual = load(slots.read()).unwrap();
                            assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                        }
                        for (index, tag, bits) in [
                            (count as u32, INTEGER, 0),
                            (u32::MAX, INTEGER, 0),
                            (0, BOOLEAN, 2),
                            (0, REFERENCE, 0),
                            (0, u64::MAX, 0),
                        ] {
                            assert_eq!(
                                materialize(host, slots, index, tag, bits),
                                abi::HELPER_DECLINED
                            );
                            assert_eq!(load(slots.read()).unwrap().bits, i64::MAX as u64);
                        }
                        assert_eq!(
                            materialize(host, slots, (count - 1) as u32, INTEGER, i64::MIN as u64),
                            abi::HELPER_COMPLETED
                        );
                    });
                    assert_eq!(
                        (
                            frame.count.calls,
                            frame.count.completed,
                            frame.count.upvalue_reads
                        ),
                        (6, 6, 6)
                    );
                    assert_eq!((frame.count.declined, frame.count.allocations), (0, 0));
                    assert!(frame.panic.is_none());
                },
            );
            assert_eq!(pc, 18);
            assert_eq!(fuel.remaining(), 67);
            assert!(matches!(values[0], Value::Integer(i64::MAX)));
            assert!(matches!(values[count - 1], Value::Integer(i64::MIN)));
        }
    });
}

#[cfg(not(miri))]
unsafe extern "C" fn move_bridge(
    host: *mut (),
    slots: *mut Payload,
    dest: u32,
    source: u32,
    panic: bool,
) -> u32 {
    unsafe {
        call::<{ abi::HELPER_MOVE }>(
            host,
            slots,
            dest,
            if panic { u32::MAX } else { source },
            0,
            42,
        )
    }
}

#[cfg(not(miri))]
#[test]
fn generated_code_calls_production_helpers_and_exits_on_real_bounds_panic() {
    super::native::with_entry(|entry| {
        crate::Lua::empty().enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
            let table = crate::Table::new(&ctx);
            let mut values = [Value::Integer(1), Value::Nil, Value::Table(table)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 3,
                    panic: None,
                    projection: None,
                };
                with_session(&mut frame, |host, slots| {
                    assert_eq!(
                        unsafe { entry(slots, host, move_bridge, INTEGER, 42) },
                        abi::HELPER_PANICKED
                    );
                });
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.declined
                    ),
                    (2, 1, 0)
                );
                assert!(frame.panic.is_some());
            });
            assert_eq!(pc, 43);
            assert!(matches!(values[0], Value::Integer(42)));
            assert!(matches!(values[1], Value::Integer(42)));
            assert!(matches!(values[2], Value::Table(t) if t == table));
        });
    });
}

#[test]
fn production_helpers_observe_direct_writes_and_rebind_current_frame_upvalues() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let table = crate::Table::new(&ctx);
        let mut values = [
            Value::Integer(1),
            Value::Nil,
            Value::Number(2.0),
            Value::Table(table),
        ];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let cell = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            closure.set_upvalue(&ctx, 0, cell);
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 4,
                panic: None,
                projection: None,
            };
            with_session(&mut frame, |host, slots| unsafe {
                assert!(store(
                    slots.read(),
                    Slot {
                        tag: INTEGER,
                        bits: 41
                    }
                ));
                assert_eq!(
                    call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, 1, 0, 0, 10),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(load(slots.add(1).read()).unwrap().bits, 41);
                assert!(store(
                    slots.read(),
                    Slot {
                        tag: INTEGER,
                        bits: 42
                    }
                ));
                assert_eq!(
                    call::<{ abi::HELPER_SET_UPVALUE }>(host, slots, 0, 2, 0, 11),
                    abi::HELPER_COMPLETED
                );
                assert!(store(
                    slots.read(),
                    Slot {
                        tag: NUMBER,
                        bits: (-0.0f64).to_bits()
                    }
                ));
                assert_eq!(
                    call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, 1, 0, 0, 12),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    call::<{ abi::HELPER_MOVE }>(host, slots, 3, 1, 0, 13),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, 2, 14),
                    abi::HELPER_DECLINED
                );
                assert_eq!(load(slots.add(2).read()).unwrap().bits, 2.0f64.to_bits());
                assert_eq!(load(slots.read()).unwrap().bits, (-0.0f64).to_bits());
            });
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.completed,
                    frame.count.declined
                ),
                (5, 4, 1)
            );
            assert_eq!(
                (frame.count.upvalue_reads, frame.count.upvalue_writes),
                (2, 1)
            );
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 14);
        for index in [0, 1, 3] {
            assert!(
                matches!(values[index], Value::Number(n) if n.to_bits() == (-0.0f64).to_bits())
            );
        }
    });
}

#[test]
fn production_helpers_preserve_closed_reference_roots_and_table_effects() {
    let mut lua = crate::Lua::empty();
    let rooted = lua.enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        let table = crate::Table::new(&ctx);
        let mut values = [
            Value::Integer(1),
            Value::Nil,
            Value::Table(table),
            Value::Integer(7),
        ];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 4,
                panic: None,
                projection: None,
            };
            with_session(&mut frame, |host, slots| unsafe {
                assert_eq!(
                    call::<{ abi::HELPER_SET_UPVALUE }>(host, slots, 0, 2, 0, 0),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    call::<{ abi::HELPER_GET_UPVALUE }>(host, slots, 0, 0, 0, 1),
                    abi::HELPER_COMPLETED
                );
                assert!(slots.read().pointer.is_null());
                assert!(load(slots.read()).is_none());
                assert!(store(
                    slots.add(3).read(),
                    Slot {
                        tag: INTEGER,
                        bits: 42
                    }
                ));
                assert_eq!(
                    call::<{ abi::HELPER_SET_TABLE }>(host, slots, 0, 3, 2, 2),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    call::<{ abi::HELPER_GET_TABLE }>(host, slots, 1, 0, 3, 3),
                    abi::HELPER_COMPLETED
                );
                assert!(slots.add(1).read().pointer.is_null());
                assert_eq!(
                    call::<{ abi::HELPER_NEW_TABLE }>(host, slots, 3, 0, 0, 4),
                    abi::HELPER_COMPLETED
                );
                assert!(slots.add(3).read().pointer.is_null());
            });
            assert_eq!((frame.count.calls, frame.count.completed), (5, 5));
            assert_eq!(
                (
                    frame.count.table_reads,
                    frame.count.table_writes,
                    frame.count.allocations
                ),
                (1, 1, 1)
            );
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 5);
        for index in 0..3 {
            assert!(matches!(values[index], Value::Table(t) if t == table));
        }
        assert!(matches!(values[3], Value::Table(t) if t != table));
        assert!(matches!(table.get_raw(&ctx, Value::Integer(42)), Value::Table(t) if t == table));
        ctx.stash(closure)
    });
    lua.gc_collect();
    lua.enter(|ctx| {
        let closure = ctx.fetch(&rooted);
        let crate::closure::UpValueState::Closed(Value::Table(table)) =
            closure.upvalues()[0].get().get()
        else {
            panic!("closed table root was lost");
        };
        assert!(matches!(table.get_raw(&ctx, Value::Integer(42)), Value::Table(t) if t == table));
    });
    drop(rooted);
    lua.gc_collect();
}

#[test]
fn production_helper_panic_keeps_direct_writes_and_stops_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let table = crate::Table::new(&ctx);
        table.set_readonly(&ctx, true);
        let mut values = [Value::Table(table), Value::Integer(0), Value::Number(1.0)];
        let mut pc = 0;
        let mut fuel = crate::Fuel::with(47);
        LuaRegisters::with_test_frame_state(
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
                    slot_count: 3,
                    panic: None,
                    projection: None,
                };
                with_session(&mut frame, |host, slots| unsafe {
                    assert!(store(
                        slots.add(2).read(),
                        Slot {
                            tag: NUMBER,
                            bits: (-0.0f64).to_bits()
                        }
                    ));
                    assert_eq!(
                        call::<{ abi::HELPER_SET_LIST }>(host, slots, 0, 1, 0, 7),
                        abi::HELPER_PANICKED
                    );
                    assert_eq!(
                        call::<{ abi::HELPER_MOVE }>(host, slots, 1, 2, 0, 8),
                        abi::HELPER_PANICKED
                    );
                });
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.declined
                    ),
                    (1, 0, 0)
                );
                assert!(frame.panic.is_some());
            },
        );
        assert_eq!(pc, 8);
        assert_eq!(fuel.remaining(), 42);
        assert!(matches!(values[1], Value::Integer(0)));
        assert!(matches!(values[2], Value::Number(n) if n.to_bits() == (-0.0f64).to_bits()));
        assert!(matches!(table.get_raw(&ctx, Value::Integer(1)), Value::Nil));
    });
}
