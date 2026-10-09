use super::*;
use crate::{table::RawTable, Lua};

pub(in crate::jit::array_window) unsafe extern "C" fn read(
    pointer: *const c_void,
    key: i64,
    slot: *mut Slot,
) -> u32 {
    unsafe { access(pointer, key, slot, Access::Read) }
}

pub(in crate::jit::array_window) unsafe extern "C" fn write(
    pointer: *const c_void,
    key: i64,
    slot: *mut Slot,
) -> u32 {
    unsafe { access(pointer, key, slot, Access::Write) }
}

unsafe fn access(pointer: *const c_void, key: i64, slot: *mut Slot, access: Access) -> u32 {
    if pointer.is_null() || slot.is_null() {
        return 0;
    }
    let view = unsafe { pointer.cast::<View>().read() };
    if view.version != VERSION
        || view.length == 0
        || view.length > LIMIT as u64
        || view.first <= 0
        || view.writable > 1
        || (access == Access::Write && view.writable == 0)
        || view.slots.is_null()
        || view.dirty.is_null()
        || view.counts.is_null()
    {
        return 0;
    }
    let Some(index) = key
        .checked_sub(view.first)
        .and_then(|i| usize::try_from(i).ok())
    else {
        return 0;
    };
    if index >= view.length as usize {
        return 0;
    }
    let cell = unsafe { view.slots.add(index) };
    let value = unsafe {
        if access == Access::Write {
            slot.read()
        } else {
            cell.read()
        }
    };
    if !scalar(value) {
        return 0;
    }
    unsafe {
        if access == Access::Write {
            cell.write(value);
            *view.dirty |= 1 << index;
            (*view.counts).writes = (*view.counts).writes.saturating_add(1);
        } else {
            slot.write(value);
            (*view.counts).reads = (*view.counts).reads.saturating_add(1);
        }
    }
    1
}

pub(in crate::jit::array_window) fn exercise(read: Entry, write: Entry) {
    Lua::empty().enter(|ctx| {
        let mut raw = RawTable::with_capacity(&ctx, 66, 0);
        raw.array_mut()[..66].fill(Value::Integer(7));
        let child = Table::new(&ctx);
        raw.array_mut()[2] = Value::Table(child);
        let table = Table::from_parts(&ctx, raw, None);
        let mut counts = Counts {
            reads: u32::MAX - 1,
            writes: u32::MAX - 1,
        };
        let mut slot = Slot::from_value(Value::Integer(99));
        with_window::<_, 64>(ctx, table, 2, 64, Access::Write, &mut counts, |window| {
            window.with_native(|session| unsafe {
                assert_eq!(session.invoke(read, 3, &mut slot), 0);
                assert_eq!(slot.bits, 99);
                for key in [i64::MIN, 0, 1, 66, i64::MAX] {
                    assert_eq!(session.invoke(write, key, &mut slot), 0);
                    assert_eq!(session.invoke(read, key, &mut slot), 0);
                }
                for key in 2..=65 {
                    slot = Slot {
                        tag: abi::NUMBER,
                        bits: 0x7ff8_0000_0000_0000 | key as u64,
                    };
                    assert_eq!(session.invoke(write, key, &mut slot), 1);
                    slot = Slot::from_value(Value::Nil);
                    assert_eq!(session.invoke(read, key, &mut slot), 1);
                    assert_eq!(slot.tag, abi::NUMBER);
                    assert_eq!(slot.bits, 0x7ff8_0000_0000_0000 | key as u64);
                }
            });
            assert_eq!(window.dirty, u64::MAX);
            assert_eq!(window.read(65).unwrap().bits, 0x7ff8_0000_0000_0041);
        })
        .unwrap();
        assert_eq!(
            counts,
            Counts {
                reads: u32::MAX,
                writes: u32::MAX
            }
        );
        assert!(matches!(table.get_value(ctx, 1), Value::Integer(7)));
        assert!(matches!(table.get_value(ctx, 66), Value::Integer(7)));
        for key in 2..=65 {
            let Value::Number(value) = table.get_value(ctx, key) else {
                panic!("scalar not committed")
            };
            assert_eq!(value.to_bits(), 0x7ff8_0000_0000_0000 | key as u64);
        }
        table.set_readonly(&ctx, true);
        with_window::<_, 64>(ctx, table, 2, 64, Access::Read, &mut counts, |window| {
            window.with_native(|session| unsafe {
                assert_eq!(session.invoke(write, 2, &mut slot), 0);
                assert_eq!(session.invoke(read, 2, &mut slot), 1);
            });
            assert_eq!(window.dirty, 0);
        })
        .unwrap();
    });
}

#[test]
fn scoped_native_pointers_preserve_writeback_and_saturating_counts() {
    exercise(read, write);
}

#[test]
fn completed_native_writes_flush_when_the_rust_scope_unwinds() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    Lua::empty().enter(|ctx| {
        let mut raw = RawTable::with_capacity(&ctx, 1, 0);
        raw.array_mut()[0] = Value::Integer(7);
        let table = Table::from_parts(&ctx, raw, None);
        let mut counts = Counts::default();
        let result = catch_unwind(AssertUnwindSafe(|| {
            with_window::<_, 1>(ctx, table, 1, 1, Access::Write, &mut counts, |window| {
                window.with_native(|session| {
                    let mut slot = Slot::from_value(Value::Integer(13));
                    assert_eq!(unsafe { session.invoke(write, 1, &mut slot) }, 1);
                    std::panic::panic_any(17u64);
                });
            })
            .unwrap();
        }));
        assert_eq!(*result.unwrap_err().downcast::<u64>().unwrap(), 17);
        assert!(matches!(table.get_value(ctx, 1), Value::Integer(13)));
        assert_eq!(counts.writes, 1);
        table.set(ctx, 1, 19).unwrap();
    });
}
