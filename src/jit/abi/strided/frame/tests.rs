use super::*;
use crate::{jit::abi::tests::assert_identical, thread::LuaRegisters};

unsafe fn read(header: *mut Header, index: usize) -> abi::Slot {
    let header = unsafe { &*header };
    let tag = header.tags[index] as u64;
    let pointer = unsafe { header.pointer(index) };
    let bits = match tag {
        abi::BOOLEAN => unsafe { pointer.read() as u64 },
        abi::INTEGER | abi::NUMBER => unsafe { pointer.cast::<u64>().read() },
        _ => {
            assert!(pointer.is_null());
            0
        }
    };
    abi::Slot { tag, bits }
}

#[test]
fn strided_frame_all_kinds_survive_helpers_materialization_and_neighbors() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 1").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::Number(f64::INFINITY),
            Value::String(ctx.intern(b"strided frame")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ];
        for initial in kinds {
            for next in kinds {
                let mut values = [initial, next, Value::Integer(41)];
                let mut pc = 0;
                LuaRegisters::with_test_native_frame(
                    ctx,
                    &mut pc,
                    &mut values,
                    |mut registers, mut _list| {
                        let mut frame = helpers::Frame {
                            list: Some(_list.reborrow()),
                            ctx,
                            closure,
                            registers: &mut registers,
                            count: helpers::Counts::default(),
                            slot_count: 3,
                            panic: None,
                            projection: None,
                        };
                        with_frame(&mut frame, false, |header, host| unsafe {
                            let expected = abi::Slot::from_value(initial);
                            let actual = read(header, 0);
                            assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                            (*header).pointer(2).cast::<i64>().write(43);
                            let helper = (*host).functions.helpers[(abi::HELPER_MOVE - 1) as usize];
                            assert_eq!(
                                helper(host.cast(), header.cast(), 0, 1, 0, 9),
                                abi::HELPER_COMPLETED
                            );
                            let expected = abi::Slot::from_value(next);
                            let actual = read(header, 0);
                            assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                            assert_eq!(read(header, 2).bits, 43);
                            ((*host).functions.copy)(host, header, 0, 1, 0);
                            for (tag, bits) in [
                                (abi::BOOLEAN, 1),
                                (abi::NUMBER, 0x8000000000000000),
                                (abi::INTEGER, i64::MIN as u64),
                                (abi::NIL, 0),
                            ] {
                                ((*host).functions.write)(host, header, 0, tag, bits);
                                let actual = read(header, 0);
                                assert_eq!((actual.tag, actual.bits), (tag, bits));
                            }
                        });
                        assert_eq!(frame.count.calls, 1);
                        assert_eq!(frame.count.completed, 1);
                        assert!(frame.panic.is_none());
                        assert_identical(frame.registers.stack_frame[1], next);
                    },
                );
                assert_eq!(pc, 10);
                assert_identical(values[0], Value::Nil);
                assert_identical(values[2], Value::Integer(43));
            }
        }
    });
}

#[test]
fn strided_frame_full_width_and_retirement() {
    for width in [0, 1, 7, 16, 63, 64, 65, 255, 256] {
        let mut values: Vec<_> = (0..width)
            .map(|i| match i % 4 {
                0 => Value::Integer(i as i64),
                1 => Value::Number(-0.0),
                2 => Value::Boolean(i % 2 != 0),
                _ => Value::Nil,
            })
            .collect();
        let mut header = Header::empty();
        header.bind(&mut values);
        for i in 0..width {
            let slot = unsafe { read(&mut header, i) };
            let expected = match i % 4 {
                0 => (abi::INTEGER, i as u64),
                1 => (abi::NUMBER, 1 << 63),
                2 => (abi::BOOLEAN, u64::from(i % 2 != 0)),
                _ => (abi::NIL, 0),
            };
            assert_eq!((slot.tag, slot.bits), expected);
        }
        header.retire();
        for i in 0..width {
            assert!(unsafe { header.pointer(i) }.is_null());
        }
    }
}

#[test]
fn strided_frame_layout_matches_each_live_scalar_field() {
    let layout = Header::empty().offsets;
    for width in [1, 7, 16, 64, 256] {
        let mut values: Vec<_> = (0..width)
            .map(|i| match i % 8 {
                0 => Value::Boolean(false),
                1 => Value::Boolean(true),
                2 => Value::Integer(i64::MIN),
                3 => Value::Integer(i64::MAX),
                4 => Value::Number(-0.0),
                5 => Value::Number(f64::INFINITY),
                6 => Value::Number(f64::from_bits(0x7ff8000000000042)),
                _ => Value::Integer(0),
            })
            .collect();
        let expected: Vec<_> = values
            .iter()
            .map(|value| {
                let (tag, field) = match value {
                    Value::Boolean(v) => (abi::BOOLEAN, ptr::from_ref(v).cast::<u8>()),
                    Value::Integer(v) => (abi::INTEGER, ptr::from_ref(v).cast::<u8>()),
                    Value::Number(v) => (abi::NUMBER, ptr::from_ref(v).cast::<u8>()),
                    _ => unreachable!(),
                };
                assert_eq!(
                    layout[tag as usize],
                    field.addr() - ptr::from_ref(value).addr()
                );
                (field.addr(), abi::Slot::from_value(*value))
            })
            .collect();
        let mut header = Header::empty();
        header.bind(&mut values);
        assert_eq!(header.offsets, layout);
        for (index, (address, expected)) in expected.into_iter().enumerate() {
            unsafe {
                assert_eq!(header.pointer(index).addr(), address);
                let actual = read(&mut header, index);
                assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
            }
        }
    }
}

#[test]
fn strided_frame_tracks_open_alias_kinds_inside_and_outside_prefix() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return _ENV").unwrap();
        for next in [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
        ] {
            for destination in 0..4 {
                let source = if destination == 2 { 1 } else { 2 };
                let mut values = [Value::Integer(17); 4];
                values[source] = next;
                let mut expected = values;
                expected[destination] = next;
                let mut pc = 0;
                LuaRegisters::with_test_native_frame(
                    ctx,
                    &mut pc,
                    &mut values,
                    |mut registers, mut _list| {
                        let alias = registers.open_test_upvalue(
                            &ctx,
                            crate::types::RegisterIndex(destination as u8),
                        );
                        closure.set_upvalue(&ctx, 0, alias);
                        let mut frame = helpers::Frame {
                            list: Some(_list.reborrow()),
                            ctx,
                            closure,
                            registers: &mut registers,
                            count: helpers::Counts::default(),
                            slot_count: 3,
                            panic: None,
                            projection: None,
                        };
                        with_frame(&mut frame, false, |header, host| unsafe {
                            assert_eq!(
                                helper::<{ abi::HELPER_SET_UPVALUE }, false>(
                                    host.cast(),
                                    header.cast(),
                                    0,
                                    source as u32,
                                    0,
                                    11
                                ),
                                abi::HELPER_COMPLETED
                            );
                            for (i, value) in expected[..3].iter().copied().enumerate() {
                                let actual = read(header, i);
                                let expected = abi::Slot::from_value(value);
                                assert_eq!(
                                    (actual.tag, actual.bits),
                                    (expected.tag, expected.bits)
                                );
                            }
                        });
                        assert_eq!(frame.count.upvalue_writes, 1);
                        for (actual, expected) in
                            frame.registers.stack_frame.iter().copied().zip(expected)
                        {
                            assert_identical(actual, expected);
                        }
                    },
                );
                assert_eq!(pc, 12);
            }
        }
    });
}
