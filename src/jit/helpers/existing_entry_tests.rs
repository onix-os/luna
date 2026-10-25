use super::*;
use crate::jit::{
    entry_bytes::EntryBytes,
    resources::{BudgetAllocator, Ledger},
};

struct Observation<'gc> {
    status: u32,
    pc: usize,
    values: [Value<'gc>; 3],
    slots: [Slot; 3],
    count: Counts,
    panicked: bool,
}

fn observe<'gc>(
    ctx: Context<'gc>,
    closure: Closure<'gc>,
    mut values: [Value<'gc>; 3],
    operand: u32,
    destination: u32,
    source_pc: u32,
    hints: Option<&EntryBytes>,
) -> Observation<'gc> {
    let mut slots = values.map(Slot::from_value);
    let mut pc = 9;
    let (status, count, panicked) =
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: Counts::default(),
                slot_count: slots.len(),
                panic: None,
                projection: None,
                read_hints: hints,
            };
            let mut host = abi::Host {
                data: std::ptr::from_mut(&mut frame).cast(),
                projection: std::ptr::null_mut(),
            };
            let status = unsafe {
                call::<{ abi::HELPER_GET_TABLE }>(
                    &mut host,
                    slots.as_mut_ptr(),
                    destination,
                    0,
                    operand,
                    source_pc,
                )
            };
            (status, frame.count, frame.panic.is_some())
        });
    Observation {
        status,
        pc,
        values,
        slots,
        count,
        panicked,
    }
}

fn assert_same<'gc>(left: &Observation<'gc>, right: &Observation<'gc>) {
    assert_eq!(
        (left.status, left.pc, left.panicked),
        (right.status, right.pc, right.panicked)
    );
    for (left, right) in left.values.iter().zip(&right.values) {
        tests::assert_identical(*left, *right);
    }
    for (left, right) in left.slots.iter().zip(&right.slots) {
        assert_eq!((left.tag, left.bits), (right.tag, right.bits));
    }
    let counts = |count: &Counts| {
        [
            count.calls,
            count.completed,
            count.declined,
            count.table_reads,
            count.table_writes,
            count.upvalue_reads,
            count.upvalue_writes,
            count.allocations,
        ]
    };
    assert_eq!(counts(&left.count), counts(&right.count));
}

#[test]
fn constant_string_reads_match_generic_effects_aliases_and_metamethod_declines() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local t={} return t.target").unwrap();
        let (index, key) = closure
            .prototype()
            .constants
            .iter()
            .enumerate()
            .find_map(|(index, value)| match value {
                crate::Constant::String(key) if key.as_bytes() == b"target" => {
                    Some((index as u32, *key))
                }
                _ => None,
            })
            .unwrap();
        let hints = EntryBytes::try_new(
            [true, false, true].into_iter(),
            BudgetAllocator(Ledger::new(3)),
        )
        .unwrap();
        let table = Table::new(&ctx);
        let meta = Table::new(&ctx);
        meta.set_field(ctx, "__index", Table::new(&ctx));
        for value in [
            Value::Nil,
            Value::Boolean(false),
            Value::Integer(-19),
            Value::Number(-0.0),
            Value::String(ctx.intern(b"marker")),
            Value::Table(Table::new(&ctx)),
        ] {
            table.set_raw(&ctx, Value::String(key), value).unwrap();
            for metatable in [None, Some(meta)] {
                table.set_metatable(ctx, metatable);
                for destination in 0..3 {
                    for pc in [0, 1, 2, 255, 65536] {
                        let values = [Value::Table(table), Value::Integer(73), Value::String(key)];
                        let generic = observe(ctx, closure, values, 2, destination, pc, None);
                        let canonical = observe(
                            ctx,
                            closure,
                            values,
                            index | abi::CONSTANT_OPERAND,
                            destination,
                            pc,
                            Some(&hints),
                        );
                        assert_same(&generic, &canonical);
                        let declined = value.is_nil() && metatable.is_some();
                        assert!(!canonical.panicked);
                        assert_eq!(
                            canonical.status,
                            if declined {
                                abi::HELPER_DECLINED
                            } else {
                                abi::HELPER_COMPLETED
                            }
                        );
                        assert_eq!(canonical.pc, pc as usize + usize::from(!declined));
                        assert_eq!(canonical.count.table_reads, u64::from(!declined));
                    }
                }
            }
        }
    });
}

#[test]
fn constant_string_hints_revalidate_table_mutations_and_non_table_operands() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local t={} return t.target").unwrap();
        let (index, key) = closure
            .prototype()
            .constants
            .iter()
            .enumerate()
            .find_map(|(index, value)| match value {
                crate::Constant::String(key) if key.as_bytes() == b"target" => {
                    Some((index as u32, *key))
                }
                _ => None,
            })
            .unwrap();
        let hints =
            EntryBytes::try_new([true].into_iter(), BudgetAllocator(Ledger::new(1))).unwrap();
        for generation in 0..8 {
            let table = Table::new(&ctx);
            for i in 0..generation * 32 {
                table.set(ctx, format!("other{i}"), i).unwrap();
            }
            for value in [
                Value::Integer(generation),
                Value::Nil,
                Value::Table(Table::new(&ctx)),
            ] {
                table.set_raw(&ctx, Value::String(key), value).unwrap();
                for stale in [None, Some(0), Some(1), Some(63), Some(126)] {
                    hints.hint(0).unwrap().remember(stale);
                    let values = [Value::Table(table), Value::Integer(73), Value::String(key)];
                    let generic = observe(ctx, closure, values, 2, 1, 0, None);
                    let canonical = observe(
                        ctx,
                        closure,
                        values,
                        index | abi::CONSTANT_OPERAND,
                        1,
                        0,
                        Some(&hints),
                    );
                    assert_same(&generic, &canonical);
                    assert!(hints.get(0).copied().unwrap());
                }
            }
        }
        for table in [Value::Nil, Value::Integer(1), Value::String(key)] {
            let values = [table, Value::Integer(73), Value::String(key)];
            let generic = observe(ctx, closure, values, 2, 1, 0, None);
            let canonical = observe(
                ctx,
                closure,
                values,
                index | abi::CONSTANT_OPERAND,
                1,
                0,
                Some(&hints),
            );
            assert_same(&generic, &canonical);
            assert_eq!(canonical.status, abi::HELPER_DECLINED);
        }
    });
}

#[test]
fn constant_string_reads_keep_panic_publication() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local t={} return t.target").unwrap();
        let index = closure.prototype().constants.iter().position(|value| {
            matches!(value, crate::Constant::String(key) if key.as_bytes() == b"target")
        }).unwrap() as u32;
        let table = Table::new(&ctx);
        table.set_field(ctx, "target", 42);
        let values = [Value::Table(table), Value::Number(-0.0), Value::Integer(73)];
        for (operand, destination) in [(index | abi::CONSTANT_OPERAND, 3), (u32::MAX, 1)] {
            let observed = observe(ctx, closure, values, operand, destination, 17, None);
            assert_eq!(observed.status, abi::HELPER_PANICKED);
            assert_eq!(observed.pc, 18);
            assert!(observed.panicked);
            assert_eq!(observed.count.calls, 1);
            assert_eq!(observed.count.completed, 0);
            assert_eq!(observed.count.declined, 0);
            assert_eq!(observed.count.table_reads, 0);
            for (left, right) in observed.values.iter().zip(&values) {
                tests::assert_identical(*left, *right);
            }
        }
    });
}

#[test]
fn canonical_entry_keeps_non_string_constant_fallback() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local t={} return t[1],t[1.5]").unwrap();
        let table = Table::new(&ctx);
        let hints =
            EntryBytes::try_new([true].into_iter(), BudgetAllocator(Ledger::new(1))).unwrap();
        let mut kinds = 0;
        for (index, &constant) in closure.prototype().constants.iter().enumerate() {
            match constant {
                crate::Constant::Integer(_) => kinds |= 1,
                crate::Constant::Number(_) => kinds |= 2,
                _ => continue,
            }
            let key = constant.into();
            table.set_raw(&ctx, key, Value::Integer(42)).unwrap();
            let values = [Value::Table(table), Value::Integer(73), key];
            let generic = observe(ctx, closure, values, 2, 1, 0, None);
            let canonical = observe(
                ctx,
                closure,
                values,
                index as u32 | abi::CONSTANT_OPERAND,
                1,
                0,
                Some(&hints),
            );
            assert_same(&generic, &canonical);
            assert_eq!(canonical.status, abi::HELPER_COMPLETED);
            assert!(hints.hint(0).unwrap().load().is_none());
        }
        assert_eq!(kinds, 3);
    });
}

#[test]
fn canonical_entry_declines_null_hosts_without_accessing_slots() {
    for operand in [0, abi::CONSTANT_OPERAND, u32::MAX] {
        assert_eq!(
            unsafe {
                call::<{ abi::HELPER_GET_TABLE }>(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    255,
                    255,
                    operand,
                    u32::MAX,
                )
            },
            abi::HELPER_DECLINED
        );
    }
}
