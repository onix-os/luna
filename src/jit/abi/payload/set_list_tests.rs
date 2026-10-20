use super::*;
use closed_table_tests::{identical, siblings};
use table_write_tests::counts;

fn run<'gc>(
    ctx: crate::Context<'gc>,
    closure: crate::Closure<'gc>,
    values: &mut [Value<'gc>],
    base: u32,
    count: u32,
    variable: bool,
    direct: bool,
    preserve: bool,
) -> (u32, [u64; 8], usize, i32, bool, bool) {
    let mut pc = 7;
    let mut fuel = crate::Fuel::with(67);
    fuel.interrupt();
    let width = values.len();
    let (result, counts, panicked) = LuaRegisters::with_test_frame_state(
        ctx,
        &mut pc,
        values,
        &mut fuel,
        variable,
        |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: width,
                panic: None,
                read_hints: None,
                projection: None,
            };
            let mut result = 0;
            if direct {
                result = helpers::call_direct::<{ abi::HELPER_SET_LIST }>(
                    &mut frame,
                    base,
                    count,
                    u32::MAX,
                    17,
                );
            } else {
                with_session(&mut frame, |host, slots| unsafe {
                    let saved: Vec<_> = (0..width).map(|i| slots.add(i).read()).collect();
                    result =
                        call::<{ abi::HELPER_SET_LIST }>(host, slots, base, count, u32::MAX, 17);
                    if preserve && result != abi::HELPER_PANICKED {
                        siblings(slots, &saved, Some(base as usize + 1));
                    }
                    if result == abi::HELPER_PANICKED {
                        assert_eq!(
                            call::<{ abi::HELPER_SET_LIST }>(host, slots, 0, 0, 0, 19),
                            abi::HELPER_PANICKED
                        );
                    }
                });
            }
            (result, counts(frame.count), frame.panic.is_some())
        },
    );
    (
        result,
        counts,
        pc,
        fuel.remaining(),
        fuel.is_interrupted(),
        panicked,
    )
}

#[test]
fn fixed_lists_preserve_values_and_sibling_loans() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return {}").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"item")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
        ];
        for width in [3, 7, 256] {
            for base in [0, width - 3] {
                for index in [i64::MIN, -1, 0, i64::MAX - 1, i64::MAX] {
                    for value in kinds {
                        let table = crate::Table::new(&ctx);
                        let mut values: Vec<_> =
                            (0..width).map(|i| kinds[i % kinds.len()]).collect();
                        values[base] = Value::Table(table);
                        values[base + 1] = Value::Integer(index);
                        values[base + 2] = value;
                        let before = values.clone();
                        let result = run(
                            ctx,
                            closure,
                            &mut values,
                            base as u32,
                            1,
                            false,
                            false,
                            true,
                        );
                        let writes = u64::from(index != i64::MAX);
                        assert_eq!(result.0, abi::HELPER_COMPLETED);
                        assert_eq!(result.1, [1, 1, 0, 0, writes, 0, 0, 0]);
                        assert_eq!(result.2, 18);
                        assert!(result.4 && !result.5);
                        let expected = index.saturating_add(1);
                        identical(values[base + 1], Value::Integer(expected));
                        if writes != 0 {
                            identical(table.get_raw(&ctx, Value::Integer(expected)), value);
                        }
                        for (i, old) in before.into_iter().enumerate() {
                            if i != base + 1 {
                                identical(values[i], old);
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn list_declines_panics_overflow_fuel_and_partial_effects_match_direct_helper() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return {}").unwrap();
        for (width, base, count) in [
            (0, 0, 0), (1, 0, 0), (2, 0, 0), (2, 0, 1),
            (3, 0, 2), (7, 0, 4), (7, 5, 0), (7, 5, 1),
            (7, u32::MAX, 1), (7, 0, u32::MAX),
        ] {
            for index in [Value::Integer(0), Value::Integer(i64::MAX - 1),
                Value::Integer(i64::MAX), Value::Number(0.0), Value::Nil] {
                for variable in [false, true] {
                    for readonly in [false, true] {
                        let mut outcomes = Vec::new();
                        let mut tables = Vec::new();
                        let mut frames = Vec::new();
                        for direct in [true, false] {
                            let table = crate::Table::new(&ctx);
                            table.set_readonly(&ctx, readonly);
                            let mut values = vec![Value::Integer(41); width];
                            if (base as usize) < width { values[base as usize] = Value::Table(table); }
                            if let Some(dest) = (base as usize).checked_add(1).filter(|i| *i < width) { values[dest] = index; }
                            outcomes.push(run(ctx, closure, &mut values, base, count, variable, direct, false));
                            tables.push(table);
                            frames.push(values);
                        }
                        assert_eq!(outcomes[0], outcomes[1], "width={width} base={base} count={count} variable={variable} readonly={readonly}");
                        for i in 0..width {
                            if i != base as usize { identical(frames[0][i], frames[1][i]); }
                        }
                        for key in [1, 2, 3, 4, i64::MAX] {
                            identical(tables[0].get_raw(&ctx, Value::Integer(key)), tables[1].get_raw(&ctx, Value::Integer(key)));
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn lists_read_terminal_values_and_reenter_after_decline() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return {}").unwrap();
        let table = crate::Table::new(&ctx);
        let mut values = [Value::Table(table), Value::Integer(0), Value::Integer(1)];
        let (c, pc, panic) =
            table_write_tests::scope(ctx, closure, &mut values, |host, slots| unsafe {
                assert_eq!(
                    materialize_terminal(host, slots, 1, INTEGER, i64::MAX as u64),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    materialize_terminal(host, slots, 2, NUMBER, (-0.0f64).to_bits()),
                    abi::HELPER_COMPLETED
                );
                let saved: Vec<_> = (0..3).map(|i| slots.add(i).read()).collect();
                assert_eq!(
                    call::<{ abi::HELPER_SET_LIST }>(host, slots, 0, 1, 0, 7),
                    abi::HELPER_COMPLETED
                );
                siblings(slots, &saved, Some(1));
                assert_eq!(
                    materialize(host, slots, 1, BOOLEAN, 1),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    call::<{ abi::HELPER_SET_LIST }>(host, slots, 0, 1, 0, 8),
                    abi::HELPER_DECLINED
                );
                assert_eq!(
                    materialize(host, slots, 1, INTEGER, 0),
                    abi::HELPER_COMPLETED
                );
                assert_eq!(
                    call::<{ abi::HELPER_SET_LIST }>(host, slots, 0, 1, 0, 9),
                    abi::HELPER_COMPLETED
                );
            });
        assert_eq!(counts(c), [3, 2, 1, 0, 1, 0, 0, 0]);
        assert_eq!(pc, 10);
        assert!(!panic);
        identical(table.get_raw(&ctx, Value::Integer(1)), Value::Number(-0.0));
    });
}
