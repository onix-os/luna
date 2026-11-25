use super::*;
use crate::{
    closure::UpValueState,
    jit::{
        entry_bytes::EntryBytes,
        resources::{BudgetAllocator, Ledger},
    },
    types::RegisterIndex,
    Callback, CallbackReturn, Constant,
};
use std::{cell::Cell, rc::Rc};

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    status: u32,
    pc: usize,
    canonical: Vec<String>,
    slots: Vec<(u64, u64)>,
    counts: [u64; 8],
    panicked: bool,
}

#[test]
fn strong_hint_helper_matches_general_lookup_mutations_refusals_and_panic_state() {
    crate::Lua::empty().enter(|ctx| {
        for case in 0..13 {
            let closure = Closure::load(ctx, None, b"return target").unwrap();
            let Constant::String(key) = closure.prototype().constants[0] else {
                panic!()
            };
            let calls = Rc::new(Cell::new(0));
            let called = calls.clone();
            let callback = Callback::from_fn(&ctx, move |_, _, _| {
                called.set(called.get() + 1);
                Ok(CallbackReturn::Return)
            });
            let table = Table::new(&ctx);
            table.set(ctx, key, 7).unwrap();
            let hints = EntryBytes::try_new(
                [true, false].into_iter(),
                BudgetAllocator(Ledger::new(1024)),
            )
            .unwrap();
            assert!(matches!(
                table.get_canonical_string(ctx, key, hints.hint(0)),
                Value::Integer(7)
            ));
            let mut environment = table;
            match case {
                0 | 9 | 10 | 11 | 12 => {}
                1 => {
                    table.set(ctx, key, 99).unwrap();
                }
                2 => {
                    table.set(ctx, key, Value::Nil).unwrap();
                }
                3 => {
                    table.set(ctx, key, Value::Nil).unwrap();
                    let metatable = Table::new(&ctx);
                    metatable.set(ctx, "__index", callback).unwrap();
                    table.set_metatable(ctx, Some(metatable));
                }
                4 => {
                    environment = Table::new(&ctx);
                    environment.set(ctx, key, 88).unwrap();
                }
                5..=7 => {
                    let metatable = Table::new(&ctx);
                    metatable
                        .set(
                            ctx,
                            "__mode",
                            match case {
                                5 => "k",
                                6 => "v",
                                _ => "kv",
                            },
                        )
                        .unwrap();
                    table.set_metatable(ctx, Some(metatable));
                    table.set(ctx, key, callback).unwrap();
                }
                8 => {
                    for index in 0..300 {
                        table.set(ctx, index, index).unwrap();
                    }
                    table.set(ctx, key, callback).unwrap();
                }
                _ => unreachable!(),
            }
            let storage = environment.into_inner();
            let conflict = (case == 9).then(|| storage.borrow_mut(&ctx));
            let hash = crate::table::PreparedStringHash::new(key).bits();
            let observe = |route| {
                let mut values = [
                    Value::Table(environment),
                    Value::Integer(73),
                    Value::Number(-0.0),
                ];
                let mut slots = values.map(Slot::from_value);
                if case == 11 {
                    slots[0] = Slot::from_value(Value::Boolean(false));
                }
                let mut pc = 19;
                let result =
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                        let cell = if case == 10 || case == 11 {
                            registers.open_test_upvalue(&ctx, RegisterIndex(0))
                        } else {
                            crate::closure::UpValue::new(
                                &ctx,
                                UpValueState::Closed(Value::Table(environment)),
                            )
                        };
                        closure.set_upvalue(&ctx, 0, cell);
                        let mut frame = Frame {
                            ctx,
                            closure,
                            registers: &mut registers,
                            count: Counts::default(),
                            slot_count: slots.len(),
                            panic: None,
                            projection: None,
                            read_hints: (route == 2).then_some(&hints),
                        };
                        if case == 12 {
                            frame.count.table_reads = u64::MAX;
                        }
                        let mut host = abi::Host {
                            data: std::ptr::from_mut(&mut frame).cast(),
                            projection: std::ptr::null_mut(),
                        };
                        let packed = 1 << 16;
                        let status = unsafe {
                            if route == 0 {
                                PREHASHED_ENTRY(
                                    &mut host,
                                    slots.as_mut_ptr(),
                                    packed,
                                    hash as u32,
                                    (hash >> 32) as u32,
                                    0,
                                )
                            } else {
                                call_hashless(&mut host, slots.as_mut_ptr(), u64::from(packed))
                            }
                        };
                        (
                            status,
                            [
                                frame.count.calls,
                                frame.count.completed,
                                frame.count.declined,
                                frame.count.table_reads,
                                frame.count.table_writes,
                                frame.count.upvalue_reads,
                                frame.count.upvalue_writes,
                                frame.count.allocations,
                            ],
                            frame.panic.is_some(),
                        )
                    });
                Observed {
                    status: result.0,
                    pc,
                    canonical: values
                        .iter()
                        .map(|v| format!("{:?}", v.debug_shallow()))
                        .collect(),
                    slots: slots.iter().map(|s| (s.tag, s.bits)).collect(),
                    counts: result.1,
                    panicked: result.2,
                }
            };
            let general = observe(0);
            let panicked = case == 9 || (case == 12 && cfg!(debug_assertions));
            let declined = case == 3 || case == 11;
            assert_eq!(
                general.status,
                if panicked {
                    abi::HELPER_PANICKED
                } else if declined {
                    abi::HELPER_DECLINED
                } else {
                    abi::HELPER_COMPLETED
                },
                "case={case}"
            );
            assert_eq!(general.panicked, panicked, "case={case}");
            assert_eq!(general.pc, usize::from(!declined), "case={case}");
            assert_eq!(general.counts[0], 1, "case={case}");
            if !panicked && !declined {
                let expected = match case {
                    0 | 10 | 12 => Value::Integer(7),
                    1 => Value::Integer(99),
                    2 => Value::Nil,
                    4 => Value::Integer(88),
                    5..=8 => Value::Function(callback.into()),
                    _ => unreachable!(),
                };
                assert_eq!(
                    general.canonical[1],
                    format!("{:?}", expected.debug_shallow()),
                    "case={case}"
                );
                let expected = Slot::from_value(expected);
                assert_eq!(general.slots[1], (expected.tag, expected.bits));
                assert_eq!(general.counts[1], 1);
                assert_eq!(general.counts[5], 1);
            }
            assert_eq!(observe(1), general, "uncached case={case}");
            assert_eq!(observe(2), general, "hinted case={case}");
            assert_eq!(calls.get(), 0, "helper called user code case={case}");
            drop(conflict);
            assert!(storage.try_borrow_mut(&ctx).is_ok());
        }
    });
}
