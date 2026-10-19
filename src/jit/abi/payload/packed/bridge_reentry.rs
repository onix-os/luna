use super::*;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "bridge_native.rs"]
mod native;

unsafe fn model(slots: *mut Packed, control: *mut (), callee: Helper, args: [u32; 4]) -> u32 {
    unsafe {
        let original = slots.read().load().unwrap().bits;
        assert!(slots.read().store_numeric(Slot {
            tag: INTEGER,
            bits: original.wrapping_add(1)
        }));
        let result = callee(control, args[0], args[1], args[2], args[3]);
        if result == abi::HELPER_COMPLETED {
            let current = slots.add(1).read().load().unwrap().bits;
            assert!(slots.add(1).read().store_numeric(Slot {
                tag: INTEGER,
                bits: current.wrapping_add(1)
            }));
        }
        result
    }
}

fn check(mut entry: impl FnMut(*mut Packed, *mut (), Helper, [u32; 4]) -> u32) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 17").unwrap();
        for case in 0..4 {
            let mut values = [Value::Integer(17), Value::Nil, Value::Integer(73)];
            let mut pc = 5;
            crate::thread::LuaRegisters::with_test_native_frame(
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
                    let expected = [
                        abi::HELPER_COMPLETED,
                        abi::HELPER_DECLINED,
                        abi::HELPER_PANICKED,
                        REFUSED,
                    ][case];
                    let result = with_bridge::<4, _>(&mut frame, |slots, bridge| {
                        if case == 3 {
                            unsafe {
                                (*bridge.control.cast::<FrameScope<'_, '_, '_, '_, '_, 4>>())
                                    .scope
                                    .count = 4
                            };
                        }
                        let callee = bridge.functions.helpers[if case == 1 { 3 } else { 0 }];
                        let result = entry(
                            slots,
                            bridge.control,
                            callee,
                            [if case == 2 { 99 } else { 1 }, 0, 0, 7],
                        );
                        assert_eq!(result, expected);
                        result
                    });
                    assert_eq!(
                        result,
                        if case == 3 {
                            Err(Refusal::Bounds)
                        } else {
                            Ok(expected)
                        }
                    );
                    assert_eq!(frame.count.calls, 1);
                    assert_eq!(frame.count.completed, u64::from(matches!(case, 0 | 3)));
                    assert_eq!(frame.count.declined, u64::from(case == 1));
                    assert_eq!(frame.panic.is_some(), case == 2);
                },
            );
            assert_eq!(pc, if case == 1 { 7 } else { 8 });
            assert!(matches!(values[0], Value::Integer(18)));
            match case {
                0 => assert!(matches!(values[1], Value::Integer(19))),
                3 => assert!(matches!(values[1], Value::Integer(18))),
                _ => assert!(matches!(values[1], Value::Nil)),
            }
            assert!(matches!(values[2], Value::Integer(73)));
        }
    });
}

#[test]
fn packed_c_bridge_reentry_model_reloads_after_success_and_stops_other_statuses() {
    check(|slots, control, callee, args| unsafe { model(slots, control, callee, args) });
}
