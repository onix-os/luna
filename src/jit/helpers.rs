use std::{
    any::Any,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::{table::RawTable, thread::LuaRegisters, Closure, Context, MetaMethod, Table, Value};

use super::abi::{self, Slot};

const fn symbol<const KIND: u32>(name: &'static str) -> (u32, &'static str, abi::HelperEntry) {
    (KIND, name, call::<KIND>)
}

pub(super) const SYMBOLS: [(u32, &str, abi::HelperEntry); 9] = [
    symbol::<{ abi::HELPER_MOVE }>("luna_move_v3"),
    symbol::<{ abi::HELPER_CONSTANT }>("luna_constant_v3"),
    symbol::<{ abi::HELPER_NEW_TABLE }>("luna_new_table_v3"),
    symbol::<{ abi::HELPER_GET_TABLE }>("luna_get_table_v3"),
    symbol::<{ abi::HELPER_SET_TABLE }>("luna_set_table_v3"),
    symbol::<{ abi::HELPER_GET_UP_TABLE }>("luna_get_up_table_v3"),
    symbol::<{ abi::HELPER_SET_UP_TABLE }>("luna_set_up_table_v3"),
    symbol::<{ abi::HELPER_GET_UPVALUE }>("luna_get_upvalue_v3"),
    symbol::<{ abi::HELPER_SET_UPVALUE }>("luna_set_upvalue_v3"),
];

#[derive(Default)]
pub(super) struct Counts {
    pub calls: u64,
    pub completed: u64,
    pub declined: u64,
    pub table_reads: u64,
    pub table_writes: u64,
    pub upvalue_reads: u64,
    pub upvalue_writes: u64,
    pub allocations: u64,
}

pub(super) struct Frame<'gc, 'a, 'b> {
    pub ctx: Context<'gc>,
    pub closure: Closure<'gc>,
    pub registers: &'a mut LuaRegisters<'gc, 'b>,
    pub count: Counts,
    pub slot_count: usize,
    pub panic: Option<Box<dyn Any + Send>>,
}

impl<'gc> Frame<'gc, '_, '_> {
    fn register(&self, slots: &[Slot], index: u32) -> Value<'gc> {
        slots[index as usize].value(self.registers.stack_frame[index as usize])
    }

    fn store(&mut self, slots: &mut [Slot], index: u32, value: Value<'gc>) {
        self.registers.stack_frame[index as usize] = value;
        slots[index as usize] = Slot::from_value(value);
    }

    fn operand(&self, slots: &[Slot], operand: u32) -> Value<'gc> {
        if operand & abi::CONSTANT_OPERAND != 0 {
            self.closure.prototype().constants[(operand & !abi::CONSTANT_OPERAND) as usize].into()
        } else {
            self.register(slots, operand)
        }
    }

    fn table_read(
        &mut self,
        slots: &mut [Slot],
        dest: u32,
        table: Value<'gc>,
        key: Value<'gc>,
    ) -> bool {
        let Value::Table(table) = table else {
            return false;
        };
        let value = table.get_raw(&self.ctx, key);
        if value.is_nil()
            && table
                .metatable()
                .is_some_and(|mt| !mt.get_value(self.ctx, MetaMethod::Index).is_nil())
        {
            return false;
        }
        self.store(slots, dest, value);
        self.count.table_reads += 1;
        true
    }

    fn table_write(&mut self, table: Value<'gc>, key: Value<'gc>, value: Value<'gc>) -> bool {
        let Value::Table(table) = table else {
            return false;
        };
        if table.is_readonly()
            || matches!(key, Value::Nil)
            || matches!(key, Value::Number(value) if value.is_nan())
        {
            return false;
        }
        if (table.get_raw(&self.ctx, key).is_nil() || table.intercepts_all_writes())
            && table
                .metatable()
                .is_some_and(|mt| !mt.get_value(self.ctx, MetaMethod::NewIndex).is_nil())
        {
            return false;
        }
        if table.set_raw(&self.ctx, key, value).is_err() {
            return false;
        }
        self.count.table_writes += 1;
        true
    }

    fn operation<const KIND: u32>(&mut self, slots: &mut [Slot], a: u32, b: u32, c: u32) -> bool {
        match KIND {
            abi::HELPER_MOVE => {
                self.store(slots, a, self.register(slots, b));
                true
            }
            abi::HELPER_CONSTANT => {
                self.store(
                    slots,
                    a,
                    self.closure.prototype().constants[b as usize].into(),
                );
                true
            }
            abi::HELPER_NEW_TABLE => {
                let table = Table::from_parts(
                    &self.ctx,
                    RawTable::with_capacity(&self.ctx, b as usize, c as usize),
                    None,
                );
                self.store(slots, a, Value::Table(table));
                self.count.allocations += 1;
                true
            }
            abi::HELPER_GET_TABLE => {
                let table = self.register(slots, b);
                let key = self.operand(slots, c);
                self.table_read(slots, a, table, key)
            }
            abi::HELPER_SET_TABLE => {
                let table = self.register(slots, a);
                let key = self.operand(slots, b);
                let value = self.operand(slots, c);
                self.table_write(table, key, value)
            }
            abi::HELPER_GET_UP_TABLE => {
                let table = self
                    .registers
                    .get_upvalue(&self.ctx, self.closure.upvalues()[b as usize].get());
                let key = self.operand(slots, c);
                let completed = self.table_read(slots, a, table, key);
                self.count.upvalue_reads += u64::from(completed);
                completed
            }
            abi::HELPER_SET_UP_TABLE => {
                let table = self
                    .registers
                    .get_upvalue(&self.ctx, self.closure.upvalues()[a as usize].get());
                let key = self.operand(slots, b);
                let value = self.operand(slots, c);
                let completed = self.table_write(table, key, value);
                self.count.upvalue_reads += u64::from(completed);
                completed
            }
            abi::HELPER_GET_UPVALUE => {
                let value = self
                    .registers
                    .get_upvalue(&self.ctx, self.closure.upvalues()[b as usize].get());
                self.store(slots, a, value);
                self.count.upvalue_reads += 1;
                true
            }
            abi::HELPER_SET_UPVALUE => {
                let value = self.register(slots, b);
                self.registers.set_upvalue(
                    &self.ctx,
                    self.closure.upvalues()[a as usize].get(),
                    value,
                );
                self.count.upvalue_writes += 1;
                true
            }
            _ => false,
        }
    }
}

unsafe extern "C" fn call<const KIND: u32>(
    host: *mut abi::Host,
    slots: *mut Slot,
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    if host.is_null() {
        return abi::HELPER_DECLINED;
    }
    let data = unsafe { (*host).data };
    let frame = unsafe { &mut *data.cast::<Frame<'_, '_, '_>>() };
    let slots = unsafe { std::slice::from_raw_parts_mut(slots, frame.slot_count) };
    frame.count.calls += 1;
    let result = catch_unwind(AssertUnwindSafe(|| {
        *frame.registers.pc = pc as usize + 1;
        if !frame.operation::<KIND>(slots, a, b, c) {
            *frame.registers.pc = pc as usize;
            return abi::HELPER_DECLINED;
        }
        *frame.registers.pc = pc as usize + 1;
        abi::HELPER_COMPLETED
    }));
    match result {
        Ok(abi::HELPER_COMPLETED) => {
            frame.count.completed += 1;
            abi::HELPER_COMPLETED
        }
        Ok(_) => {
            frame.count.declined += 1;
            abi::HELPER_DECLINED
        }
        Err(payload) => {
            for (slot, dest) in slots
                .iter()
                .copied()
                .zip(frame.registers.stack_frame.iter_mut())
            {
                slot.write_back(dest);
            }
            frame.panic = Some(payload);
            abi::HELPER_PANICKED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_symbols_have_unique_keys_and_decline_null_hosts() {
        let mut kinds = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for (kind, name, entry) in SYMBOLS {
            assert!(kinds.insert(kind));
            assert!(names.insert(name));
            let result = unsafe {
                entry(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    u32::MAX,
                    u32::MAX,
                    u32::MAX,
                    u32::MAX,
                )
            };
            assert_eq!(result, abi::HELPER_DECLINED);
        }
    }
}
