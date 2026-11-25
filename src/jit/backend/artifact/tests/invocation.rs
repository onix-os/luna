use super::*;
use crate::{
    jit::{array_window as window, readonly_lookup::binding},
    table::RawTable,
    Value,
};

fn same_exit(a: &Exit, b: &Exit) {
    assert_eq!(
        (a.pc, a.instructions, a.reason),
        (b.pc, b.instructions, b.reason)
    );
}
fn same_slots(a: &[Slot], b: &[Slot]) {
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(b) {
        assert_eq!((a.tag, a.bits), (b.tag, b.bits));
    }
}

pub(in crate::jit) fn exercise(installed: &Installed, source: &Snapshot) -> (u64, u64, u64) {
    let control = selected(source).unwrap();
    let mut calls = 0;
    let mut instructions = 0;
    let mut progressing = 0;
    for module in 0..installed.len() {
        let before = instructions;
        match installed.kind(module) {
            Kind::Ordinary => {
                let entry: Entry = unsafe { std::mem::transmute(installed.entry(module, 0)) };
                for pc in 0..source.operations.len() {
                    for budget in [0, 1, 2, 7, 64, u32::MAX] {
                        let mut a = vec![
                            Slot {
                                tag: abi::INTEGER,
                                bits: 3
                            };
                            source.registers
                        ];
                        let mut b = a.clone();
                        let expected = control.invoke(&mut a, pc, budget);
                        let actual = unsafe {
                            abi::invoke(entry, b.as_mut_ptr(), pc, budget, std::ptr::null_mut())
                        };
                        same_exit(&expected, &actual);
                        same_slots(&a, &b);
                        calls += 1;
                        instructions += u64::from(actual.instructions);
                    }
                }
            }
            Kind::Readonly => {
                let plan = crate::jit::readonly_lookup::Plan::recognize(source).unwrap();
                let original = control.readonly_lookup.as_ref().unwrap();
                let entry: binding::Entry =
                    unsafe { std::mem::transmute(installed.entry(module, 0)) };
                for pc in 0..source.operations.len() {
                    for budget in [0u32, 1, 2, 7, 64, u32::MAX] {
                        let mut a = vec![
                            Slot {
                                tag: abi::INTEGER,
                                bits: 3
                            };
                            source.registers
                        ];
                        let mut b = a.clone();
                        let mut av = binding::View {
                            binding: plan.binding(),
                            slot: Slot {
                                tag: abi::INTEGER,
                                bits: 7,
                            },
                            executed: 0,
                        };
                        let mut bv = binding::View {
                            binding: plan.binding(),
                            slot: av.slot,
                            executed: 0,
                        };
                        let mut ae = Exit::default();
                        let mut be = Exit::default();
                        unsafe {
                            (original.entry)(
                                a.as_mut_ptr(),
                                pc as u64,
                                budget.min(64),
                                &mut ae,
                                std::ptr::null_mut(),
                                &mut av,
                            );
                            entry(
                                b.as_mut_ptr(),
                                pc as u64,
                                budget.min(64),
                                &mut be,
                                std::ptr::null_mut(),
                                &mut bv,
                            );
                        }
                        same_exit(&ae, &be);
                        same_slots(&a, &b);
                        assert_eq!(
                            (av.binding, av.executed, av.slot.tag, av.slot.bits),
                            (bv.binding, bv.executed, bv.slot.tag, bv.slot.bits)
                        );
                        calls += 1;
                        instructions += u64::from(be.instructions);
                    }
                }
            }
            Kind::Arrays => {
                let plans: Vec<_> = (0..source.operations.len())
                    .filter_map(|end| window::Plan::new(source, end))
                    .collect();
                assert_eq!(plans.len(), installed.functions(module));
                crate::Lua::empty().enter(|ctx| {
                    for (index, plan) in plans.into_iter().enumerate() {
                        let entry: window::KernelEntry =
                            unsafe { std::mem::transmute(installed.entry(module, index)) };
                        for pc in plan.start..=plan.end {
                            for budget in [0, 1, 2, 7, 64, u32::MAX] {
                                for controls in [[1, 20, 1, 1], [20, 1, -1, 20], [3, 20, 0, 3]] {
                                    let table = || {
                                        let mut raw = RawTable::with_capacity(&ctx, 32, 0);
                                        for (index, value) in raw.array_mut().iter_mut().enumerate()
                                        {
                                            *value = Value::Integer(index as i64 + 1);
                                        }
                                        crate::Table::from_parts(&ctx, raw, None)
                                    };
                                    let at = table();
                                    let bt = table();
                                    let mut av = vec![Value::Integer(3); source.registers];
                                    let mut bv = av.clone();
                                    av[usize::from(plan.table)] = Value::Table(at);
                                    bv[usize::from(plan.table)] = Value::Table(bt);
                                    for (offset, value) in controls.into_iter().enumerate() {
                                        av[usize::from(plan.base) + offset] = Value::Integer(value);
                                        bv[usize::from(plan.base) + offset] = Value::Integer(value);
                                    }
                                    let mut a: Vec<_> =
                                        av.iter().copied().map(Slot::from_value).collect();
                                    let mut b: Vec<_> =
                                        bv.iter().copied().map(Slot::from_value).collect();
                                    let expected = control
                                        .array_kernels
                                        .as_ref()
                                        .unwrap()
                                        .invoke(ctx, &av, &mut a, pc, budget);
                                    let actual = unsafe {
                                        window::invoke_kernel(
                                            entry, plan, ctx, &bv, &mut b, pc, budget,
                                        )
                                    };
                                    match (expected, actual) {
                                        (None, None) => {}
                                        (Some(a), Some(b)) => {
                                            same_exit(&a.exit, &b.exit);
                                            assert_eq!(a.counts, b.counts);
                                            instructions += u64::from(b.exit.instructions);
                                        }
                                        _ => panic!("array artifact refusal mismatch"),
                                    }
                                    same_slots(&a, &b);
                                    for key in 1..=32 {
                                        let a = Slot::from_value(at.get_value(ctx, key));
                                        let b = Slot::from_value(bt.get_value(ctx, key));
                                        assert_eq!((a.tag, a.bits), (b.tag, b.bits));
                                    }
                                    calls += 1;
                                }
                            }
                        }
                    }
                });
            }
            Kind::Resume => {
                use binding::integer_resume as resume;
                let plan = resume::Plan::recognize(source).unwrap();
                let original = control
                    .readonly_lookup
                    .as_ref()
                    .unwrap()
                    .integer_resume
                    .as_ref()
                    .unwrap()
                    .1
                    .entry;
                let entry: resume::Entry =
                    unsafe { std::mem::transmute(installed.entry(module, 0)) };
                for budget in [0u32, 1, 2, 3, 4, 7, 64, u32::MAX] {
                    for values in [
                        [10, 7, 3, 20, 1, 3],
                        [i64::MAX, 7, i64::MAX, i64::MAX, 1, i64::MAX],
                        [10, 7, 20, 1, -1, 20],
                    ] {
                        let mut a = values;
                        let mut b = values;
                        let mut av = resume::View {
                            cookie: plan.cookie(),
                            cells: a.each_mut().map(std::ptr::from_mut),
                            argument: 11,
                            finish: 123,
                            executed: 0,
                        };
                        let mut bv = resume::View {
                            cookie: plan.cookie(),
                            cells: b.each_mut().map(std::ptr::from_mut),
                            argument: 11,
                            finish: 123,
                            executed: 0,
                        };
                        let mut ae = Exit::default();
                        let mut be = Exit::default();
                        unsafe {
                            original(&mut av, budget.min(64), &mut ae);
                            entry(&mut bv, budget.min(64), &mut be);
                        }
                        same_exit(&ae, &be);
                        assert_eq!(a, b);
                        assert_eq!(
                            (av.argument, av.finish, av.executed),
                            (bv.argument, bv.finish, bv.executed)
                        );
                        calls += 1;
                        instructions += u64::from(be.instructions);
                    }
                }
            }
        }
        if instructions > before {
            progressing |= 1 << (installed.kind(module) as u8 - 1);
        }
    }
    assert!(calls > 0);
    (calls, instructions, progressing)
}

pub(in crate::jit) fn exercise_prepared(installed: &Installed, source: &Snapshot) -> (u64, u64) {
    use crate::{jit::helpers, thread::LuaRegisters};
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return target").unwrap();
        let fresh = Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
        assert_eq!(&*source_packet(source), &*source_packet(&fresh));
        let control = selected(source).unwrap();
        assert_eq!(installed.kind(0), Kind::Ordinary);
        let entry: Entry = unsafe { std::mem::transmute(installed.entry(0, 0)) };
        let mut calls = 0;
        let mut instructions = 0;
        let mut reads = 0;
        for value in [
            Value::Nil,
            Value::Integer(17),
            Value::Number(-0.0),
            Value::Table(crate::Table::new(&ctx)),
        ] {
            for borrowed in [false, true] {
                for budget in [0, 1, 2, 64, u32::MAX] {
                    let observe = |candidate| {
                        ctx.globals().set(ctx, "target", value).unwrap();
                        let mut canonical = vec![Value::Nil; source.registers];
                        let mut pc = 0;
                        LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            |mut registers| {
                                let mut slots: Vec<_> = registers
                                    .stack_frame
                                    .iter()
                                    .copied()
                                    .map(Slot::from_value)
                                    .collect();
                                let mut frame = helpers::Frame {
                                    ctx,
                                    closure,
                                    registers: &mut registers,
                                    count: helpers::Counts::default(),
                                    slot_count: slots.len(),
                                    panic: None,
                                    projection: None,
                                    read_hints: None,
                                };
                                let mut host = abi::Host {
                                    data: std::ptr::from_mut(&mut frame).cast(),
                                    projection: std::ptr::null_mut(),
                                };
                                let globals = ctx.globals().into_inner();
                                let guard = borrowed.then(|| globals.borrow_mut(&ctx));
                                let exit = unsafe {
                                    if candidate {
                                        abi::invoke(entry, slots.as_mut_ptr(), 0, budget, &mut host)
                                    } else {
                                        control.invoke_host(&mut slots, 0, budget, &mut host)
                                    }
                                };
                                drop(guard);
                                let c = frame.count;
                                (
                                    (exit.pc, exit.instructions, exit.reason),
                                    *frame.registers.pc,
                                    [
                                        c.calls,
                                        c.completed,
                                        c.declined,
                                        c.table_reads,
                                        c.table_writes,
                                        c.upvalue_reads,
                                        c.upvalue_writes,
                                        c.allocations,
                                    ],
                                    frame.panic.is_some(),
                                    slots
                                        .iter()
                                        .map(|slot| (slot.tag, slot.bits))
                                        .collect::<Vec<_>>(),
                                )
                            },
                        )
                    };
                    let expected = observe(false);
                    let actual = observe(true);
                    assert_eq!(expected, actual);
                    calls += 1;
                    instructions += u64::from(actual.0 .1);
                    reads += actual.2[3];
                }
            }
        }
        assert!(instructions > 0 && reads > 0);
        (calls, instructions)
    })
}
