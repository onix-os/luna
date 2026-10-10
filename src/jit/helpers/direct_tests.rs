use super::*;

struct Observation<'gc> {
    values: [Value<'gc>; 7],
    table: [Value<'gc>; 3],
    upvalue: Value<'gc>,
    counts: [u64; 8],
    pc: usize,
    fuel: i32,
    result: u32,
    panicked: bool,
}

fn observe<'gc, const KIND: u32>(
    ctx: Context<'gc>,
    closure: Closure<'gc>,
    table: Table<'gc>,
    scenario: usize,
    direct: bool,
) -> Observation<'gc> {
    table.set_readonly(&ctx, false);
    table.set_metatable(ctx, None);
    for key in [1, 2, 42] {
        table
            .set_raw(&ctx, Value::Integer(key), Value::Nil)
            .unwrap();
    }
    if scenario != 3 {
        table
            .set_raw(&ctx, Value::Integer(42), Value::Boolean(true))
            .unwrap();
    } else {
        let meta = Table::new(&ctx);
        meta.set_raw(
            &ctx,
            Value::String(ctx.intern(b"__index")),
            Value::Boolean(true),
        )
        .unwrap();
        meta.set_raw(
            &ctx,
            Value::String(ctx.intern(b"__newindex")),
            Value::Boolean(true),
        )
        .unwrap();
        table.set_metatable(ctx, Some(meta));
    }
    table.set_readonly(&ctx, scenario == 2);
    let mut values = [
        Value::Table(table),
        Value::Integer(0),
        Value::Integer(42),
        Value::Number(f64::from_bits(0x7ff8000000000042)),
        Value::Nil,
        Value::Function(closure.into()),
        Value::Integer(i64::MAX),
    ];
    if scenario == 8 {
        values[0] = Value::Number(-0.0);
    }
    let count = match scenario {
        7 => 3,
        9 => 0,
        _ => values.len(),
    };
    let (mut a, mut b, c) = match KIND {
        abi::HELPER_MOVE => (4, 3, 0),
        abi::HELPER_CONSTANT => (4, 0, 0),
        abi::HELPER_NEW_TABLE => (4, 1, 1),
        abi::HELPER_GET_TABLE => (4, 0, 2),
        abi::HELPER_SET_TABLE => (0, 2, 3),
        abi::HELPER_GET_UP_TABLE => (4, 0, 2),
        abi::HELPER_SET_UP_TABLE => (0, 2, 3),
        abi::HELPER_GET_UPVALUE => (4, 0, 0),
        abi::HELPER_SET_UPVALUE => (0, 3, 0),
        abi::HELPER_SET_LIST => (0, 2, 0),
        _ => unreachable!(),
    };
    if scenario == 5 {
        a = 7;
    }
    if scenario == 6 {
        b = 7;
    }
    let mut pc = 5;
    let mut fuel = crate::Fuel::with(47);
    let (result, panicked, counts, upvalue) = LuaRegisters::with_test_frame_state(
        ctx,
        &mut pc,
        &mut values,
        &mut fuel,
        scenario == 4,
        |mut registers| {
            let upvalue = if scenario == 1 {
                registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0))
            } else {
                crate::closure::UpValue::new(
                    &ctx,
                    crate::closure::UpValueState::Closed(Value::Table(table)),
                )
            };
            closure.set_upvalue(&ctx, 0, upvalue);
            let mut frame = Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: Counts::default(),
                slot_count: count,
                panic: None,
                projection: None,
            };
            let result = if direct {
                call_direct::<KIND>(&mut frame, a, b, c, 11)
            } else {
                let mut slots = vec![Slot::canonical(); count];
                let mut host = abi::Host {
                    data: std::ptr::from_mut(&mut frame).cast(),
                    projection: std::ptr::null_mut(),
                };
                unsafe { call::<KIND>(&mut host, slots.as_mut_ptr(), a, b, c, 11) }
            };
            let counts = [
                frame.count.calls,
                frame.count.completed,
                frame.count.declined,
                frame.count.table_reads,
                frame.count.table_writes,
                frame.count.upvalue_reads,
                frame.count.upvalue_writes,
                frame.count.allocations,
            ];
            (
                result,
                frame.panic.is_some(),
                counts,
                frame.registers.get_upvalue(&ctx, upvalue),
            )
        },
    );
    Observation {
        values,
        table: [1, 2, 42].map(|key| table.get_raw(&ctx, Value::Integer(key))),
        upvalue,
        counts,
        pc,
        fuel: fuel.remaining(),
        result,
        panicked,
    }
}

fn compare<'gc, const KIND: u32>(ctx: Context<'gc>, closure: Closure<'gc>, table: Table<'gc>) {
    for scenario in 0..10 {
        let old = observe::<KIND>(ctx, closure, table, scenario, false);
        let new = observe::<KIND>(ctx, closure, table, scenario, true);
        assert_eq!(
            (new.result, new.panicked, new.pc, new.fuel, new.counts),
            (old.result, old.panicked, old.pc, old.fuel, old.counts),
            "kind {KIND}, scenario {scenario}"
        );
        for (index, (actual, expected)) in new.values.into_iter().zip(old.values).enumerate() {
            if KIND == abi::HELPER_NEW_TABLE && index == 4 && matches!(expected, Value::Table(_)) {
                let (Value::Table(a), Value::Table(b)) = (actual, expected) else {
                    panic!("missing new table")
                };
                assert_ne!(a, b);
                assert_eq!(
                    (a.length(&ctx), a.metatable()),
                    (b.length(&ctx), b.metatable())
                );
            } else {
                super::tests::assert_identical(actual, expected);
            }
        }
        for (actual, expected) in new.table.into_iter().zip(old.table) {
            super::tests::assert_identical(actual, expected);
        }
        super::tests::assert_identical(new.upvalue, old.upvalue);
    }
}

#[test]
fn direct_transport_matches_proxy_helpers_effects_aliases_and_panics() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local n = 42; return _ENV, n").unwrap();
        let table = Table::new(&ctx);
        compare::<{ abi::HELPER_MOVE }>(ctx, closure, table);
        compare::<{ abi::HELPER_CONSTANT }>(ctx, closure, table);
        compare::<{ abi::HELPER_NEW_TABLE }>(ctx, closure, table);
        compare::<{ abi::HELPER_GET_TABLE }>(ctx, closure, table);
        compare::<{ abi::HELPER_SET_TABLE }>(ctx, closure, table);
        compare::<{ abi::HELPER_GET_UP_TABLE }>(ctx, closure, table);
        compare::<{ abi::HELPER_SET_UP_TABLE }>(ctx, closure, table);
        compare::<{ abi::HELPER_GET_UPVALUE }>(ctx, closure, table);
        compare::<{ abi::HELPER_SET_UPVALUE }>(ctx, closure, table);
        compare::<{ abi::HELPER_SET_LIST }>(ctx, closure, table);
    });
}
