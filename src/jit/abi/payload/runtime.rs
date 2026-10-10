use super::*;
use crate::{
    jit::{abi, helpers},
    thread::LuaRegisters,
};

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
