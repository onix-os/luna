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

    fn assert_identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
        match (actual, expected) {
            (Value::Nil, Value::Nil) => {}
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
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
            (actual, expected) => panic!("value mismatch: {actual:?} != {expected:?}"),
        }
    }

    fn invoke<const KIND: u32>(
        frame: &mut Frame<'_, '_, '_>,
        slots: &mut [Slot],
        a: u32,
        b: u32,
        c: u32,
        pc: u32,
    ) -> u32 {
        assert!(slots.len() >= frame.slot_count);
        let mut host = abi::Host {
            data: (frame as *mut Frame<'_, '_, '_>).cast(),
        };
        unsafe { call::<KIND>(&mut host, slots.as_mut_ptr(), a, b, c, pc) }
    }

    #[test]
    fn moves_preserve_reference_identity_aliases_pending_scalars_and_panic_bounds() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return 42").unwrap();
            let values = [
                Value::String(crate::String::from_slice(&ctx, b"move")),
                Value::Table(Table::new(&ctx)),
                Value::Function(closure.into()),
                Value::Function(
                    crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return))
                        .into(),
                ),
                Value::Thread(crate::Thread::new(ctx)),
                Value::UserData(crate::UserData::new_static(&ctx, 42i64)),
            ];
            for original in values {
                let mut canonical = [original, Value::Integer(0), Value::Nil, Value::Nil];
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                    let mut slots = std::array::from_fn::<_, 4, _>(|index| {
                        Slot::from_value(registers.stack_frame[index])
                    });
                    let mut frame = Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: Counts::default(),
                        slot_count: 4,
                        panic: None,
                    };
                    for destination in [0, 1] {
                        assert_eq!(
                            invoke::<{ abi::HELPER_MOVE }>(
                                &mut frame,
                                &mut slots,
                                destination,
                                0,
                                0,
                                7
                            ),
                            abi::HELPER_COMPLETED
                        );
                        assert_identical(
                            frame.registers.stack_frame[destination as usize],
                            original,
                        );
                        assert_eq!(slots[destination as usize].tag, abi::REFERENCE);
                        assert_eq!(slots[destination as usize].bits, 0);
                        assert_eq!(*frame.registers.pc, 8);
                    }
                    let bits = 0x7ff8_0000_0000_1234;
                    slots[0] = Slot {
                        tag: abi::NUMBER,
                        bits,
                    };
                    assert_eq!(
                        invoke::<{ abi::HELPER_MOVE }>(&mut frame, &mut slots, 1, 0, 0, 7),
                        abi::HELPER_COMPLETED
                    );
                    assert_identical(
                        frame.registers.stack_frame[1],
                        Value::Number(f64::from_bits(bits)),
                    );
                    assert_eq!((slots[1].tag, slots[1].bits), (abi::NUMBER, bits));
                    slots[0] = Slot::from_value(original);
                    for (dest, source) in [(4, 0), (1, 4)] {
                        assert_eq!(
                            invoke::<{ abi::HELPER_MOVE }>(
                                &mut frame, &mut slots, dest, source, 0, 17
                            ),
                            abi::HELPER_PANICKED
                        );
                        assert_eq!(*frame.registers.pc, 18);
                        assert!(frame.panic.take().is_some());
                        assert_identical(frame.registers.stack_frame[0], original);
                        assert_identical(
                            frame.registers.stack_frame[1],
                            Value::Number(f64::from_bits(bits)),
                        );
                    }
                    assert_eq!(frame.count.calls, 5);
                    assert_eq!(frame.count.completed, 3);
                    assert_eq!(frame.count.declined, 0);
                });
            }
        });
        lua.gc_collect();
    }

    #[test]
    fn scoped_host_completes_all_helpers_with_canonical_reference_ownership() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let environment = Table::new(&ctx);
            let closure = Closure::load_with_env(
                ctx,
                Some("helper-frame"),
                b"return _ENV, 'helper-value'",
                environment,
            )
            .unwrap();
            let constant = closure
                .prototype()
                .constants
                .iter()
                .position(|value| {
                    matches!(value, crate::Constant::String(value) if value.as_bytes() == b"helper-value")
                })
                .unwrap() as u32;
            let key = abi::CONSTANT_OPERAND | constant;
            let mut values = [Value::Nil; 8];
            values[1] = Value::Integer(1);
            values[2] = Value::Integer(42);
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut slots: [Slot; 8] =
                    std::array::from_fn(|index| Slot::from_value(registers.stack_frame[index]));
                let mut frame = Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: Counts::default(),
                    slot_count: slots.len(),
                    panic: None,
                };
                macro_rules! complete {
                    ($kind:expr, $a:expr, $b:expr, $c:expr) => {
                        assert_eq!(
                            invoke::<{ $kind }>(&mut frame, &mut slots, $a, $b, $c, 7),
                            abi::HELPER_COMPLETED
                        );
                        assert_eq!(*frame.registers.pc, 8);
                    };
                }
                complete!(abi::HELPER_CONSTANT, 3, constant, 0);
                complete!(abi::HELPER_MOVE, 4, 3, 0);
                complete!(abi::HELPER_NEW_TABLE, 0, 4, 0);
                complete!(abi::HELPER_SET_TABLE, 0, 1, 2);
                complete!(abi::HELPER_GET_TABLE, 5, 0, 1);
                complete!(abi::HELPER_SET_UP_TABLE, 0, key, 2);
                complete!(abi::HELPER_GET_UP_TABLE, 6, 0, key);
                complete!(abi::HELPER_GET_UPVALUE, 7, 0, 0);
                complete!(abi::HELPER_SET_UPVALUE, 0, 0, 0);
                assert_eq!(frame.count.calls, 9);
                assert_eq!(frame.count.completed, 9);
                assert_eq!(frame.count.declined, 0);
                assert_eq!(frame.count.table_reads, 2);
                assert_eq!(frame.count.table_writes, 2);
                assert_eq!(frame.count.upvalue_reads, 3);
                assert_eq!(frame.count.upvalue_writes, 1);
                assert_eq!(frame.count.allocations, 1);
                assert!(frame.panic.is_none());
                assert_identical(frame.registers.stack_frame[5], Value::Integer(42));
                assert_identical(frame.registers.stack_frame[6], Value::Integer(42));
                assert_identical(frame.registers.stack_frame[7], Value::Table(environment));
                assert_identical(frame.registers.stack_frame[3], frame.registers.stack_frame[4]);
                assert_identical(
                    frame.registers.get_upvalue(&ctx, closure.upvalues()[0].get()),
                    frame.registers.stack_frame[0]
                );
                for (slot, canonical) in slots.iter().zip(frame.registers.stack_frame.iter()) {
                    assert_identical(slot.value(*canonical), *canonical);
                    if slot.tag == abi::REFERENCE {
                        assert_eq!(slot.bits, 0);
                    }
                }
            });
        });
        lua.gc_collect();
    }

    #[test]
    fn scoped_host_declines_before_effect_and_transports_panic_after_materialization() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return 42").unwrap();
            let mut values = [Value::Integer(0); 8];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let sentinel = Slot::from_value(Value::Integer(12345));
                let mut slots = [sentinel; 16];
                for slot in &mut slots[..8] {
                    *slot = Slot::from_value(Value::Integer(91));
                }
                let mut frame = Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: Counts::default(),
                    slot_count: 8,
                    panic: None,
                };
                assert_eq!(
                    invoke::<{ abi::HELPER_GET_TABLE }>(&mut frame, &mut slots, 0, 1, 2, 17),
                    abi::HELPER_DECLINED
                );
                assert_eq!(*frame.registers.pc, 17);
                assert!(frame
                    .registers
                    .stack_frame
                    .iter()
                    .all(|v| matches!(v, Value::Integer(0))));
                assert_eq!(frame.count.declined, 1);
                assert_eq!(
                    invoke::<{ abi::HELPER_MOVE }>(&mut frame, &mut slots, 8, 1, 0, 17),
                    abi::HELPER_PANICKED
                );
                assert_eq!(*frame.registers.pc, 18);
                assert!(frame
                    .registers
                    .stack_frame
                    .iter()
                    .all(|v| matches!(v, Value::Integer(91))));
                assert_eq!(frame.count.calls, 2);
                assert_eq!(frame.count.completed, 0);
                assert_eq!(frame.count.declined, 1);
                assert!(slots[8..]
                    .iter()
                    .all(|slot| slot.tag == sentinel.tag && slot.bits == sentinel.bits));
                let payload = frame.panic.take().unwrap();
                let original = payload.as_ref() as *const dyn Any as *const ();
                let propagated =
                    catch_unwind(AssertUnwindSafe(|| std::panic::resume_unwind(payload)))
                        .unwrap_err();
                assert_eq!(propagated.as_ref() as *const dyn Any as *const (), original);
                assert!(propagated
                    .downcast_ref::<String>()
                    .unwrap()
                    .contains("index out of bounds"));
            });
        });
        lua.gc_collect();
    }

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
