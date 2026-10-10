use super::*;
use crate::{
    jit::{abi, helpers},
    thread::LuaRegisters,
};

#[cfg(test)]
#[path = "local_materialize_tests.rs"]
mod local_tests;

#[cfg(test)]
#[path = "closed_table_tests.rs"]
mod closed_table_tests;

#[repr(C)]
pub(crate) struct Bridge {
    pub base: abi::Host,
    pub read: unsafe extern "C" fn(*mut Payload, u32) -> u64,
    pub write: unsafe extern "C" fn(*mut Bridge, *mut Payload, u32, u64, u64),
    pub helpers: [abi::HelperEntry; 10],
    materializations: u64,
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
    unsafe { (*host).materializations += 1 };
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

#[inline(always)]
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
            materializations: 0,
        };
        body(slots, &mut bridge);
    });
}

#[cfg(not(miri))]
pub(crate) fn check_native_write(entry: unsafe extern "C" fn(*mut Payload, *mut Bridge, u64, u64)) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let scalars = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
        ];
        for initial in scalars.into_iter().chain([
            Value::String(ctx.intern(b"payload")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ]) {
            for destination in scalars {
                let expected = Slot::from_value(destination);
                let final_bits = expected.bits
                    ^ match expected.tag {
                        BOOLEAN => 1,
                        INTEGER => u64::MAX,
                        NUMBER => 1 << 63,
                        _ => 0,
                    };
                let mut values = [initial, Value::Integer(12345)];
                let mut pc = 17;
                let mut fuel = crate::Fuel::with(67);
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
                            slot_count: 2,
                            panic: None,
                            projection: None,
                        };
                        with_bridge(&mut frame, |slots, host| unsafe {
                            let materializations =
                                u64::from(Slot::from_value(initial).tag != expected.tag);
                            for bits in [expected.bits, final_bits] {
                                entry(slots, host, expected.tag, bits);
                                assert_eq!((*host).materializations, materializations);
                                let actual = load(slots.read()).unwrap();
                                assert_eq!((actual.tag, actual.bits), (expected.tag, bits));
                                let sibling = load(slots.add(1).read()).unwrap();
                                assert_eq!((sibling.tag, sibling.bits), (INTEGER, 12345));
                            }
                        });
                        assert_eq!(frame.count.calls, 0);
                        assert_eq!(frame.count.completed, 0);
                        assert_eq!(frame.count.declined, 0);
                        assert_eq!(frame.count.allocations, 0);
                        assert_eq!(frame.count.table_reads, 0);
                        assert_eq!(frame.count.table_writes, 0);
                        assert_eq!(frame.count.upvalue_reads, 0);
                        assert_eq!(frame.count.upvalue_writes, 0);
                        assert!(frame.panic.is_none());
                    },
                );
                let actual = Slot::from_value(values[0]);
                assert_eq!((actual.tag, actual.bits), (expected.tag, final_bits));
                assert!(matches!(values[1], Value::Integer(12345)));
                assert_eq!(pc, 17);
                assert_eq!(fuel.remaining(), 67);
            }
        }
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
    values: NonNull<Value<'gc>>,
    counts: NonNull<helpers::Counts>,
    pc: NonNull<usize>,
    ctx: crate::Context<'gc>,
    closure: crate::Closure<'gc>,
    count: usize,
    stopped: bool,
    borrow: PhantomData<&'s mut helpers::Frame<'gc, 'a, 'b, 'p>>,
}

impl Session<'_, '_, '_, '_, '_> {
    fn bind_each(&mut self, count: usize, mut write: impl FnMut(usize, Payload)) {
        let frame = unsafe { self.frame.as_mut() };
        assert_eq!(count, frame.slot_count);
        self.counts = NonNull::from(&mut frame.count);
        self.pc = NonNull::from(&mut *frame.registers.pc);
        let mut canonical = Frame::new(&mut frame.registers.stack_frame[..count]);
        self.values = canonical.values;
        for index in 0..count {
            write(index, canonical.bind(index).unwrap());
        }
    }

    fn rebind(&mut self, slots: &mut [Payload]) {
        self.bind_each(slots.len(), |index, value| slots[index] = value);
    }
}

#[inline(always)]
fn with_session(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    body: impl FnOnce(*mut (), *mut Payload),
) {
    let count = frame.slot_count;
    assert!(count <= 256 && count <= frame.registers.stack_frame.len());
    assert!(frame.projection.is_none());
    let stopped = frame.panic.is_some();
    let ctx = frame.ctx;
    let closure = frame.closure;
    let mut session = Session {
        frame: NonNull::from(frame),
        values: NonNull::dangling(),
        counts: NonNull::dangling(),
        pc: NonNull::dangling(),
        ctx,
        closure,
        count,
        stopped,
        borrow: PhantomData,
    };
    let mut slots = [std::mem::MaybeUninit::<Payload>::uninit(); 256];
    session.bind_each(count, |index, value| {
        slots[index].write(value);
    });
    body(
        std::ptr::from_mut(&mut session).cast(),
        slots.as_mut_ptr().cast(),
    );
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
    if session.stopped {
        return abi::HELPER_PANICKED;
    }
    if KIND == abi::HELPER_GET_UP_TABLE {
        if let Some(result) = closed_table(session, slots, a, b, c, pc) {
            return result;
        }
    }
    let count = session.count;
    let result = helpers::call_direct::<KIND>(unsafe { session.frame.as_mut() }, a, b, c, pc);
    if result != abi::HELPER_PANICKED {
        session.rebind(unsafe { std::slice::from_raw_parts_mut(slots, count) });
    } else {
        session.stopped = true;
    }
    result
}

fn closed_table(
    session: &mut Session<'_, '_, '_, '_, '_>,
    slots: *mut Payload,
    dest: u32,
    upvalue: u32,
    operand: u32,
    pc: u32,
) -> Option<u32> {
    if dest as usize >= session.count || operand & abi::CONSTANT_OPERAND == 0 {
        return None;
    }
    let crate::closure::UpValueState::Closed(table) = session
        .closure
        .upvalues()
        .get(upvalue as usize)?
        .get()
        .get()
    else {
        return None;
    };
    let key = session
        .closure
        .prototype()
        .constants
        .get((operand & !abi::CONSTANT_OPERAND) as usize)
        .copied()?
        .into();
    unsafe {
        session.counts.as_mut().calls += 1;
        *session.pc.as_ptr() = pc as usize + 1;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        helpers::with_table_value(session.ctx, table, key, |value| {
            let mut canonical = Frame {
                values: session.values,
                len: session.count,
                borrow: PhantomData,
            };
            assert!(canonical.store(
                unsafe { std::slice::from_raw_parts_mut(slots, session.count) },
                dest as usize,
                value
            ));
        })
    }));
    Some(match result {
        Ok(completed) => {
            let counts = unsafe { session.counts.as_mut() };
            if completed {
                counts.completed += 1;
                counts.table_reads += 1;
                counts.upvalue_reads += 1;
                unsafe {
                    *session.pc.as_ptr() = pc as usize + 1;
                }
                abi::HELPER_COMPLETED
            } else {
                counts.declined += 1;
                unsafe {
                    *session.pc.as_ptr() = pc as usize;
                }
                abi::HELPER_DECLINED
            }
        }
        Err(payload) => {
            unsafe {
                session.frame.as_mut().panic = Some(payload);
            }
            session.stopped = true;
            abi::HELPER_PANICKED
        }
    })
}

unsafe extern "C" fn materialize(
    host: *mut (),
    slots: *mut Payload,
    index: u32,
    tag: u64,
    bits: u64,
) -> u32 {
    let session = unsafe { &mut *host.cast::<Session<'_, '_, '_, '_, '_>>() };
    if session.stopped {
        return abi::HELPER_PANICKED;
    }
    let count = session.count;
    let value = match tag {
        NIL => Some(Value::Nil),
        BOOLEAN if bits <= 1 => Some(Value::Boolean(bits != 0)),
        INTEGER => Some(Value::Integer(bits as i64)),
        NUMBER => Some(Value::Number(f64::from_bits(bits))),
        _ => None,
    };
    if let Some(value) = value.filter(|_| (index as usize) < count) {
        let mut canonical = Frame {
            values: session.values,
            len: count,
            borrow: PhantomData,
        };
        assert!(canonical.store(
            unsafe { std::slice::from_raw_parts_mut(slots, count) },
            index as usize,
            value,
        ));
        abi::HELPER_COMPLETED
    } else {
        abi::HELPER_DECLINED
    }
}

#[test]
fn initialized_prefix_bounds_hold_for_empty_small_and_full_frames() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for count in [0, 1, 7, 255, 256] {
            let mut values = vec![Value::Integer(1); count];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
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
                    if count != 0 {
                        assert!(store(
                            slots.add(count - 1).read(),
                            Slot {
                                tag: INTEGER,
                                bits: 42
                            }
                        ));
                        assert_eq!(
                            call::<{ abi::HELPER_MOVE }>(host, slots, 0, (count - 1) as u32, 0, 7),
                            abi::HELPER_COMPLETED
                        );
                        assert_eq!(load(slots.read()).unwrap().bits, 42);
                    }
                    assert_eq!(
                        call::<{ abi::HELPER_MOVE }>(
                            host,
                            slots,
                            0,
                            count as u32,
                            0,
                            if count == 0 { 7 } else { 8 }
                        ),
                        abi::HELPER_PANICKED
                    );
                    assert_eq!(
                        materialize(host, slots, 0, INTEGER, 99),
                        abi::HELPER_PANICKED
                    );
                });
                assert_eq!(
                    (frame.count.calls, frame.count.completed),
                    if count == 0 { (1, 0) } else { (2, 1) }
                );
                assert!(frame.panic.is_some());
            });
            assert_eq!(pc, if count == 0 { 8 } else { 9 });
            for (index, value) in values.into_iter().enumerate() {
                let expected = if index == 0 || index + 1 == count {
                    42
                } else {
                    1
                };
                assert!(matches!(value, Value::Integer(actual) if actual == expected));
            }
        }
    });
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
                    assert_eq!(
                        materialize(host, slots, 1, INTEGER, 99),
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
