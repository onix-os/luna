use std::cell::Cell;

use super::*;
use crate::{IntoValue, MetaMethod};

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static HITS: Cell<u64> = const { Cell::new(0) };
}

pub(in crate::thread) fn scoped<R>(enabled: bool, run: impl FnOnce() -> R) -> (R, u64) {
    struct Restore(bool, u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.set(self.0);
            HITS.set(self.1);
        }
    }
    let _restore = Restore(ENABLED.replace(enabled), HITS.replace(0));
    let result = run();
    (result, HITS.get())
}

impl<'gc> LuaFrame<'gc, '_> {
    #[inline(never)]
    pub(in crate::thread) fn try_set_metatable(
        &mut self,
        ctx: Context<'gc>,
        function: RegisterIndex,
        arguments: VarCount,
        returns: Option<VarCount>,
        prefix: u32,
    ) -> bool {
        if !ENABLED.get()
            || arguments.to_constant() != Some(2)
            || returns.is_some_and(|count| count.to_constant() != Some(1))
            || prefix > 64
            || ctx.hook_enabled()
            || self.state.running
            || self.state.closing
            || self.state.deferred_error
        {
            return false;
        }
        let Some(Frame::Lua {
            bottom,
            base,
            stack_size,
            is_variable: false,
            expected_return: None,
            ..
        }) = self.state.frames.last()
        else {
            return false;
        };
        let (bottom, base, width) = (*bottom, *base, *stack_size);
        let Some(start) = base.checked_add(usize::from(function.0)) else {
            return false;
        };
        if self.stack.len() != base + width
            || start
                .checked_add(3)
                .is_none_or(|end| end > self.stack.len())
        {
            return false;
        }
        let tail = returns.is_none();
        let depth = self.state.frames.len() - usize::from(tail);
        if depth >= self.state.max_call_depth || depth >= self.state.frames.capacity() {
            return false;
        }
        let capture_limit = if tail { bottom } else { start };
        if self.state.open_upvalues.iter().any(|cell| {
            !matches!(cell.get(), UpValueState::Open(open) if open.stack_index < capture_limit)
        }) || self.state.to_be_closed.iter().any(|&index| index >= capture_limit) {
            return false;
        }
        let [Value::Function(Function::Callback(callback)), Value::Table(table), metatable] =
            self.stack[start..start + 3]
        else {
            return false;
        };
        let metatable = match metatable {
            Value::Nil => None,
            Value::Table(table) => Some(table),
            _ => return false,
        };
        if !ctx.jit_registry().borrow().is_set_metatable(ctx, callback) {
            return false;
        }
        let mut boundary = self.fuel.clone();
        let call_fuel = Self::FUEL_PER_CALL + 2 * Self::FUEL_PER_ITEM;
        boundary.consume(call_fuel);
        boundary.consume(prefix as i32);
        boundary.consume(super::super::Executor::FUEL_PER_STEP);
        if !boundary.should_continue() {
            return false;
        }
        let Ok(mut target) = table.into_inner().try_borrow_mut(&ctx) else {
            return false;
        };
        let read = |source: Table<'gc>, method: MetaMethod| {
            let key = method.into_value(ctx);
            if source == table {
                Some(target.raw_table.get(&ctx, key))
            } else {
                Some(
                    source
                        .into_inner()
                        .try_borrow()
                        .ok()?
                        .raw_table
                        .get(&ctx, key),
                )
            }
        };
        if target.metatable.is_some_and(|existing| {
            read(existing, MetaMethod::Metatable).is_none_or(|value| !value.is_nil())
        }) || metatable.is_some_and(|new| {
            [MetaMethod::Gc, MetaMethod::Mode]
                .into_iter()
                .any(|method| read(new, method).is_none_or(|value| !value.is_nil()))
        }) {
            return false;
        }
        target.metatable = metatable;
        drop(target);
        self.fuel.consume(call_fuel);
        self.fuel.consume(super::super::Executor::FUEL_PER_CALLBACK);
        self.fuel.consume(super::super::Executor::FUEL_PER_STEP);
        if tail {
            self.state.frames.pop();
            self.stack.truncate(bottom);
            self.stack.push(Value::Table(table));
            self.state.return_to(self.stack, bottom);
        } else {
            self.stack[start] = Value::Table(table);
            self.stack[start + 1..].fill(Value::Nil);
        }
        HITS.set(HITS.get() + 1);
        true
    }
}

#[cfg(test)]
mod tests;
