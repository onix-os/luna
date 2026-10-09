use std::cell::RefMut;

use crate::{table::TableState, Context, Table, Value};

pub(in crate::jit) fn recurring(operations: &[crate::opcode::Operation]) -> bool {
    use crate::opcode::Operation;
    for (end, operation) in operations.iter().enumerate() {
        let Operation::NumericForLoop { base, jump } = *operation else {
            continue;
        };
        let Some(start) = (end + 1).checked_add_signed(isize::from(jump)) else {
            continue;
        };
        if start >= end || end - start > 32 || base.0 > 252 {
            continue;
        }
        let body = &operations[start..end];
        let Some(receiver) = body.iter().find_map(|operation| match *operation {
            Operation::GetTable { table, .. } | Operation::SetTable { table, .. } => Some(table),
            _ => None,
        }) else {
            continue;
        };
        if (usize::from(base.0)..=usize::from(base.0) + 3).contains(&usize::from(receiver.0)) {
            continue;
        }
        if body.iter().all(|operation| match *operation {
            Operation::GetTable { dest, table, .. } => table == receiver && dest != receiver,
            Operation::SetTable { table, .. } => table == receiver,
            Operation::Add { dest, .. }
            | Operation::Sub { dest, .. }
            | Operation::Mul { dest, .. } => dest != receiver,
            _ => false,
        }) {
            return true;
        }
    }
    false
}

#[derive(Default)]
pub(in crate::jit) struct Cache<'gc> {
    enabled: bool,
    held: Option<(Table<'gc>, RefMut<'gc, TableState<'gc>>)>,
}

impl<'gc> Cache<'gc> {
    pub(in crate::jit) fn enabled() -> Self {
        Self {
            enabled: true,
            held: None,
        }
    }

    pub(in crate::jit) fn clear(&mut self) {
        self.held = None;
    }

    fn table(&mut self, ctx: Context<'gc>, table: Table<'gc>) -> Option<&mut TableState<'gc>> {
        if !self.enabled {
            return None;
        }
        if self.held.as_ref().is_none_or(|(held, _)| *held != table) {
            self.clear();
            let state = table.into_inner().try_borrow_mut(&ctx).ok()?;
            if state.metatable.is_some()
                || state.readonly
                || state.intercept_all_writes
                || state.raw_table.has_weak_values()
                || state.raw_table.has_weak_keys()
            {
                return None;
            }
            self.held = Some((table, state));
        }
        self.held.as_mut().map(|(_, state)| &mut **state)
    }

    pub(super) fn read(
        &mut self,
        ctx: Context<'gc>,
        table: Table<'gc>,
        key: Value<'gc>,
    ) -> Option<Value<'gc>> {
        Some(self.table(ctx, table)?.raw_table.get(&ctx, key))
    }

    pub(super) fn write(
        &mut self,
        ctx: Context<'gc>,
        table: Table<'gc>,
        key: Value<'gc>,
        value: Value<'gc>,
    ) -> Option<bool> {
        Some(
            self.table(ctx, table)?
                .raw_table
                .set(&ctx, key, value)
                .is_ok(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{table::RawTable, Lua};

    #[test]
    fn classifies_compiled_table_loops_and_isolated_accesses() {
        let cases: &[(&[u8], bool)] = &[
            (b"local t={} for i=1,50 do t[i]=i end local n=0 for i=1,50 do n=n+t[i] end return n", true),
            (b"local t={a=1} for i=1,50 do t.a=t.a+i end return t.a", true),
            (b"local n=0 for i=1,50 do local t={i,i+1} n=n+t[1] end return n", false),
            (b"local t={} t.a=1 return t.a", false),
            (b"local t={} for i=1,50 do t={} t[i]=i end return t", false),
        ];
        Lua::empty().enter(|ctx| {
            for &(source, expected) in cases {
                let closure = crate::Closure::load(ctx, None, source).unwrap();
                let operations: Vec<_> = closure
                    .prototype()
                    .opcodes
                    .iter()
                    .map(|op| op.decode())
                    .collect();
                assert_eq!(recurring(&operations), expected, "source={source:?}");
            }
        });
    }

    #[test]
    fn selects_bounded_repeated_receivers_without_changing_native_eligibility() {
        use crate::{
            opcode::{Operation, RCIndex},
            types::RegisterIndex as R,
        };
        let store = Operation::SetTable {
            table: R(0),
            key: RCIndex::Register(R(4)),
            value: RCIndex::Register(R(4)),
        };
        for count in [1, 2, 31, 32, 33, 100] {
            let mut operations = vec![store; count];
            operations.push(Operation::NumericForLoop {
                base: R(1),
                jump: -(count as i16 + 1),
            });
            assert_eq!(recurring(&operations), count <= 32);
        }
        let branch = Operation::NumericForLoop {
            base: R(1),
            jump: -3,
        };
        let read = Operation::GetTable {
            dest: R(5),
            table: R(0),
            key: RCIndex::Register(R(4)),
        };
        let add = Operation::Add {
            dest: R(6),
            left: RCIndex::Register(R(6)),
            right: RCIndex::Register(R(5)),
        };
        assert!(recurring(&[read, add, branch]));
        for middle in [
            Operation::Move {
                dest: R(8),
                source: R(9),
            },
            Operation::SetTable {
                table: R(9),
                key: RCIndex::Register(R(4)),
                value: RCIndex::Register(R(4)),
            },
            Operation::GetTable {
                dest: R(0),
                table: R(0),
                key: RCIndex::Register(R(4)),
            },
            Operation::Add {
                dest: R(0),
                left: RCIndex::Register(R(6)),
                right: RCIndex::Register(R(5)),
            },
        ] {
            assert!(!recurring(&[store, middle, branch]));
        }
        for jump in [0, 1, i16::MIN] {
            assert!(!recurring(&[
                store,
                Operation::NumericForLoop { base: R(1), jump }
            ]));
        }
        for base in [0, 253, 255] {
            assert!(!recurring(&[
                store,
                Operation::NumericForLoop {
                    base: R(base),
                    jump: -2
                }
            ]));
        }
        assert!(!recurring(&[read, add]));
    }

    #[test]
    fn helper_declines_panics_and_other_operations_release_the_borrow() {
        use super::super::{abi, call, Frame};
        use crate::{thread::LuaRegisters, Closure};

        for scenario in 0..4 {
            Lua::empty().enter(|ctx| {
                let table = Table::new(&ctx);
                table.set(ctx, 1, 42).unwrap();
                table
                    .set(ctx, crate::MetaMethod::Index, Table::new(&ctx))
                    .unwrap();
                let other = Table::from_parts(&ctx, RawTable::new(&ctx), Some(table));
                let closure = Closure::load(ctx, None, &b"return"[..]).unwrap();
                let mut values = [Value::Table(table), Value::Integer(1), Value::Nil];
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                    let mut slots = values_for(registers.stack_frame);
                    let mut frame = Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: Default::default(),
                        table_cache: Cache::enabled(),
                        slot_count: slots.len(),
                        panic: None,
                        projection: None,
                    };
                    let mut host = abi::Host {
                        data: std::ptr::from_mut(&mut frame).cast(),
                        projection: std::ptr::null_mut(),
                    };
                    assert_eq!(
                        unsafe {
                            call::<{ abi::HELPER_GET_TABLE }>(
                                &mut host,
                                slots.as_mut_ptr(),
                                2,
                                0,
                                1,
                                7,
                            )
                        },
                        abi::HELPER_COMPLETED
                    );
                    assert!(table.into_inner().try_borrow().is_err());
                    let result = match scenario {
                        0 => unsafe {
                            call::<{ abi::HELPER_MOVE }>(&mut host, slots.as_mut_ptr(), 2, 1, 0, 8)
                        },
                        1 => unsafe {
                            call::<{ abi::HELPER_GET_TABLE }>(
                                &mut host,
                                slots.as_mut_ptr(),
                                2,
                                99,
                                1,
                                8,
                            )
                        },
                        _ => {
                            let value = if scenario == 2 {
                                Value::Integer(0)
                            } else {
                                other.into()
                            };
                            frame.registers.stack_frame[0] = value;
                            slots[0] = abi::Slot::from_value(value);
                            unsafe {
                                call::<{ abi::HELPER_GET_TABLE }>(
                                    &mut host,
                                    slots.as_mut_ptr(),
                                    2,
                                    0,
                                    1,
                                    8,
                                )
                            }
                        }
                    };
                    assert_eq!(
                        result,
                        match scenario {
                            0 => abi::HELPER_COMPLETED,
                            1 => abi::HELPER_PANICKED,
                            _ => abi::HELPER_DECLINED,
                        }
                    );
                    assert!(table.into_inner().try_borrow_mut(&ctx).is_ok());
                    assert!(other.into_inner().try_borrow_mut(&ctx).is_ok());
                    assert_eq!(frame.count.calls, 2);
                    assert_eq!(frame.count.completed, if scenario == 0 { 2 } else { 1 });
                    assert_eq!(frame.count.table_reads, 1);
                    assert_eq!(frame.count.declined, u64::from(scenario >= 2));
                    assert_eq!(*frame.registers.pc, if scenario >= 2 { 8 } else { 9 });
                    assert_eq!(frame.panic.is_some(), scenario == 1);
                });
            });
        }

        fn values_for(values: &[Value<'_>]) -> [abi::Slot; 3] {
            std::array::from_fn(|index| abi::Slot::from_value(values[index]))
        }
    }

    #[test]
    fn reuses_one_borrow_and_releases_before_switching_or_unwinding() {
        Lua::empty().enter(|ctx| {
            let a = Table::new(&ctx);
            let b = Table::new(&ctx);
            let mut cache = Cache::enabled();
            for i in 1..257 {
                assert_eq!(cache.write(ctx, a, i.into(), (i * 2).into()), Some(true));
                assert!(matches!(cache.read(ctx, a, i.into()), Some(Value::Integer(value)) if value == i * 2));
                assert!(a.into_inner().try_borrow().is_err());
            }
            assert!(matches!(cache.read(ctx, b, 1.into()), Some(Value::Nil)));
            assert!(a.into_inner().try_borrow_mut(&ctx).is_ok());
            assert!(b.into_inner().try_borrow().is_err());
            cache.clear();
            assert!(b.into_inner().try_borrow_mut(&ctx).is_ok());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut cache = Cache::enabled();
                assert!(cache.read(ctx, a, 1.into()).is_some());
                panic!("native unwind");
            }));
            assert!(result.is_err());
            assert!(a.into_inner().try_borrow_mut(&ctx).is_ok());
            assert!(matches!(a.get_raw(&ctx, 256.into()), Value::Integer(512)));
        });
    }

    #[test]
    fn declines_flags_and_external_borrows_without_retaining_old_tables() {
        Lua::empty().enter(|ctx| {
            let ordinary = Table::new(&ctx);
            for kind in 0..7 {
                let table = Table::new(&ctx);
                {
                    let mut state = table.into_inner().borrow_mut(&ctx);
                    match kind {
                        0 => state.readonly = true,
                        1 => state.intercept_all_writes = true,
                        2 => state.metatable = Some(table),
                        3 => state.raw_table.make_values_weak(&ctx),
                        4 => state.raw_table.make_keys_weak(&ctx),
                        _ => {}
                    }
                }
                let shared = (kind == 5).then(|| table.into_inner().borrow());
                let exclusive = (kind == 6).then(|| table.into_inner().borrow_mut(&ctx));
                let mut cache = Cache::enabled();
                assert!(cache.read(ctx, ordinary, 1.into()).is_some());
                assert!(cache.read(ctx, table, 1.into()).is_none());
                assert!(cache.held.is_none());
                assert!(ordinary.into_inner().try_borrow_mut(&ctx).is_ok());
                drop((shared, exclusive));
            }
        });
    }

    #[test]
    fn preserves_key_rules_reference_values_and_disabled_behavior() {
        Lua::empty().enter(|ctx| {
            let table = Table::from_parts(&ctx, RawTable::with_capacity(&ctx, 8, 0), None);
            let marker = Table::new(&ctx);
            let mut cache = Cache::enabled();
            assert_eq!(cache.write(ctx, table, Value::Nil, 1.into()), Some(false));
            assert_eq!(cache.write(ctx, table, f64::NAN.into(), 1.into()), Some(false));
            assert_eq!(cache.write(ctx, table, 1.0.into(), marker.into()), Some(true));
            assert!(matches!(cache.read(ctx, table, 1.into()), Some(Value::Table(value)) if value == marker));
            assert_eq!(cache.write(ctx, table, (-0.0).into(), 42.into()), Some(true));
            assert!(matches!(cache.read(ctx, table, 0.into()), Some(Value::Integer(42))));
            cache.clear();
            table.set_readonly(&ctx, true);
            assert!(cache.write(ctx, table, 1.into(), 2.into()).is_none());
            table.set_readonly(&ctx, false);
            let mut disabled = Cache::default();
            assert!(disabled.read(ctx, table, 1.into()).is_none());
            assert!(table.into_inner().try_borrow_mut(&ctx).is_ok());
        });
    }
}
