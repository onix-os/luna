use super::*;

unsafe extern "C" fn entry(
    slots: *mut Payload,
    pc: u64,
    budget: u32,
    exit: *mut abi::Exit,
    host: *mut Bridge,
) {
    assert!(budget <= 64);
    unsafe { (*exit).pc = pc };
    if budget == 0 {
        return;
    }
    unsafe {
        let first = slots.read();
        assert_eq!(first.tag, INTEGER);
        first.pointer.cast::<i64>().write(i64::MIN);
        ((*host).functions.terminal)(host, slots, 0, NUMBER, (-0.0f64).to_bits());
        assert_eq!(slots.read().tag, NIL);
        assert!(slots.read().pointer.is_null());
        assert_eq!(
            ((*host).functions.helpers[(abi::HELPER_MOVE - 1) as usize])(
                host.cast(),
                slots.cast(),
                0,
                0,
                0,
                7,
            ),
            abi::HELPER_COMPLETED
        );
        let refreshed = slots.read();
        assert_eq!(refreshed.tag, NUMBER);
        refreshed
            .pointer
            .cast::<f64>()
            .write(f64::from_bits(0x7ff8000000000042));
    }
}

#[test]
fn bound_entry_preserves_refresh_and_canonical_writes() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for count in [1, 7, 256] {
            for budget in [0, 1, 64, 65, u32::MAX] {
                let mut values = vec![Value::Integer(17); count + 1];
                let mut pc = 5;
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
                    let mut exit = abi::Exit::default();
                    with_binding::<256>(&mut frame, |binding| unsafe {
                        binding.invoke(entry, count, 5, budget, &mut exit);
                    });
                    assert_eq!(exit.pc, 5);
                    assert_eq!(frame.count.calls, u64::from(budget != 0));
                    assert_eq!(frame.count.completed, frame.count.calls);
                    assert!(frame.panic.is_none());
                });
                assert_eq!(pc, if budget == 0 { 5 } else { 8 });
                if budget == 0 {
                    assert!(matches!(values[0], Value::Integer(17)));
                } else {
                    assert!(
                        matches!(values[0], Value::Number(n) if n.to_bits() == 0x7ff8000000000042)
                    );
                }
                assert!(values[1..].iter().all(|v| matches!(v, Value::Integer(17))));
            }
        }
    });
}

#[test]
fn bound_entry_rejects_count_and_capacity_before_entry() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for (count, expected) in [(0, 0), (1, 0), (1, 2), (2, 2), (3, 3)] {
            let mut values = vec![Value::Integer(17); 3];
            let mut pc = 5;
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
                let mut exit = abi::Exit::default();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    with_binding::<2>(&mut frame, |binding| unsafe {
                        binding.invoke(entry, expected, 5, 0, &mut exit);
                    });
                }));
                let admitted = count <= 2 && count == expected;
                assert_eq!(result.is_ok(), admitted);
                assert_eq!(exit.pc, if admitted { 5 } else { 0 });
                assert_eq!(frame.count.calls, 0);
            });
            assert!(values.iter().all(|v| matches!(v, Value::Integer(17))));
        }
    });
}
