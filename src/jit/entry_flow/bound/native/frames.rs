use super::*;
use crate::{jit::helpers, thread::LuaRegisters, Closure, Table, Value};

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::String(a), Value::String(b)) => assert!(ottavino_gc_arena::Gc::ptr_eq(
            a.into_inner(),
            b.into_inner()
        )),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (a, b) => {
            let a = Slot::from_value(a);
            let b = Slot::from_value(b);
            assert_ne!(a.tag, abi::REFERENCE);
            assert_eq!((a.tag, a.bits), (b.tag, b.bits));
        }
    }
}

#[test]
fn live_helpers_preserve_rebinding_weak_modes_and_panic_exits() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"host_increment(1); return target").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let bound = snapshot
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Call { .. }))
            .unwrap()
            + 1;
        assert!(matches!(
            snapshot.operations[bound],
            Operation::GetUpTable { .. }
        ));
        assert!(snapshot.prepared_keys.is_some());
        let codes = [
            Failure::None,
            Failure::RequireBoundEntry(bound as u32),
            Failure::RequireBoundBudget(bound as u32, 0),
            Failure::RequireBoundBudget(bound as u32, 1),
            Failure::RequireBoundBudget(bound as u32, 2),
            Failure::RequireBoundBudget(bound as u32, 3),
            Failure::RequireBoundBudget(bound as u32, 63),
            Failure::RequireBoundBudget(bound as u32, 64),
        ]
        .map(|mode| compile(&snapshot, mode));
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_4321)),
            Value::String(ctx.intern(b"reference")),
            Value::Table(Table::new(&ctx)),
        ];
        let mut seen = [false; 3];
        for mode in ["", "k", "v", "kv"] {
            let environment = Table::new(&ctx);
            let replacement = Table::new(&ctx);
            replacement.set(ctx, "target", -19).unwrap();
            for case in 0..5 {
                let mt = Table::new(&ctx);
                mt.set(ctx, "__mode", mode).unwrap();
                if case == 1 {
                    mt.set(ctx, "__index", replacement).unwrap();
                }
                environment.set_metatable(ctx, Some(mt));
                let binding = match case {
                    3 => Value::Nil,
                    4 => Value::Table(replacement),
                    _ => Value::Table(environment),
                };
                closure.upvalues()[0]
                    .get()
                    .set(&ctx, crate::closure::UpValueState::Closed(binding));
                for value in values {
                    environment.set(ctx, "target", value).unwrap();
                    for budget in [0u32, 1, 2, 3, 63, 64, u32::MAX] {
                        let specialized = [0, 1, 2, 3, 63, 64]
                            .iter()
                            .position(|&value| value == budget.min(64))
                            .unwrap()
                            + 2;
                        let reports = [&codes[0], &codes[1], &codes[specialized]].map(|code| {
                            let mut canonical = vec![value; snapshot.registers];
                            let mut frame_pc = bound;
                            let report = LuaRegisters::with_test_frame(
                                ctx,
                                &mut frame_pc,
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
                                    };
                                    let mut host = abi::Host {
                                        data: std::ptr::from_mut(&mut frame).cast(),
                                        projection: std::ptr::null_mut(),
                                    };
                                    let table = environment.into_inner();
                                    let borrow = (case == 2).then(|| table.borrow_mut(&ctx));
                                    let exit = unsafe {
                                        code.invoke_host(&mut slots, bound, budget, &mut host)
                                    };
                                    drop(borrow);
                                    let c = frame.count;
                                    if budget > 0 {
                                        if case == 2 {
                                            assert_eq!(exit.reason, Kind::Panic as u32);
                                            assert!(frame.panic.is_some());
                                            assert_eq!(*frame.registers.pc, bound + 1);
                                        } else if case == 3 || case == 1 && value.is_nil() {
                                            assert_eq!(
                                                (exit.pc, exit.instructions, exit.reason),
                                                (bound as u64, 0, Kind::Interpreter as u32)
                                            );
                                        } else {
                                            assert!(c.table_reads > 0);
                                            let Operation::GetUpTable { dest, .. } =
                                                snapshot.operations[bound]
                                            else {
                                                unreachable!()
                                            };
                                            identical(
                                                frame.registers.stack_frame[dest.0 as usize],
                                                if case == 4 {
                                                    Value::Integer(-19)
                                                } else {
                                                    value
                                                },
                                            );
                                        }
                                    }
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
                                        slots.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>(),
                                    )
                                },
                            );
                            (report, canonical)
                        });
                        for report in &reports[1..] {
                            assert_eq!(
                                reports[0].0, report.0,
                                "mode={mode} case={case} budget={budget}"
                            );
                            for (&actual, &expected) in report.1.iter().zip(&reports[0].1) {
                                identical(actual, expected);
                            }
                        }
                        seen[0] |= reports[1].0 .2[3] > 0;
                        seen[1] |= reports[1].0 .2[2] > 0;
                        seen[2] |= reports[1].0 .3;
                    }
                }
            }
        }
        assert_eq!(seen, [true; 3]);
    });
}
