use super::*;
use crate::thread::LuaRegisters;

#[test]
fn strided_helpers_retire_on_panic_and_refuse_reentry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let mut pc = 5;
        let mut values = [Value::Integer(17); 4];
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 4,
                panic: None,
                read_hints: None,
                projection: None,
            };
            with_session(
                &mut frame,
                Mask::new(&[1]).unwrap(),
                |header, data| unsafe {
                    assert_eq!(rust_entry::<1, 1>(header, 1), 1);
                    assert_eq!(
                        helper::<{ abi::HELPER_MOVE }>(header, data, 1, u32::MAX, 0, 31),
                        abi::HELPER_PANICKED
                    );
                    assert!((*header).base.is_null());
                    assert_eq!(rust_entry::<1, 1>(header, 2), 0);
                    assert_eq!(
                        helper::<{ abi::HELPER_MOVE }>(header, data, 1, 2, 0, 99),
                        abi::HELPER_PANICKED
                    );
                },
            );
            assert_eq!(frame.count.calls, 1);
            assert_eq!(frame.count.completed, 0);
            assert!(frame.panic.is_some());
            let result = probe::<{ abi::HELPER_MOVE }>(&mut frame, [1, 2, 0, 100], false);
            assert_eq!(result, abi::HELPER_PANICKED);
            assert_eq!(frame.count.calls, 1);
        });
        assert_eq!(pc, 32);
        crate::jit::abi::tests::assert_identical(values[1], Value::Integer(18));
    });
}

fn transitions(native: bool) {
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let code = native.then(|| super::native::compile(helper::<{ abi::HELPER_MOVE }>));
    let entry: Transition = rust_transition::<{ abi::HELPER_MOVE }>;
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let entry = code.as_ref().map_or(entry, |code| code.entry);
    #[cfg(miri)]
    assert!(!native);
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(true),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_1234_5678_9abc)),
            Value::String(ctx.intern(b"strided helper")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
            Value::Integer(i64::MAX),
            Value::Integer(i64::MIN),
        ];
        for value in kinds {
            let mut values = [Value::Nil, Value::Integer(17), value, Value::Integer(73)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: 4,
                    panic: None,
                    read_hints: None,
                    projection: None,
                };
                with_session(
                    &mut frame,
                    Mask::new(&[1]).unwrap(),
                    |header, data| unsafe {
                        let ready = if matches!(value, Value::Integer(_)) {
                            READY
                        } else {
                            0
                        };
                        assert_eq!(
                            entry(header, data, 1, 2, 0, 7),
                            abi::HELPER_COMPLETED | ready
                        );
                        assert_eq!((*header).base.is_null(), ready == 0);
                        if let Value::Integer(value) = value {
                            let current = (*header).base.add((*header).stride).cast::<i64>().read();
                            assert_eq!(current, value.wrapping_add(2));
                        }
                        assert_eq!(
                            entry(header, data, 1, 3, 0, 9),
                            abi::HELPER_COMPLETED | READY
                        );
                    },
                );
                crate::jit::abi::tests::assert_identical(
                    frame.registers.stack_frame[1],
                    Value::Integer(75),
                );
                assert_eq!((frame.count.calls, frame.count.completed), (2, 2));
                assert!(frame.panic.is_none());
            });
            assert_eq!(pc, 10);
            crate::jit::abi::tests::assert_identical(values[2], value);
            crate::jit::abi::tests::assert_identical(values[3], Value::Integer(73));
        }
    });
}

#[test]
fn strided_helpers_retire_on_decline_without_reexecuting() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let mut values = [Value::Integer(17); 4];
        let mut pc = 5;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: 4,
                panic: None,
                read_hints: None,
                projection: None,
            };
            with_session(
                &mut frame,
                Mask::new(&[1]).unwrap(),
                |header, data| unsafe {
                    assert_eq!(
                        rust_transition::<{ abi::HELPER_GET_TABLE }>(header, data, 1, 0, 2, 7),
                        abi::HELPER_DECLINED
                    );
                    assert!((*header).base.is_null());
                    assert_eq!(rust_entry::<1, 1>(header, 2), 0);
                    assert_eq!(
                        helper::<{ abi::HELPER_MOVE }>(header, data, 1, 2, 0, 99),
                        abi::HELPER_DECLINED
                    );
                },
            );
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.completed,
                    frame.count.declined
                ),
                (1, 0, 1)
            );
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, 7);
        crate::jit::abi::tests::assert_identical(values[1], Value::Integer(18));
    });
}

#[test]
fn strided_helpers_rebind_all_kinds_and_restore_integer_access() {
    transitions(false);
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn strided_helpers_native_rebind_all_kinds() {
    transitions(true);
}
