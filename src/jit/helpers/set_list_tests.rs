use super::tests::assert_identical;
use super::*;
use crate::{Fuel, Lua};

fn invoke(frame: &mut Frame<'_, '_, '_, '_>, slots: &mut [Slot], count: u32) -> u32 {
    let mut host = abi::Host {
        data: std::ptr::from_mut(frame).cast(),
        projection: std::ptr::null_mut(),
    };
    unsafe { call::<{ abi::HELPER_SET_LIST }>(&mut host, slots.as_mut_ptr(), 0, count, 0, 7) }
}

#[test]
fn pending_values_overflow_and_zero_count_preserve_index_and_fuel() {
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        let reference = Value::Table(Table::new(&ctx));
        for start in [i64::MIN, -1, 0, i64::MAX - 1, i64::MAX] {
            for count in 0..=2 {
                let table = Table::new(&ctx);
                let mut canonical = [
                    Value::Table(table),
                    Value::Integer(-99),
                    Value::Nil,
                    reference,
                ];
                let mut slots = canonical.map(Slot::from_value);
                slots[1] = Slot::from_value(Value::Integer(start));
                slots[2] = Slot::from_value(Value::Number(-0.0));
                let mut pc = 7;
                let mut fuel = Fuel::with(47);
                LuaRegisters::with_test_frame_state(
                    ctx,
                    &mut pc,
                    &mut canonical,
                    &mut fuel,
                    false,
                    |mut registers| {
                        let mut frame = Frame {
                            ctx,
                            closure,
                            registers: &mut registers,
                            count: Counts::default(),
                            slot_count: slots.len(),
                            panic: None,
                            projection: None,
                        };
                        assert_eq!(invoke(&mut frame, &mut slots, count), abi::HELPER_COMPLETED);
                        let mut index = start;
                        let mut writes = 0;
                        for offset in 0..count {
                            let Some(next) = index.checked_add(1) else {
                                break;
                            };
                            index = next;
                            let value = table.get_raw(&ctx, index.into());
                            if offset == 0 {
                                let Value::Number(value) = value else {
                                    panic!("missing numeric value");
                                };
                                assert_eq!(value.to_bits(), (-0.0f64).to_bits());
                            } else {
                                assert_identical(value, reference);
                            }
                            writes += 1;
                        }
                        assert_identical(frame.registers.stack_frame[1], Value::Integer(index));
                        assert_eq!((slots[1].tag, slots[1].bits), (abi::INTEGER, index as u64));
                        assert_eq!(frame.count.table_writes, writes);
                        assert_eq!(
                            (
                                frame.count.calls,
                                frame.count.completed,
                                frame.count.declined
                            ),
                            (1, 1, 0)
                        );
                        assert!(frame.panic.is_none());
                    },
                );
                assert_eq!(pc, 8);
                assert_eq!(fuel.remaining(), 47 - 4 - count as i32);
            }
        }
    });
    lua.gc_collect();
}

#[test]
fn invalid_types_and_variable_stack_decline_without_fuel_or_effects() {
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        for case in 0..3 {
            let table = Table::new(&ctx);
            let mut canonical = [
                if case == 0 {
                    Value::Nil
                } else {
                    Value::Table(table)
                },
                if case == 1 {
                    Value::Number(0.0)
                } else {
                    Value::Integer(0)
                },
                Value::Integer(42),
            ];
            let before = canonical;
            let mut slots = canonical.map(Slot::from_value);
            let mut pc = 7;
            let mut fuel = Fuel::with(-20);
            LuaRegisters::with_test_frame_state(
                ctx,
                &mut pc,
                &mut canonical,
                &mut fuel,
                case == 2,
                |mut registers| {
                    let mut frame = Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: Counts::default(),
                        slot_count: slots.len(),
                        panic: None,
                        projection: None,
                    };
                    assert_eq!(invoke(&mut frame, &mut slots, 1), abi::HELPER_DECLINED);
                    assert_eq!(
                        (
                            frame.count.calls,
                            frame.count.completed,
                            frame.count.declined
                        ),
                        (1, 0, 1)
                    );
                    assert_eq!(frame.count.table_writes, 0);
                    assert!(frame.panic.is_none());
                },
            );
            assert_eq!(pc, 7);
            assert_eq!(fuel.remaining(), -20);
            for (actual, expected) in canonical.into_iter().zip(before) {
                assert_identical(actual, expected);
            }
            assert!(table.get_raw(&ctx, Value::Integer(1)).is_nil());
        }
    });
}

#[test]
fn readonly_panic_keeps_pending_scalars_and_charges_attempted_list_fuel() {
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        let table = Table::new(&ctx);
        table.set_readonly(&ctx, true);
        let mut canonical = [Value::Table(table), Value::Integer(-99), Value::Integer(-1)];
        let mut slots = canonical.map(Slot::from_value);
        slots[1] = Slot::from_value(Value::Integer(0));
        slots[2] = Slot::from_value(Value::Integer(42));
        let mut pc = 7;
        let mut fuel = Fuel::with(47);
        LuaRegisters::with_test_frame_state(
            ctx,
            &mut pc,
            &mut canonical,
            &mut fuel,
            false,
            |mut registers| {
                let mut frame = Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: Counts::default(),
                    slot_count: slots.len(),
                    panic: None,
                    projection: None,
                };
                assert_eq!(invoke(&mut frame, &mut slots, 1), abi::HELPER_PANICKED);
                assert!(frame.panic.take().is_some());
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.declined,
                        frame.count.table_writes
                    ),
                    (1, 0, 0, 0)
                );
            },
        );
        assert_eq!(pc, 8);
        assert_eq!(fuel.remaining(), 42);
        assert_identical(canonical[1], Value::Integer(0));
        assert_identical(canonical[2], Value::Integer(42));
        assert!(table.get_raw(&ctx, Value::Integer(1)).is_nil());
    });
}

#[test]
fn partial_writes_are_not_replayed_after_a_helper_panic() {
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 42").unwrap();
        let table = Table::new(&ctx);
        let mut canonical = [Value::Table(table), Value::Integer(-99), Value::Integer(-1)];
        let mut slots = canonical.map(Slot::from_value);
        slots[1] = Slot::from_value(Value::Integer(0));
        slots[2] = Slot::from_value(Value::Integer(42));
        let mut pc = 7;
        let mut fuel = Fuel::with(47);
        LuaRegisters::with_test_frame_state(
            ctx,
            &mut pc,
            &mut canonical,
            &mut fuel,
            false,
            |mut registers| {
                let mut frame = Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: Counts::default(),
                    slot_count: slots.len(),
                    panic: None,
                    projection: None,
                };
                assert_eq!(invoke(&mut frame, &mut slots, 2), abi::HELPER_PANICKED);
                assert!(frame.panic.take().is_some());
                assert_eq!(
                    (
                        frame.count.completed,
                        frame.count.declined,
                        frame.count.table_writes
                    ),
                    (0, 0, 1)
                );
            },
        );
        assert_eq!(pc, 8);
        assert_eq!(fuel.remaining(), 41);
        assert_identical(canonical[1], Value::Integer(0));
        assert_identical(table.get_raw(&ctx, Value::Integer(1)), Value::Integer(42));
        assert!(table.get_raw(&ctx, Value::Integer(2)).is_nil());
    });
}
