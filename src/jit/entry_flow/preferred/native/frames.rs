use super::*;
use crate::{jit::helpers, thread::LuaRegisters, Closure, Table, Value};

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::String(a), Value::String(b)) => {
            assert!(ottavino_gc_arena::Gc::ptr_eq(
                a.into_inner(),
                b.into_inner()
            ));
        }
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        (a, b) => {
            let a = Slot::from_value(a);
            let b = Slot::from_value(b);
            assert_ne!(a.tag, abi::REFERENCE);
            assert_eq!((a.tag, a.bits), (b.tag, b.bits));
        }
    }
}

#[test]
fn live_frames_preserve_helpers_rebinding_weak_modes_and_panics() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local total=0 for i=1,3 do host_increment(i); total=target end return total",
        )
        .unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let graph = FlowGraph::new(&snapshot).unwrap();
        let resume = Plan::new(&snapshot, &graph).unwrap().resume as usize;
        assert!(matches!(
            snapshot.operations[resume],
            Operation::GetUpTable { .. }
        ));
        assert!(snapshot.prepared_keys.is_some());
        let codes = [
            Failure::DisablePreferredEntry,
            Failure::RequirePreferredEntry,
        ]
        .map(|failure| compile(&snapshot, failure));
        assert!(!codes[0].preferred_entry);
        assert!(codes[1].preferred_entry);
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_4321)),
            Value::String(ctx.intern(b"reference")),
            Value::Table(Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 19)),
        ];
        let mut observed = [false; 3];
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
                    for pc in (0..=snapshot.operations.len()).chain([usize::MAX]) {
                        for budget in [0, 1, 2, 3, 63, 64, u32::MAX] {
                            let reports = codes.each_ref().map(|code| {
                                let mut canonical = vec![value; snapshot.registers];
                                let mut frame_pc = pc;
                                let report = LuaRegisters::with_test_native_frame(
                                    ctx,
                                    &mut frame_pc,
                                    &mut canonical,
                                    |mut registers, mut _list| {
                                        let mut slots: Vec<_> = registers
                                            .stack_frame
                                            .iter()
                                            .copied()
                                            .map(Slot::from_value)
                                            .collect();
                                        let mut frame = helpers::Frame {
                                            list: Some(_list.reborrow()),
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
                                            code.invoke_host(&mut slots, pc, budget, &mut host)
                                        };
                                        drop(borrow);
                                        let c = frame.count;
                                        let result = (
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
                                        );
                                        if pc == resume && budget > 0 {
                                            if case == 2 {
                                                assert_eq!(exit.reason, Kind::Panic as u32);
                                                assert!(frame.panic.is_some());
                                                assert_eq!(*frame.registers.pc, resume + 1);
                                            } else if case == 3 || case == 1 && value.is_nil() {
                                                assert_eq!(exit.reason, Kind::Interpreter as u32);
                                                assert_eq!(
                                                    (exit.pc, exit.instructions),
                                                    (resume as u64, 0)
                                                );
                                            } else {
                                                assert!(c.table_reads > 0);
                                                identical(
                                                    frame.registers.stack_frame[match snapshot
                                                        .operations[resume]
                                                    {
                                                        Operation::GetUpTable { dest, .. } => {
                                                            dest.0 as usize
                                                        }
                                                        _ => unreachable!(),
                                                    }],
                                                    if case == 4 {
                                                        Value::Integer(-19)
                                                    } else {
                                                        value
                                                    },
                                                );
                                            }
                                        }
                                        result
                                    },
                                );
                                (report, canonical)
                            });
                            assert_eq!(
                                reports[0].0, reports[1].0,
                                "mode={mode} case={case} pc={pc} budget={budget}"
                            );
                            for (&a, &b) in reports[0].1.iter().zip(&reports[1].1) {
                                identical(a, b);
                            }
                            observed[0] |= reports[1].0 .2[3] > 0;
                            observed[1] |= reports[1].0 .2[2] > 0;
                            observed[2] |= reports[1].0 .3;
                        }
                    }
                }
            }
        }
        assert_eq!(observed, [true; 3]);
    });
}
