use std::{
    fmt,
    hash::{Hash, Hasher},
    i64, mem,
};

use ottavino_gc_arena::{lock::RefLock, Collect, Gc, Mutation};

use crate::{Context, FromValue, IntoValue, TypeError, Value};

use super::raw::{InvalidTableKey, NextValue, RawTable};

pub type TableInner<'gc> = RefLock<TableState<'gc>>;

/// The primary Lua data structure.
///
/// A `Table` is a combination of an array and a map. It a map of [`Value`]s to other `Value`s, but
/// as an optimization, when keys are integral and start from 1, it stores them within an internal
/// array.
///
/// Entries with values of [`Value::Nil`] are transparently removed from the table.
///
/// When a `Table` has only integral keys starting from 1, and every value is non-nil, then it is
/// also known as a "sequence" and acts similar to how an array would in other languages.
///
/// All Lua tables can have another associated table called a "metatable" which governs how they
/// act in Lua code. In Lua code, operations on a table can trigger special "metamethods" in the
/// metatable (if they are present).
///
/// On the Rust side, all methods on `Table` are "raw", which in Lua jargon means that they
/// never trigger metamethods. This MUST be true, because `luna` does not (and cannot)
/// silently trigger running Lua code. In order to trigger metamethods, you must use the
/// [`meta_ops`](crate::meta_ops) module and manually run any triggered code on an
/// [`Executor`](crate::Executor).
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Table<'gc>(Gc<'gc, TableInner<'gc>>);

impl<'gc> PartialEq for Table<'gc> {
    fn eq(&self, other: &Table<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Table<'gc> {}

impl<'gc> Hash for Table<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state)
    }
}

impl<'gc> Table<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> Table<'gc> {
        Self::from_parts(mc, RawTable::new(mc), None)
    }

    pub fn from_parts(
        mc: &Mutation<'gc>,
        raw_table: RawTable<'gc>,
        metatable: Option<Table<'gc>>,
    ) -> Table<'gc> {
        Self(Gc::new(
            mc,
            RefLock::new(TableState {
                raw_table,
                metatable,
                readonly: false,
                intercept_all_writes: false,
            }),
        ))
    }

    pub fn from_inner(inner: Gc<'gc, TableInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, TableInner<'gc>> {
        self.0
    }

    pub fn get<K: IntoValue<'gc>, V: FromValue<'gc>>(
        self,
        ctx: Context<'gc>,
        key: K,
    ) -> Result<V, TypeError> {
        V::from_value(ctx, self.get_value(ctx, key))
    }

    pub fn set<K: IntoValue<'gc>, V: IntoValue<'gc>>(
        self,
        ctx: Context<'gc>,
        key: K,
        value: V,
    ) -> Result<Value<'gc>, InvalidTableKey> {
        self.set_raw(&ctx, key.into_value(ctx), value.into_value(ctx))
    }

    pub fn get_value<K: IntoValue<'gc>>(self, ctx: Context<'gc>, key: K) -> Value<'gc> {
        self.get_raw(&ctx, key.into_value(ctx))
    }

    /// A convenience method over [`Table::set`] for setting a string field of a table.
    ///
    /// It behaves exactly the same as [`Table::set`], except since this only accepts string keys,
    /// we know it cannot possibly error.
    pub fn set_field<V: IntoValue<'gc>>(
        self,
        ctx: Context<'gc>,
        key: &'static str,
        value: V,
    ) -> Value<'gc> {
        self.set(ctx, key, value).unwrap()
    }

    /// Get a value from this table without any automatic type conversion.
    ///
    /// Takes a `Mutation` because a table declared `__mode = "v"` holds its values weakly, and the
    /// only safe way to read one is to upgrade it.
    pub fn get_raw(self, mc: &Mutation<'gc>, key: Value<'gc>) -> Value<'gc> {
        self.0.borrow().raw_table.get(mc, key)
    }

    /// Set a value in this table without any automatic type conversion.
    pub fn set_raw(
        self,
        mc: &Mutation<'gc>,
        key: Value<'gc>,
        value: Value<'gc>,
    ) -> Result<Value<'gc>, InvalidTableKey> {
        if self.0.borrow().readonly {
            return Err(InvalidTableKey::ReadOnly);
        }
        self.0.borrow_mut(&mc).raw_table.set(&mc, key, value)
    }

    /// Remove every entry, releasing the memory rather than leaving tombstones behind.
    ///
    /// Setting keys to nil one at a time leaves `Key::Dead` markers and keeps the buckets
    /// allocated, which is the right trade during iteration but wrong for emptying a table you
    /// intend to reuse.
    pub fn clear(self, mc: &Mutation<'gc>) -> Result<(), InvalidTableKey> {
        if self.0.borrow().readonly {
            return Err(InvalidTableKey::ReadOnly);
        }
        self.0.borrow_mut(mc).raw_table.clear();
        Ok(())
    }
    /// Whether this table refuses writes.
    pub fn is_readonly(self) -> bool {
        self.0.borrow().readonly
    }

    /// Whether `__newindex` fires on every store rather than only on absent keys.
    pub fn intercepts_all_writes(self) -> bool {
        self.0.borrow().intercept_all_writes
    }

    /// Make `__newindex` fire on every store, including keys the table already has.
    ///
    /// Lua fires `__newindex` only for absent keys, which is right when a metatable is a fallback.
    /// It is wrong when the metatable is deciding *where* a value belongs: a name assigned a table
    /// and then a string would stay put instead of moving. A host building a namespace out of a
    /// table wants this; anything modelling plain Lua does not.
    pub fn set_intercept_all_writes(self, mc: &Mutation<'gc>, intercept: bool) {
        self.0.borrow_mut(mc).intercept_all_writes = intercept;
    }

    /// Freeze or unfreeze this table.
    ///
    /// A frozen table refuses every write, raw ones included, so freezing the stdlib tables stops
    /// one script rewriting `string.format` for every other script sharing the globals.
    pub fn set_readonly(self, mc: &Mutation<'gc>, readonly: bool) {
        self.0.borrow_mut(mc).readonly = readonly;
    }

    /// Returns a 'border' for this table.
    ///
    /// A 'border' for a table is any i >= 0 where:
    /// `(i == 0 or table[i] ~= nil) and table[i + 1] == nil`
    ///
    /// If a table has exactly one border, it is called a 'sequence', and this border is the table's
    /// length.
    pub fn length(self, mc: &Mutation<'gc>) -> i64 {
        self.0.borrow().raw_table.length(mc)
    }

    /// Returns the next value after this key in table iteration order.
    ///
    /// The array portion comes first, in index order, followed by the map portion **in the order
    /// its keys were first inserted**. Lua itself promises no particular order for `pairs`, so this
    /// is a guarantee luna adds: it means a table used to build an ordered structure — a row whose
    /// column order is its key order, say — reads back the way it was written, and reads the same
    /// way on every run. A key removed and later re-added keeps its original position.
    ///
    /// The order is only guaranteed to be stable if there are no inserts into the table, so
    /// iterating using this method while simultaneously inserting may result in unspecified (but
    /// never unsafe) behavior. As a special case, removing from the table (setting a value to
    /// [`Value::Nil`]) is *always* allowed, even for the currently returned key.
    ///
    /// If given `Nil`, it will return the first pair in the table. If given a key that is present
    /// in the table, it will return the next pair in iteration order. If given a key that is not
    /// present in the table, the behavior is unspecified.
    pub fn next(self, mc: &Mutation<'gc>, key: Value<'gc>) -> NextValue<'gc> {
        self.0.borrow().raw_table.next(mc, key)
    }

    /// Iterate over the key-value pairs of the table.
    ///
    /// Internally uses the `Table::next` method and thus matches the behavior of Lua.
    pub fn iter(self, ctx: Context<'gc>) -> Iter<'gc> {
        Iter::new(ctx, self)
    }

    /// Revive the values of entries whose key is still alive. See `Finalizers::mark_ephemerons`.
    pub(crate) fn revive_live_entries(
        self,
        fc: &ottavino_gc_arena::Finalization<'gc>,
        roots: &mut Vec<Value<'gc>>,
    ) -> usize {
        self.0.borrow().raw_table.revive_live_entries(fc, roots)
    }

    pub fn metatable(self) -> Option<Table<'gc>> {
        self.0.borrow().metatable
    }

    /// Attach a metatable.
    ///
    /// Takes `Context` rather than `Mutation` because a metatable carrying `__gc` enrols this
    /// table with the finalizer registry.
    pub fn set_metatable(
        self,
        ctx: Context<'gc>,
        metatable: Option<Table<'gc>>,
    ) -> Option<Table<'gc>> {
        if let Some(mt) = metatable {
            if !mt.get_value(ctx, crate::MetaMethod::Gc).is_nil() {
                ctx.finalizers().register_table(&ctx, self.0);
            }
            // `__mode` is read once, here. Changing it afterwards does not retroactively weaken
            // entries — PUC-Rio calls that undefined, and this is luna's answer.
            if let Value::String(mode) = mt.get_value(ctx, crate::MetaMethod::Mode) {
                let mode = mode.as_bytes();
                if mode.contains(&b'k') {
                    self.0.borrow_mut(&ctx).raw_table.make_keys_weak(&ctx);
                    // Ephemeron revival is for `"k"` alone. Under `"kv"` the value is weak in its
                    // own right and must die when nothing else holds it, however alive its key is,
                    // so putting values back would be exactly wrong.
                    if !mode.contains(&b'v') {
                        ctx.finalizers().register_weak_keys(&ctx, self.0);
                    }
                } else if mode.contains(&b'v') {
                    self.0.borrow_mut(&ctx).raw_table.make_values_weak(&ctx);
                }
            }
        }
        mem::replace(&mut self.0.borrow_mut(&ctx).metatable, metatable)
    }
}

/// Iteration over a table's entries.
///
/// Not `Collect`: it holds a `Context`, which is a borrow into the arena rather than something
/// collected, and an iterator does not outlive the `enter` that made it.
#[derive(Copy, Clone)]
pub struct Iter<'gc> {
    // Held because a weak table's slots can only be read by upgrading them.
    ctx: Context<'gc>,
    table: Table<'gc>,
    prev: Option<Value<'gc>>,
}

impl<'gc> Iter<'gc> {
    pub fn new(ctx: Context<'gc>, table: Table<'gc>) -> Self {
        Self {
            ctx,
            table,
            prev: Some(Value::Nil),
        }
    }
}

impl<'gc> Iterator for Iter<'gc> {
    type Item = (Value<'gc>, Value<'gc>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.table.next(&self.ctx, self.prev.take()?) {
            NextValue::Found { key, value } => {
                self.prev = Some(key);
                Some((key, value))
            }
            _ => None,
        }
    }
}

#[derive(Collect)]
#[collect(no_drop)]
pub struct TableState<'gc> {
    pub raw_table: RawTable<'gc>,
    pub metatable: Option<Table<'gc>>,
    // A frozen table refuses every write, including raw ones. The stdlib tables live in shared
    // globals, so without this one script can replace `string.format` for every other script.
    pub readonly: bool,
    // When set, `__newindex` fires on every store, not only on keys the table lacks.
    pub intercept_all_writes: bool,
}

impl<'gc> fmt::Debug for TableState<'gc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct ShallowTableDebug<'gc>(Table<'gc>);

        impl<'gc> fmt::Debug for ShallowTableDebug<'gc> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let t = self.0.into_inner().borrow();
                match t.metatable {
                    Some(meta) => f
                        .debug_struct("TableState")
                        .field("raw_table", &t.raw_table)
                        .field(
                            "metatable",
                            &Some(format_args!("Table({:p})", Gc::as_ptr(meta.into_inner()))),
                        )
                        .finish(),
                    None => f
                        .debug_struct("TableState")
                        .field("raw_table", &t.raw_table)
                        .field("metatable", &None::<()>)
                        .finish(),
                }
            }
        }

        f.debug_struct("TableState")
            .field("raw_table", &self.raw_table)
            .field(
                "metatable",
                &self.metatable.as_ref().map(|t| ShallowTableDebug(*t)),
            )
            .finish()
    }
}
