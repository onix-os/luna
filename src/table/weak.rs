//! A value held weakly, for tables declared `__mode = "v"`.
//!
//! The obvious way to build a weak table is to keep holding the strong `Value` and teach the
//! collector not to trace it, then clear the dead entries afterwards. Do not do that: if the
//! clearing is ever wrong — a missed sweep, a read in the window, a resurrection in between — the
//! table holds a dangling pointer, and a collected `Gc` cannot even be safely inspected to find out
//! that it died. That is unsoundness, not a bug.
//!
//! Holding a `GcWeak` instead makes the safety a fact about the type rather than a discipline: the
//! only way to read the value is `upgrade`, which answers `None` when it is gone. Clearing is then
//! just "upgrade failed, drop the entry", and it can happen lazily, whenever the entry is next
//! looked at.

use ottavino_gc_arena::{Collect, Gc, GcWeak, Mutation};

use crate::{
    closure::ClosureInner, string::StringInner, table::TableInner, thread::ThreadInner,
    userdata::UserDataInner, Callback, Closure, Function, String, Table, Thread, UserData, Value,
};

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum WeakValue<'gc> {
    /// Values with nothing behind them to collect. Held as they are, because they cannot die.
    Immediate(Value<'gc>),
    String(GcWeak<'gc, StringInner>),
    Table(GcWeak<'gc, TableInner<'gc>>),
    Closure(GcWeak<'gc, ClosureInner<'gc>>),
    Callback(Callback<'gc>),
    Thread(GcWeak<'gc, ThreadInner<'gc>>),
    UserData(GcWeak<'gc, UserDataInner<'gc>>),
}

impl<'gc> WeakValue<'gc> {
    /// Hold a value weakly.
    pub fn new(value: Value<'gc>) -> Self {
        match value {
            v @ (Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_)) => {
                WeakValue::Immediate(v)
            }
            Value::String(s) => WeakValue::String(Gc::downgrade(s.into_inner())),
            Value::Table(t) => WeakValue::Table(Gc::downgrade(t.into_inner())),
            Value::Function(Function::Closure(c)) => {
                WeakValue::Closure(Gc::downgrade(c.into_inner()))
            }
            // A callback is a plain function pointer with no separate allocation to lose, so
            // holding it weakly would gain nothing.
            Value::Function(Function::Callback(c)) => WeakValue::Callback(c),
            Value::Thread(t) => WeakValue::Thread(Gc::downgrade(t.into_inner())),
            Value::UserData(u) => WeakValue::UserData(Gc::downgrade(u.into_inner())),
        }
    }

    /// The value, or `None` if it has been collected.
    pub fn get(self, mc: &Mutation<'gc>) -> Option<Value<'gc>> {
        Some(match self {
            WeakValue::Immediate(v) => v,
            WeakValue::String(w) => Value::String(String::from_inner(w.upgrade(mc)?)),
            WeakValue::Table(w) => Value::Table(Table::from_inner(w.upgrade(mc)?)),
            WeakValue::Closure(w) => {
                Value::Function(Function::Closure(Closure::from_inner(w.upgrade(mc)?)))
            }
            WeakValue::Callback(c) => Value::Function(Function::Callback(c)),
            WeakValue::Thread(w) => Value::Thread(Thread::from_inner(w.upgrade(mc)?)),
            WeakValue::UserData(w) => Value::UserData(UserData::from_inner(w.upgrade(mc)?)),
        })
    }

    /// Whether this slot is empty because its value was collected.
    ///
    /// Distinct from holding `Value::Nil`, which is how Lua spells "no entry" and is `Immediate`.
    pub fn is_collected(self, mc: &Mutation<'gc>) -> bool {
        self.get(mc).is_none()
    }
}
