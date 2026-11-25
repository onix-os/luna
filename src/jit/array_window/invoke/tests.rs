use super::*;
use crate::{table::RawTable, Lua};

unsafe extern "C" fn store(
    slots: *mut Slot,
    pc: u64,
    budget: u32,
    exit: *mut abi::Exit,
    view: *const std::ffi::c_void,
) {
    let mut value = unsafe { slots.add(4).read() };
    let completed = if budget != 0 {
        unsafe { native::tests::write(view, value.bits as i64, &mut value) }
    } else {
        0
    };
    unsafe {
        exit.write(abi::Exit {
            pc: pc + u64::from(completed),
            instructions: completed,
            reason: if completed != 0 { 2 } else { 1 },
        })
    };
}

fn plan() -> Plan {
    Plan {
        start: 3,
        end: 4,
        base: 1,
        table: 0,
        access: Access::Write,
    }
}

fn array(ctx: Context<'_>) -> Table<'_> {
    let mut raw = RawTable::with_capacity(&ctx, 8, 0);
    raw.array_mut().fill(Value::Integer(7));
    Table::from_parts(&ctx, raw, None)
}

#[test]
fn kernel_bridge_releases_borrows_and_revalidates_canonical_receivers() {
    Lua::empty().enter(|ctx| {
        let first = array(ctx);
        let second = array(ctx);
        let mut canonical = [
            Value::Table(first),
            Value::Integer(1),
            Value::Integer(8),
            Value::Integer(1),
            Value::Integer(1),
        ];
        let mut slots = canonical.map(Slot::from_value);
        let result = unsafe { invoke(store, plan(), ctx, &canonical, &mut slots, 3, 1) }.unwrap();
        assert_eq!(
            (result.exit.pc, result.exit.instructions, result.exit.reason),
            (4, 1, 2)
        );
        assert_eq!(
            result.counts,
            Counts {
                reads: 0,
                writes: 1
            }
        );
        assert!(matches!(first.get_value(ctx, 1), Value::Integer(1)));
        first.set(ctx, 1, 17).unwrap();
        canonical[0] = Value::Table(second);
        let result = unsafe { invoke(store, plan(), ctx, &canonical, &mut slots, 3, 1) }.unwrap();
        assert_eq!(result.counts.writes, 1);
        assert!(matches!(first.get_value(ctx, 1), Value::Integer(17)));
        assert!(matches!(second.get_value(ctx, 1), Value::Integer(1)));
        second.set_readonly(&ctx, true);
        assert!(unsafe { invoke(store, plan(), ctx, &canonical, &mut slots, 3, 1) }.is_none());
    });
}

#[test]
fn bridge_declines_unusable_receivers_frames_and_observers_before_native_entry() {
    Lua::empty().enter(|ctx| {
        let table = array(ctx);
        let canonical = [
            Value::Table(table),
            Value::Integer(1),
            Value::Integer(8),
            Value::Integer(1),
            Value::Integer(1),
        ];
        let initial = canonical.map(Slot::from_value);
        for scenario in 0..12 {
            let mut canonical = canonical;
            let mut slots = initial;
            let mut pc = 3;
            let mut budget = 1;
            match scenario {
                0 => budget = 0,
                1 => pc = 5,
                2 => slots[0].tag = abi::INTEGER,
                3 => slots[0].bits = 1,
                4 => canonical[0] = Value::Nil,
                5 => slots[4] = Slot::from_value(Value::Integer(9)),
                6 => slots[3] = Slot::from_value(Value::Number(1.0)),
                7 => table.set_readonly(&ctx, true),
                8 => {
                    table.set_metatable(ctx, Some(Table::new(&ctx)));
                }
                9 => table.set_intercept_all_writes(&ctx, true),
                _ => (),
            }
            let inner = table.into_inner();
            let _borrow = (scenario == 10).then(|| inner.borrow_mut(&ctx));
            let slots = if scenario == 11 {
                &mut slots[..4]
            } else {
                &mut slots[..]
            };
            assert!(
                unsafe { invoke(store, plan(), ctx, &canonical, slots, pc, budget) }.is_none(),
                "scenario={scenario}"
            );
            drop(_borrow);
            table.set_readonly(&ctx, false);
            table.set_metatable(ctx, None);
            table.set_intercept_all_writes(&ctx, false);
            assert!(matches!(table.get_value(ctx, 1), Value::Integer(7)));
        }
    });
}
