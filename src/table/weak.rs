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

/// A *key* held weakly, for tables declared `__mode = "k"`.
///
/// Keys need one thing values do not: an address. The table's map compares a stored key against an
/// incoming one, and a weak key must answer that comparison without upgrading — the entry may be
/// looked up after its key has died, and the answer is still "not this one". `as_ptr` gives the
/// address of a `GcWeak` whether or not the object is still there, which is exactly the same trick
/// the table already uses for keys removed during iteration.
///
/// Keys with nothing behind them — booleans, numbers, and callbacks, which are bare function
/// pointers — are held as they are. Making them weak would gain nothing because they cannot die.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum WeakKey<'gc> {
    Immediate(CanonicalKeyRepr<'gc>),
    String(GcWeak<'gc, StringInner>),
    Table(GcWeak<'gc, TableInner<'gc>>),
    Closure(GcWeak<'gc, ClosureInner<'gc>>),
    Thread(GcWeak<'gc, ThreadInner<'gc>>),
    UserData(GcWeak<'gc, UserDataInner<'gc>>),
}

/// The keys that are never worth holding weakly, kept in their strong form.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum CanonicalKeyRepr<'gc> {
    Boolean(bool),
    Integer(i64),
    Number(u64),
    Callback(Callback<'gc>),
}

impl<'gc> WeakKey<'gc> {
    /// The address this key hashes and compares by, or `None` if it has no separate allocation.
    pub fn as_ptr(self) -> Option<*const ()> {
        Some(match self {
            WeakKey::Immediate(_) => return None,
            WeakKey::String(w) => w.as_ptr() as *const (),
            WeakKey::Table(w) => w.as_ptr() as *const (),
            WeakKey::Closure(w) => w.as_ptr() as *const (),
            WeakKey::Thread(w) => w.as_ptr() as *const (),
            WeakKey::UserData(w) => w.as_ptr() as *const (),
        })
    }

    /// Whether the key is still reachable *without* going through this table.
    ///
    /// This is the ephemeron question, and the only reason weak keys need collector cooperation at
    /// all: an entry survives exactly when its key does.
    pub fn is_live(self, mc: &Mutation<'gc>) -> bool {
        match self {
            WeakKey::Immediate(_) => true,
            WeakKey::String(w) => w.upgrade(mc).is_some(),
            WeakKey::Table(w) => w.upgrade(mc).is_some(),
            WeakKey::Closure(w) => w.upgrade(mc).is_some(),
            WeakKey::Thread(w) => w.upgrade(mc).is_some(),
            WeakKey::UserData(w) => w.upgrade(mc).is_some(),
        }
    }
}

impl<'gc> WeakKey<'gc> {
    /// The key as a value, or `None` if the object it named has been collected.
    pub fn to_value(self, mc: &Mutation<'gc>) -> Option<Value<'gc>> {
        Some(match self {
            WeakKey::Immediate(CanonicalKeyRepr::Boolean(b)) => Value::Boolean(b),
            WeakKey::Immediate(CanonicalKeyRepr::Integer(i)) => Value::Integer(i),
            WeakKey::Immediate(CanonicalKeyRepr::Number(n)) => Value::Number(f64::from_bits(n)),
            WeakKey::Immediate(CanonicalKeyRepr::Callback(c)) => {
                Value::Function(Function::Callback(c))
            }
            WeakKey::String(w) => Value::String(String::from_inner(w.upgrade(mc)?)),
            WeakKey::Table(w) => Value::Table(Table::from_inner(w.upgrade(mc)?)),
            WeakKey::Closure(w) => {
                Value::Function(Function::Closure(Closure::from_inner(w.upgrade(mc)?)))
            }
            WeakKey::Thread(w) => Value::Thread(Thread::from_inner(w.upgrade(mc)?)),
            WeakKey::UserData(w) => Value::UserData(UserData::from_inner(w.upgrade(mc)?)),
        })
    }
}

impl<'gc> WeakKey<'gc> {
    /// Whether this key is about to be collected, asked during finalization.
    pub fn is_dead(self, fc: &ottavino_gc_arena::Finalization<'gc>) -> bool {
        match self {
            WeakKey::Immediate(_) => false,
            WeakKey::String(w) => w.is_dead(fc),
            WeakKey::Table(w) => w.is_dead(fc),
            WeakKey::Closure(w) => w.is_dead(fc),
            WeakKey::Thread(w) => w.is_dead(fc),
            WeakKey::UserData(w) => w.is_dead(fc),
        }
    }
}

impl<'gc> WeakValue<'gc> {
    /// Mark this value live, if it is not already.
    ///
    /// The ephemeron step: a weak-key table's values are held weakly so that marking does not
    /// reach a key *through* its own value — the cycle that makes a naive weak-key table leak —
    /// and are then resurrected here for exactly those entries whose key survived on its own.
    ///
    /// Returns whether anything changed, which is what lets the caller iterate to a fixed point:
    /// resurrecting a value can make another table's key reachable, which keeps *its* value, and
    /// so on until nothing new appears.
    pub fn resurrect_if_dead(self, fc: &ottavino_gc_arena::Finalization<'gc>) -> bool {
        fn revive<'gc, T>(w: GcWeak<'gc, T>, fc: &ottavino_gc_arena::Finalization<'gc>) -> bool {
            if w.is_dead(fc) {
                w.resurrect(fc);
                true
            } else {
                false
            }
        }

        match self {
            WeakValue::Immediate(_) | WeakValue::Callback(_) => false,
            WeakValue::String(w) => revive(w, fc),
            WeakValue::Table(w) => revive(w, fc),
            WeakValue::Closure(w) => revive(w, fc),
            WeakValue::Thread(w) => revive(w, fc),
            WeakValue::UserData(w) => revive(w, fc),
        }
    }
}
