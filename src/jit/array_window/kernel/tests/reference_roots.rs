use super::*;

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Table(actual), Value::Table(expected)) => assert_eq!(actual, expected),
        (Value::String(actual), Value::String(expected)) => {
            assert!(ottavino_gc_arena::Gc::ptr_eq(
                actual.into_inner(),
                expected.into_inner()
            ));
        }
        (actual, expected) => {
            let (actual, expected) = (Slot::from_value(actual), Slot::from_value(expected));
            assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
        }
    }
}

#[test]
fn native_writes_preserve_reference_cells_at_every_pc_and_budget() {
    let mut lua = Lua::empty();
    let retained = lua.enter(|ctx| {
        let closure =
            Closure::load(ctx, None, b"local t={} for i=1,100 do t[i]=i end return t").unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
        let plan = (0..source.operations.len())
            .find_map(|end| Plan::new(&source, end))
            .unwrap();
        let generic = backend::compile(
            &source,
            MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX)),
            1024 * 1024,
        )
        .unwrap();
        let child = Table::new(&ctx);
        child.set(ctx, "marker", 99).unwrap();
        let string = crate::String::from_slice(&ctx, b"native-array-root");
        let kinds = [
            Value::Table(child),
            Value::String(string),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            Value::Boolean(true),
        ];
        let initial: [Value<'_>; 80] = std::array::from_fn(|index| kinds[index % kinds.len()]);
        let mut retained = Vec::new();
        let mut comparisons = 0;
        with_kernel(&source, plan, |entry| {
            for pc in plan.start..=plan.end {
                for budget in (0..=65).chain([u32::MAX]) {
                    let native_table = array(ctx, &initial);
                    let control_table = array(ctx, &initial);
                    let mut canonical = vec![Value::Integer(3); source.registers];
                    canonical[usize::from(plan.table)] = Value::Table(native_table);
                    for (offset, value) in [1, 65, 1, 1].into_iter().enumerate() {
                        canonical[usize::from(plan.base) + offset] = Value::Integer(value);
                    }
                    let mut native_slots: Vec<_> =
                        canonical.iter().copied().map(Slot::from_value).collect();
                    let mut control_slots = native_slots.clone();
                    let outcome = unsafe {
                        crate::jit::array_window::invoke::invoke(
                            entry,
                            plan,
                            ctx,
                            &canonical,
                            &mut native_slots,
                            pc,
                            budget,
                        )
                    };
                    if budget == 0 {
                        assert!(outcome.is_none());
                        for (index, value) in initial.iter().copied().enumerate() {
                            identical(native_table.get_value(ctx, index as i64 + 1), value);
                        }
                        continue;
                    }
                    let outcome = outcome.unwrap();
                    assert!(
                        outcome.exit.instructions > 0
                            && outcome.exit.instructions <= budget.min(64)
                    );
                    assert_eq!(outcome.counts.reads, 0);
                    canonical[usize::from(plan.table)] = Value::Table(control_table);
                    let mut frame_pc = pc;
                    LuaRegisters::with_test_frame(
                        ctx,
                        &mut frame_pc,
                        &mut canonical,
                        |mut registers| {
                            let mut frame = helpers::Frame {
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: helpers::Counts::default(),
                                slot_count: source.registers,
                                panic: None,
                                projection: None,
                                read_hints: None,
                            };
                            let mut host = abi::Host {
                                data: (&mut frame as *mut helpers::Frame<'_, '_, '_, '_>).cast(),
                                projection: std::ptr::null_mut(),
                            };
                            let expected = unsafe {
                                generic.invoke_host(
                                    &mut control_slots,
                                    pc,
                                    outcome.exit.instructions,
                                    &mut host,
                                )
                            };
                            assert_eq!(
                                (outcome.exit.pc, outcome.exit.instructions),
                                (expected.pc, expected.instructions)
                            );
                            assert_eq!(frame.count.table_reads, 0);
                            assert_eq!(frame.count.table_writes, u64::from(outcome.counts.writes));
                            assert!(frame.panic.is_none());
                        },
                    );
                    same(&native_slots, &control_slots);
                    for key in 1..=80 {
                        identical(
                            native_table.get_value(ctx, key),
                            control_table.get_value(ctx, key),
                        );
                    }
                    retained.push(ctx.stash(native_table));
                    comparisons += 1;
                }
            }
        });
        assert_eq!(comparisons, (plan.end - plan.start + 1) * 66);
        eprintln!("native_array_reference_comparisons={comparisons}");
        retained
    });
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        let first = ctx.fetch(&retained[0]);
        let Value::Table(child) = first.get_value(ctx, 65) else {
            panic!("table root lost");
        };
        let Value::String(string) = first.get_value(ctx, 66) else {
            panic!("string root lost");
        };
        assert_eq!(string.as_bytes(), b"native-array-root");
        identical(child.get_value(ctx, "marker"), Value::Integer(99));
        for table in &retained {
            let table = ctx.fetch(table);
            identical(table.get_value(ctx, 65), Value::Table(child));
            identical(table.get_value(ctx, 66), Value::String(string));
        }
    });
}
