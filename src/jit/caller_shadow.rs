use crate::{closure::UpValue, thread::LuaRegisters, Closure};

use super::{
    abi::{self, Slot},
    projection::Origin,
};

pub(super) struct Shadow<'gc, const N: usize> {
    closure: Closure<'gc>,
    key: (usize, usize),
    count: usize,
    ready: bool,
    slots: [Slot; N],
}

pub(super) struct Transfer<'a, 'gc, const N: usize> {
    shadow: &'a mut Shadow<'gc, N>,
    tail: usize,
    capture: Option<usize>,
}

impl<'gc, const N: usize> Shadow<'gc, N> {
    pub(super) fn capture(
        closure: Closure<'gc>,
        registers: &LuaRegisters<'gc, '_>,
    ) -> Option<Self> {
        let count = registers.stack_frame.len();
        if count > N {
            return None;
        }
        let mut slots = [Slot {
            tag: abi::NIL,
            bits: 0,
        }; N];
        for (slot, value) in slots.iter_mut().zip(registers.stack_frame.iter().copied()) {
            *slot = Slot::from_value(value);
        }
        Some(Self {
            closure,
            key: registers.shadow_key(),
            count,
            ready: true,
            slots,
        })
    }

    fn matches(&self, closure: Closure<'gc>, registers: &LuaRegisters<'gc, '_>) -> bool {
        ottavino_gc_arena::Gc::ptr_eq(self.closure.into_inner(), closure.into_inner())
            && self.key == registers.shadow_key()
            && self.count == registers.stack_frame.len()
    }

    fn valid(&self) -> bool {
        self.slots[..self.count]
            .iter()
            .all(|slot| slot.tag <= abi::REFERENCE)
    }

    pub(super) fn flush(
        &self,
        closure: Closure<'gc>,
        registers: &mut LuaRegisters<'gc, '_>,
    ) -> bool {
        if !self.ready || !self.matches(closure, registers) || !self.valid() {
            return false;
        }
        for (slot, dest) in self.slots[..self.count]
            .iter()
            .copied()
            .zip(registers.stack_frame.iter_mut())
        {
            slot.write_back(dest);
        }
        true
    }

    pub(super) fn slots_mut(&mut self) -> &mut [Slot] {
        assert!(self.ready);
        &mut self.slots[..self.count]
    }

    pub(super) fn prepare_call(
        &mut self,
        closure: Closure<'gc>,
        registers: &mut LuaRegisters<'gc, '_>,
        function: u8,
        arguments: u8,
        capture: UpValue<'gc>,
    ) -> Option<Transfer<'_, 'gc, N>> {
        let tail = usize::from(function);
        if !self.ready
            || !self.matches(closure, registers)
            || !self.valid()
            || tail >= self.count
            || tail + 1 + usize::from(arguments) > self.count
        {
            return None;
        }
        let capture = match registers.projection_origin(capture)? {
            Origin::Register(index, _)
                if index < self.count && self.slots[index].tag == abi::INTEGER =>
            {
                Some(index)
            }
            Origin::Upper(_, crate::Value::Integer(_)) => None,
            _ => return None,
        };
        if let Some(index) = capture.filter(|index| *index < tail) {
            self.slots[index].write_back(&mut registers.stack_frame[index]);
        }
        for (slot, dest) in self.slots[tail..self.count]
            .iter()
            .copied()
            .zip(registers.stack_frame[tail..].iter_mut())
        {
            slot.write_back(dest);
        }
        self.ready = false;
        Some(Transfer {
            shadow: self,
            tail,
            capture,
        })
    }
}

impl<'gc, const N: usize> Transfer<'_, 'gc, N> {
    fn recover_prefix(
        self,
        closure: Closure<'gc>,
        key: (usize, usize),
        values: &mut [crate::Value<'gc>],
    ) -> bool {
        if self.shadow.ready
            || !ottavino_gc_arena::Gc::ptr_eq(
                self.shadow.closure.into_inner(),
                closure.into_inner(),
            )
            || self.shadow.key != key
            || values.len() != self.tail
            || !self.shadow.valid()
        {
            return false;
        }
        for (index, (slot, value)) in self.shadow.slots[..self.tail]
            .iter()
            .copied()
            .zip(values)
            .enumerate()
        {
            if self.capture != Some(index) {
                slot.write_back(value);
            }
        }
        true
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(super) fn recover(
        self,
        host: &mut crate::thread::activation::ActivationHost<'gc, '_>,
        caller: &crate::thread::activation::CallerFrame<'gc>,
    ) -> bool {
        host.with_caller_prefix(caller, self.tail, |closure, key, values| {
            self.recover_prefix(closure, key, values)
        }) == Some(true)
    }

    pub(super) fn resume(self, closure: Closure<'gc>, registers: &LuaRegisters<'gc, '_>) -> bool {
        if self.shadow.ready || !self.shadow.matches(closure, registers) {
            return false;
        }
        for (slot, value) in self.shadow.slots[self.tail..self.shadow.count]
            .iter_mut()
            .zip(registers.stack_frame[self.tail..].iter().copied())
        {
            *slot = Slot::from_value(value);
        }
        if let Some(index) = self.capture.filter(|index| *index < self.tail) {
            self.shadow.slots[index] = Slot::from_value(registers.stack_frame[index]);
        }
        self.shadow.ready = true;
        true
    }
}

#[test]
fn recovery_materializes_pending_prefix_without_overwriting_capture_effects() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let mut pc = 0;
        let mut values = [crate::Value::Integer(10); 8];
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let capture = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            let key = registers.shadow_key();
            let mut shadow = Shadow::<8>::capture(closure, &registers).unwrap();
            shadow.slots_mut()[0] = Slot::from_value(crate::Value::Integer(42));
            shadow.slots_mut()[2] = Slot::from_value(crate::Value::Integer(99));
            let transfer = shadow
                .prepare_call(closure, &mut registers, 6, 1, capture)
                .unwrap();
            registers.stack_frame[0] = crate::Value::Integer(45);
            registers.stack_frame[6] = crate::Value::Integer(123);
            assert!(transfer.recover_prefix(closure, key, &mut registers.stack_frame[..6]));
            assert!(matches!(
                registers.stack_frame[0],
                crate::Value::Integer(45)
            ));
            assert!(matches!(
                registers.stack_frame[2],
                crate::Value::Integer(99)
            ));
            assert!(matches!(
                registers.stack_frame[6],
                crate::Value::Integer(123)
            ));
            assert!(!shadow.flush(closure, &mut registers));
        });
    });
}

#[test]
fn refused_recovery_never_materializes_or_revalidates_pending_values() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let foreign = Closure::load(ctx, None, b"return 1").unwrap();
        for case in 0..3 {
            let mut pc = 0;
            let mut values = [crate::Value::Integer(10); 8];
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let capture = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
                let mut key = registers.shadow_key();
                let mut shadow = Shadow::<8>::capture(closure, &registers).unwrap();
                shadow.slots_mut()[2] = Slot::from_value(crate::Value::Integer(99));
                let transfer = shadow
                    .prepare_call(closure, &mut registers, 6, 1, capture)
                    .unwrap();
                if case == 1 {
                    key.1 += 1;
                }
                assert!(!transfer.recover_prefix(
                    if case == 0 { foreign } else { closure },
                    key,
                    &mut registers.stack_frame[..if case == 2 { 5 } else { 6 }],
                ));
                assert!(matches!(
                    registers.stack_frame[2],
                    crate::Value::Integer(10)
                ));
                assert!(!shadow.flush(closure, &mut registers));
            });
        }
    });
}

#[test]
fn dropped_transfer_never_revalidates_stale_caller_slots() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let mut pc = 0;
        let mut values = [crate::Value::Integer(10); 8];
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let capture = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            let mut shadow = Shadow::<8>::capture(closure, &registers).unwrap();
            let transfer = shadow
                .prepare_call(closure, &mut registers, 6, 1, capture)
                .unwrap();
            registers.stack_frame[0] = crate::Value::Integer(99);
            drop(transfer);
            assert!(!shadow.flush(closure, &mut registers));
            assert!(shadow
                .prepare_call(closure, &mut registers, 6, 1, capture)
                .is_none());
            assert!(matches!(
                registers.stack_frame[0],
                crate::Value::Integer(99)
            ));
        });
    });
}

#[test]
fn call_transfer_preserves_pending_scalars_and_refreshes_callee_effects() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let table = crate::Table::new(&ctx);
        let mut pc = 0;
        let mut values = [crate::Value::Integer(10); 8];
        values[1] = table.into();
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let capture = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            let mut shadow = Shadow::<8>::capture(closure, &registers).unwrap();
            shadow.slots_mut()[0] = Slot::from_value(crate::Value::Integer(42));
            shadow.slots[2] = Slot::from_value(crate::Value::Integer(99));
            shadow.slots[5] = Slot::from_value(crate::Value::Integer(111));
            let transfer = shadow
                .prepare_call(closure, &mut registers, 6, 1, capture)
                .unwrap();
            assert!(matches!(
                registers.stack_frame[0],
                crate::Value::Integer(42)
            ));
            assert!(matches!(
                registers.stack_frame[2],
                crate::Value::Integer(10)
            ));
            assert!(matches!(
                registers.stack_frame[5],
                crate::Value::Integer(10)
            ));
            registers.stack_frame[0] = crate::Value::Integer(45);
            registers.stack_frame[6] = crate::Value::Nil;
            registers.stack_frame[7] = crate::Value::Nil;
            assert!(transfer.resume(closure, &registers));
            assert!(shadow.flush(closure, &mut registers));
            assert!(matches!(
                registers.stack_frame[0],
                crate::Value::Integer(45)
            ));
            assert!(
                matches!(registers.stack_frame[1], crate::Value::Table(value) if value == table)
            );
            assert!(matches!(
                registers.stack_frame[2],
                crate::Value::Integer(99)
            ));
            assert!(matches!(
                registers.stack_frame[5],
                crate::Value::Integer(111)
            ));
            assert!(matches!(registers.stack_frame[6], crate::Value::Nil));
            assert!(matches!(registers.stack_frame[7], crate::Value::Nil));
            assert_eq!(*registers.pc, 0);
        });
    });
}

#[test]
fn overlapping_capture_and_argument_tail_rebind_after_return() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let mut pc = 0;
        let mut values = [crate::Value::Integer(10); 8];
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let capture = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(7));
            let mut shadow = Shadow::<8>::capture(closure, &registers).unwrap();
            shadow.slots[7] = Slot::from_value(crate::Value::Integer(-7));
            let transfer = shadow
                .prepare_call(closure, &mut registers, 6, 1, capture)
                .unwrap();
            assert!(matches!(
                registers.stack_frame[7],
                crate::Value::Integer(-7)
            ));
            registers.stack_frame[6] = crate::Value::Nil;
            registers.stack_frame[7] = crate::Value::Nil;
            assert!(transfer.resume(closure, &registers));
            assert_eq!(shadow.slots[7].tag, abi::NIL);
            assert!(shadow.flush(closure, &mut registers));
            assert!(matches!(registers.stack_frame[7], crate::Value::Nil));
        });
    });
}

#[test]
fn malformed_transfer_and_foreign_caller_decline_before_materialization() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let foreign = Closure::load(ctx, None, b"return 1").unwrap();
        let mut pc = 0;
        let mut values = [crate::Value::Integer(10); 8];
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let capture = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            assert!(Shadow::<7>::capture(closure, &registers).is_none());
            let mut shadow = Shadow::<8>::capture(closure, &registers).unwrap();
            shadow.slots[0] = Slot::from_value(crate::Value::Integer(99));
            for (caller, function, arguments) in [(closure, 8, 0), (closure, 7, 1), (foreign, 6, 1)]
            {
                assert!(shadow
                    .prepare_call(caller, &mut registers, function, arguments, capture)
                    .is_none());
                assert!(registers
                    .stack_frame
                    .iter()
                    .all(|value| matches!(*value, crate::Value::Integer(10))));
            }
            shadow.slots[5].tag = u64::MAX;
            assert!(shadow
                .prepare_call(closure, &mut registers, 6, 1, capture)
                .is_none());
            assert!(!shadow.flush(closure, &mut registers));
            assert!(registers
                .stack_frame
                .iter()
                .all(|value| matches!(*value, crate::Value::Integer(10))));
        });
    });
}

#[test]
fn closed_capture_and_wrong_stack_decline_without_reusing_shadow() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return 1").unwrap();
        let capture = UpValue::new(
            &ctx,
            crate::closure::UpValueState::Closed(crate::Value::Integer(10)),
        );
        let mut pc = 0;
        let mut values = [crate::Value::Integer(10); 8];
        let mut shadow =
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut shadow = Shadow::<8>::capture(closure, &registers).unwrap();
                shadow.slots[0] = Slot::from_value(crate::Value::Integer(99));
                assert!(shadow
                    .prepare_call(closure, &mut registers, 6, 1, capture)
                    .is_none());
                shadow
            });
        let mut other_pc = 0;
        let mut other = [crate::Value::Integer(20); 8];
        LuaRegisters::with_test_frame(ctx, &mut other_pc, &mut other, |mut registers| {
            assert!(!shadow.flush(closure, &mut registers));
            let capture = registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            assert!(shadow
                .prepare_call(closure, &mut registers, 6, 1, capture)
                .is_none());
            assert!(registers
                .stack_frame
                .iter()
                .all(|value| matches!(*value, crate::Value::Integer(20))));
        });
    });
}
