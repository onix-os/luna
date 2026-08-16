use std::{fmt, hash::Hash, i64, mem};

use allocator_api2::vec;
use hashbrown::{hash_map, HashMap};
use ottavino_gc_arena::{allocator_api::MetricsAlloc, Collect, Gc, Mutation};
use thiserror::Error;

use crate::{
    table::CanonicalKeyRepr,
    table::{WeakKey, WeakValue},
    Callback, Closure, Function, String, Table, Thread, UserData, Value,
};

#[derive(Debug, Copy, Clone, Error)]
pub enum InvalidTableKey {
    #[error("table key is NaN")]
    IsNaN,
    #[error("table key is Nil")]
    IsNil,
    #[error("table is read-only")]
    ReadOnly,
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum NextValue<'gc> {
    /// The key provided to [`Table::next`] was found and there is an element present after it.
    ///
    /// As a special case, this may be returned if the key provided to [`Table::next`] was an
    /// integer that is a valid index into the array part of the table, *whether or not* that key
    /// is actually present. This matches the behavior of PUC-Rio Lua and is necessary to allow any
    /// field to be set to `Nil` during iteration.
    Found { key: Value<'gc>, value: Value<'gc> },
    /// The key provided to [`Table::next`] was found and it was the last entry in iteration order.
    Last,
    /// The key provided to [`Table::next`] did not match an existing entry.
    NotFound,
}

#[derive(Collect)]
#[collect(no_drop)]
pub struct RawTable<'gc> {
    array: vec::Vec<Value<'gc>, MetricsAlloc<'gc>>,
    // TODO: It would be safer to use `hashbrown::HashTable` and access the inner raw table when
    // necessary, but `HashTable` does not allow access to the inner raw table yet.
    // Each entry carries its position in `order` beside its value, so that `next` can resume from
    // any key without searching.
    map: HashMap<Key<'gc>, (Slot<'gc>, usize), (), MetricsAlloc<'gc>>,
    // The map part's keys in the order they were first inserted, which is the order `next` walks.
    //
    // Lua promises nothing about `pairs` order, but a hash order seeded per process means the same
    // table iterates differently between runs of the same binary, which is no use to anything
    // building an ordered structure out of a table. Slots for removed keys are left in place and
    // skipped, and compacted when the map is next grown.
    // Holds `Key` rather than `CanonicalKey` so that a weak-key table's order does not itself keep
    // the keys alive — a strong entry here would defeat the whole point of `__mode = "k"`.
    order: vec::Vec<Key<'gc>, MetricsAlloc<'gc>>,
    // Set by `__mode = "v"`. Weak tables keep everything in the map part, so there is no second
    // representation to keep in step.
    #[collect(require_static)]
    weak_values: bool,
    // Set by `__mode = "k"`. Entries whose key has been collected read as absent and are dropped
    // when next encountered.
    #[collect(require_static)]
    weak_keys: bool,
}

/// The seed every table hashes with.
///
/// One per process rather than one per table. The randomness is what makes a table resistant to a
/// caller feeding it colliding keys, and a single unpredictable seed does that just as well as a
/// million of them — PUC-Rio keeps one per state for the same reason. Per-table cost 32 bytes and
/// ~18ns of construction, measured, on an empty table that is only ~205 bytes to begin with.
fn hasher() -> &'static ahash::random_state::RandomState {
    static HASHER: std::sync::OnceLock<ahash::random_state::RandomState> =
        std::sync::OnceLock::new();
    HASHER.get_or_init(ahash::random_state::RandomState::new)
}

/// The hash an entry is stored under.
///
/// A key that is not live carries its own, because it cannot be recomputed: a weak key may have
/// lost the object it named, and a tombstone has only an address, which does not hash the way the
/// key it replaced did. Rehashing on growth needs the *original* hash either way.
fn key_hash<'gc>(key: &Key<'gc>) -> u64 {
    match key {
        Key::Live(k) => hasher().hash_one(*k),
        Key::Weak(_, hash) | Key::Dead(_, hash) => *hash,
    }
}

impl<'gc> fmt::Debug for RawTable<'gc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                self.array
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        (
                            Value::Integer((i + 1).try_into().unwrap()).debug_shallow(),
                            v.debug_shallow(),
                        )
                    })
                    .chain({
                        self.map.iter().filter_map(|(k, v)| {
                            if let Key::Live(k) = k {
                                Some((k.to_value().debug_shallow(), v.0.shown().debug_shallow()))
                            } else {
                                None
                            }
                        })
                    }),
            )
            .finish()
    }
}

impl<'gc> RawTable<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> Self {
        Self::with_capacity(mc, 0, 0)
    }

    pub fn with_capacity(mc: &Mutation<'gc>, array_capacity: usize, map_capacity: usize) -> Self {
        let mut array = vec::Vec::new_in(MetricsAlloc::new(mc));
        array.resize(array_capacity, Value::Nil);

        let map = HashMap::with_capacity_and_hasher_in(map_capacity, (), MetricsAlloc::new(mc));

        Self {
            array,
            map,
            order: vec::Vec::new_in(MetricsAlloc::new(mc)),
            weak_values: false,
            weak_keys: false,
        }
    }

    /// Drop order slots whose keys are gone, keeping the surviving keys in their relative order.
    ///
    /// Only safe to call when the map is being rebuilt anyway: it renumbers every entry.
    fn compact_order(&mut self) {
        let map = &mut self.map;
        let mut next = 0;
        self.order.retain(|&key| {
            if matches!(key, Key::Dead(..)) {
                return false;
            }
            let hash = key_hash(&key);
            match map.raw_table_mut().find(hash, |(k, _)| k.same(key)) {
                Some(bucket) => {
                    unsafe { bucket.as_mut() }.1 .1 = next;
                    next += 1;
                    true
                }
                None => false,
            }
        });
    }

    /// Whether the array part answers for this index, or the map must be asked.
    ///
    /// A weak table keeps its entries in the map, so an empty array slot is not an answer for one:
    /// the entry may be in the map, held weakly. Only the array slots a caller filled directly
    /// through [`RawTable::array_mut`] are ever occupied in such a table.
    fn array_answers(&self, index: usize) -> bool {
        index < self.array.len() && (!self.weak_values || !self.array[index].is_nil())
    }

    pub fn get(&self, mc: &Mutation<'gc>, key: Value<'gc>) -> Value<'gc> {
        if let Some(index) = to_array_index(key) {
            if self.array_answers(index) {
                return self.array[index];
            }
        }

        if let Ok(key) = CanonicalKey::new(key) {
            if let Some((_, v)) = self
                .map
                .raw_entry()
                .from_hash(hasher().hash_one(key), |k| k.eq(key))
            {
                v.0.get(mc)
            } else {
                Value::Nil
            }
        } else {
            Value::Nil
        }
    }

    /// Hold this key the way this table holds keys.
    fn wrap_key(&self, key: CanonicalKey<'gc>, hash: u64) -> Key<'gc> {
        if self.weak_keys {
            Key::Weak(weaken(key), hash)
        } else {
            Key::Live(key)
        }
    }

    fn wrap(&self, value: Value<'gc>) -> Slot<'gc> {
        if self.weak_values {
            Slot::Weak(WeakValue::new(value))
        } else {
            Slot::Strong(value)
        }
    }

    pub fn set(
        &mut self,
        mc: &Mutation<'gc>,
        key: Value<'gc>,
        value: Value<'gc>,
    ) -> Result<Value<'gc>, InvalidTableKey> {
        // If the key is an array candidate and less than the current length of the array, it will
        // go there.
        let index_key = to_array_index(key);
        if let Some(index) = index_key {
            if self.array_answers(index) {
                return Ok(mem::replace(&mut self.array[index], value));
            }
        }

        let table_key = CanonicalKey::new(key)?;
        let hash = hasher().hash_one(table_key);

        // If the value is nil then we are removing from the map part, which cannot fail.
        if value.is_nil() {
            return Ok(
                if let hash_map::RawEntryMut::Occupied(mut occupied) = self
                    .map
                    .raw_entry_mut()
                    .from_hash(hash, |k| k.eq(table_key))
                {
                    let (k, v) = occupied.get_key_value_mut();
                    if let Some(dead) = k.kill(hash) {
                        *k = dead;
                    }
                    // The order slot stays, holding this key's position in case it comes back.
                    mem::replace(&mut v.0, Slot::Strong(Value::Nil)).get(mc)
                } else {
                    Value::Nil
                },
            );
        }

        // If there is an existing entry in the map part, replace it, otherwise try to fit a new
        // entry. The slot is built first: `wrap` reads `self`, which the raw-table borrow below
        // would otherwise rule out.
        let slot = self.wrap(value);
        let stored_key = self.wrap_key(table_key, hash);
        let next_order = self.order.len();
        let raw_map = self.map.raw_table_mut();
        if let Some(bucket) = raw_map.find(hash, |(k, _)| k.eq(table_key)) {
            let (k, v) = unsafe { bucket.as_mut() };
            if k.needs_revival(mc) {
                // Resurrect a removed key, held the way this table holds keys — a weak table must
                // get a weak key back, or the entry would never be collected again. It keeps the
                // order slot it had before, which has to be revived alongside it.
                *k = stored_key;
                let position = v.1;
                let previous = mem::replace(&mut v.0, slot);
                self.order[position] = stored_key;
                return Ok(previous.get(mc));
            }
            return Ok(mem::replace(&mut v.0, slot).get(mc));
        } else if raw_map
            .try_insert_no_grow(hash, (stored_key, (slot, next_order)))
            .is_ok()
        {
            self.order.push(stored_key);
            return Ok(Value::Nil);
        }

        // If a new element does not fit in either the array or map part of the table, we need to
        // grow.

        // A weak table keeps everything in the map: the array part holds its values strongly, so an
        // entry moved there would never be released.
        if let Some(index_key) = index_key.filter(|_| !self.weak_values) {
            // We have an array-candidate key, so we'd like to grow the array and place it there,
            // if possible.

            // First, we find the total count of array candidate elements across the array part, the
            // map part, and the newly inserted key.

            const USIZE_BITS: usize = mem::size_of::<usize>() * 8;

            // Count of array-candidate elements based on the highest bit in the index
            let mut array_counts = [0; USIZE_BITS];
            // Total count of all array-candidate elements
            let mut array_total = 0;

            for (i, e) in self.array.iter().enumerate() {
                if !e.is_nil() {
                    array_counts[highest_bit(i)] += 1;
                    array_total += 1;
                }
            }

            // A key with no live form is a weak key holding an object, which is never an array
            // index, so it counts for nothing here.
            for (&key, &(value, _)) in &self.map {
                if !value.is_definitely_empty() {
                    if let Some(i) = key.live_key().and_then(|k| to_array_index(k.to_value())) {
                        array_counts[highest_bit(i)] += 1;
                        array_total += 1;
                    }
                }
            }

            array_counts[highest_bit(index_key)] += 1;
            array_total += 1;

            // Then, we compute the new optimal size for the array by finding the largest array size
            // such that at least half of the elements in the array would be in use.

            let mut optimal_size = 0;
            let mut total = 0;
            for i in 0..USIZE_BITS {
                if (1 << i) / 2 >= array_total {
                    break;
                }

                if array_counts[i] > 0 {
                    total += array_counts[i];
                    if total > (1 << i) / 2 {
                        optimal_size = 1 << i;
                    }
                }
            }

            // If we can fit our new key in an optimally sized array, resize the array and do that.
            if optimal_size > index_key {
                self.grow_array(optimal_size - self.array.len());
                // If the value is non-nil, it should have been replaced in the map part without
                // needing to grow growing.
                debug_assert!(self.array[index_key].is_nil());
                self.array[index_key] = value;
                return Ok(Value::Nil);
            }
        }

        // If we can't grow the array, we need to grow the map and place the key there. We
        // explicitly double the size of the map.
        self.purge_collected(mc);
        self.reserve_map(self.map.len().max(1));

        // Now we can insert the new key value pair
        let slot = self.wrap(value);
        let stored_key = self.wrap_key(table_key, hash);
        self.map
            .raw_table_mut()
            .try_insert_no_grow(hash, (stored_key, (slot, self.order.len())))
            .unwrap();
        self.order.push(stored_key);

        Ok(Value::Nil)
    }

    /// Returns a 'border' for this table.
    ///
    /// See [`Table::length`] for a more full description of what this means.
    pub fn length(&self, mc: &Mutation<'gc>) -> i64 {
        // Binary search for a border. Entry at max must be Nil, min must be 0 or entry at min must
        // be != Nil.
        fn binary_search<F: Fn(i64) -> bool>(mut min: i64, mut max: i64, is_nil: F) -> i64 {
            while max - min > 1 {
                let mid = min + (max - min) / 2;
                if is_nil(mid) {
                    max = mid;
                } else {
                    min = mid;
                }
            }
            min
        }

        let array_len: i64 = self.array.len().try_into().unwrap();

        // A weak table's entries are split between the two parts with no rule about which holds
        // what, so the border has to be searched through the table itself rather than the array.
        if self.weak_values {
            let is_nil = |i: i64| self.get(mc, Value::Integer(i)).is_nil();
            let mut max: i64 = 1;
            while !is_nil(max) {
                if max == i64::MAX {
                    return i64::MAX;
                }
                max = max.checked_mul(2).unwrap_or(i64::MAX);
            }
            return binary_search(0, max, is_nil);
        }

        if !self.array.is_empty() && self.array[array_len as usize - 1].is_nil() {
            // If the array part ends in a Nil, there must be a border inside it
            binary_search(0, array_len, |i| self.array[i as usize - 1].is_nil())
        } else if self.map.is_empty() {
            // If the array part does not end in a nil but the map part is empty, then the array
            // length is a border.
            array_len
        } else {
            // Otherwise, we check the map part for a border. We need to find some nil value in the
            // map part as the max for a binary search.
            let min = array_len;
            let mut max = array_len.checked_add(1).unwrap();
            while self
                .map
                .raw_entry()
                .from_hash(hasher().hash_one(CanonicalKey::Integer(max)), |k| {
                    k.eq(CanonicalKey::Integer(max))
                })
                .is_some_and(|(_, v)| !v.0.is_empty(mc))
            {
                if max == i64::MAX {
                    // If we can't find a nil entry by doubling, then the table is pathological. We
                    // return the favor with a pathological answer: i64::MAX + 1 can't exist in the
                    // table, therefore it is Nil, so since the table contains i64::MAX, i64::MAX is
                    // a border.
                    return i64::MAX;
                } else if let Some(double_max) = max.checked_mul(2) {
                    max = double_max;
                } else {
                    max = i64::MAX;
                }
            }

            // We have found a max where table[max] == nil, so we can now binary search
            binary_search(min, max, |i| {
                match self
                    .map
                    .raw_entry()
                    .from_hash(hasher().hash_one(CanonicalKey::Integer(i)), |k| {
                        k.eq(CanonicalKey::Integer(i))
                    }) {
                    Some((_, v)) => v.0.is_empty(mc),
                    None => true,
                }
            })
        }
    }

    /// Returns the next key for this table in iteration order following the given `key`.
    ///
    /// See [`Table::next`] for a more full description of what this means.
    pub fn next(&self, mc: &Mutation<'gc>, key: Value<'gc>) -> NextValue<'gc> {
        // `start_index` being set means that we will scan for the first non-nil entry greater than
        // or equal to this (0-based) index.
        let start_index = if let Some(index_key) = to_array_index(key) {
            if self.array_answers(index_key) {
                // In order to satisfy our rule that setting an entry to `Nil` is always allowed
                // during iteration, we must allow being provided a missing entry in the array
                // portion.
                Some(index_key + 1)
            } else {
                None
            }
        } else if key.is_nil() {
            Some(0)
        } else {
            None
        };

        // The first live entry at or after `from` in insertion order, skipping the slots left
        // behind by keys that have been removed.
        let from_order = |from: usize| -> NextValue<'gc> {
            for &candidate in &self.order[from.min(self.order.len())..] {
                // A weak key whose object has been collected reads as absent: the entry is skipped
                // here and dropped the next time the map is rebuilt.
                let Some(key_value) = candidate.to_value(mc) else {
                    continue;
                };
                if let Some((_, &(value, _))) = self
                    .map
                    .raw_entry()
                    .from_hash(key_hash(&candidate), |k| k.same(candidate))
                {
                    // A weak slot whose value has been collected reads as absent, so iteration
                    // skips it rather than yielding a hole.
                    let value = value.get(mc);
                    if !value.is_nil() {
                        return NextValue::Found {
                            key: key_value,
                            value,
                        };
                    }
                }
            }
            NextValue::Last
        };

        // If `start_index` is set, then we search the array portion past `start_index` for any
        // non-nil values, otherwise we return the first entry with a non-nil value in the map
        // portion (which always follows the array portion in our iteration order).
        if let Some(start_index) = start_index {
            for i in start_index..self.array.len() {
                if !self.array[i].is_nil() {
                    return NextValue::Found {
                        key: Value::Integer((i + 1).try_into().unwrap()),
                        value: self.array[i],
                    };
                }
            }

            return from_order(0);
        }

        // Otherwise, if we were given a key present in the map portion, we return the key following
        // it in insertion order.
        if let Ok(table_key) = CanonicalKey::new(key) {
            if let Some((_, &(_, order_index))) = self
                .map
                .raw_entry()
                .from_hash(hasher().hash_one(table_key), |k| k.eq(table_key))
            {
                return from_order(order_index + 1);
            }
        }

        NextValue::NotFound
    }

    /// Return the array part of the table as a slice.
    ///
    /// All integer keys that *can* fit into the array part of the table will always be stored in
    /// the array. This means that if you have an integer key `1 <= key <= array.len()`, then you
    /// can look up the value for this key with `array[key - 1]`. Such keys that are not present in
    /// the table will have a value in the array of `Value::Nil`.
    ///
    /// The length of the array part of the table and the length of the table itself are
    /// independent. There may be integral keys *outside* of the array part of the table, even for
    /// sequences.
    ///
    /// If you need to treat the table as purely array-like, and need to ensure that the entire
    /// sequence fits in the array part of the table, you can grow the array part of the table to be
    /// equal to its current length. Remember that non-sequence tables can be pathological, and can
    /// have arbitrarily large borders (numbers returned by `RawTable::length()`).
    pub fn array(&self) -> &[Value<'gc>] {
        &self.array
    }

    /// Return the array part of the table as a mutable slice.
    ///
    /// All values that *can* be stored in the array part of the table will always be stored
    /// there, so this comes with no invariants that you must uphold. Writing a value to `i` in the
    /// table is equivalent to setting the key `i + 1` in the table, and writing `Value::Nil` is
    /// equivalent to removing the key.
    pub fn array_mut(&mut self) -> &mut [Value<'gc>] {
        &mut self.array
    }

    /// Grow the array part of the table to accommodate for at *least* `additional` more elements.
    ///
    /// The array part always has a length equal to its capacity, but may have trailing nil values
    /// at the end.
    ///
    /// The map part of the table is forbidden from having keys that could be in the array part, so
    /// this may move elements from the map part of the table into the array part.
    pub fn grow_array(&mut self, additional: usize) {
        self.array.reserve(additional);
        self.array.resize(self.array.capacity(), Value::Nil);

        // We need to take any newly valid array keys from the map part.
        self.map.retain(|k, v| {
            if v.0.is_definitely_empty() {
                // If our entry is dead, remove it.
                return false;
            }

            // A key with no live form is a weak key holding an object, which is never an array
            // index, so the entry stays where it is.
            let Some(key) = k.live_key() else {
                return true;
            };

            // If our live key is an array index that fits in the array portion, move the entry to
            // the array portion. A weakly held value cannot go: reading one needs a `Mutation` to
            // upgrade with, and the array would hold it strongly anyway.
            if let Some(i) = to_array_index(key.to_value()) {
                if i < self.array.len() {
                    if let Slot::Strong(value) = v.0 {
                        self.array[i] = value;
                        return false;
                    }
                }
            }

            true
        });
        self.compact_order();
    }

    /// Drop the entries a weak table has lost.
    ///
    /// [`RawTable::reserve_map`] cannot do this: judging a weak slot means upgrading it, which
    /// needs a `Mutation`. Without it a weak table's buckets only ever grow, however much of it
    /// has been collected.
    fn purge_collected(&mut self, mc: &Mutation<'gc>) {
        if !self.weak_values && !self.weak_keys {
            return;
        }
        self.map
            .retain(|k, v| k.to_value(mc).is_some() && !v.0.is_empty(mc));
        self.compact_order();
    }

    /// Reserve space in the map part of the table for at least `additional` more elements.
    pub fn reserve_map(&mut self, additional: usize) {
        if additional > self.map.capacity() - self.map.len() {
            // We always filter out all dead keys when growing the map.
            self.map.retain(|_, v| !v.0.is_definitely_empty());
            self.compact_order();

            self.map
                .raw_table_mut()
                .reserve(additional, |(key, _)| key_hash(key));
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Collect)]
#[collect(no_drop)]
enum CanonicalKey<'gc> {
    Boolean(bool),
    Integer(i64),
    Number(u64),
    String(String<'gc>),
    Table(Table<'gc>),
    Closure(Closure<'gc>),
    Callback(Callback<'gc>),
    Thread(Thread<'gc>),
    UserData(UserData<'gc>),
}

impl<'gc> CanonicalKey<'gc> {
    fn new(value: Value<'gc>) -> Result<Self, InvalidTableKey> {
        Ok(match value {
            Value::Nil => {
                return Err(InvalidTableKey::IsNil);
            }
            Value::Number(n) => {
                // NaN keys are disallowed, f64 keys where their closest i64 representation is equal
                // to themselves when cast back to f64 are considered integer keys.
                if n.is_nan() {
                    return Err(InvalidTableKey::IsNaN);
                } else if let Some(i) = f64_to_i64(n) {
                    CanonicalKey::Integer(i)
                } else {
                    CanonicalKey::Number(canonical_float_bytes(n))
                }
            }
            Value::Boolean(b) => CanonicalKey::Boolean(b),
            Value::Integer(i) => CanonicalKey::Integer(i),
            Value::String(s) => CanonicalKey::String(s),
            Value::Table(t) => CanonicalKey::Table(t),
            Value::Function(Function::Closure(c)) => CanonicalKey::Closure(c),
            Value::Function(Function::Callback(c)) => CanonicalKey::Callback(c),
            Value::Thread(t) => CanonicalKey::Thread(t),
            Value::UserData(u) => CanonicalKey::UserData(u),
        })
    }

    fn to_value(self) -> Value<'gc> {
        match self {
            CanonicalKey::Boolean(b) => b.into(),
            CanonicalKey::Integer(i) => i.into(),
            CanonicalKey::Number(n) => f64::from_bits(n).into(),
            CanonicalKey::String(s) => s.into(),
            CanonicalKey::Table(t) => t.into(),
            CanonicalKey::Closure(c) => c.into(),
            CanonicalKey::Callback(c) => c.into(),
            CanonicalKey::Thread(t) => t.into(),
            CanonicalKey::UserData(u) => u.into(),
        }
    }
}

// A table key which may be "live" or "dead".
//
// "Removed" keys in tables do not actually have their entry removed, instead the value is set to
// Nil and the key is set to a dead key if it is a GC object.
//
// This is done to make iteration predictable in the presence of any table mutation that does not
// cause the table to grow.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
enum Key<'gc> {
    Live(CanonicalKey<'gc>),
    /// A key held weakly, for `__mode = "k"`, with the hash it was inserted under.
    ///
    /// The hash is stored because it cannot be recomputed: rehashing on growth needs the hash of
    /// the *original* key, and a weak key may no longer have one to hash.
    Weak(WeakKey<'gc>, #[collect(require_static)] u64),
    /// A removed key, with the hash its entry is stored under.
    Dead(
        #[collect(require_static)] *const (),
        #[collect(require_static)] u64,
    ),
}

impl<'gc> CanonicalKeyRepr<'gc> {
    fn to_canonical(self) -> CanonicalKey<'gc> {
        match self {
            CanonicalKeyRepr::Boolean(b) => CanonicalKey::Boolean(b),
            CanonicalKeyRepr::Integer(i) => CanonicalKey::Integer(i),
            CanonicalKeyRepr::Number(n) => CanonicalKey::Number(n),
            CanonicalKeyRepr::String(s) => CanonicalKey::String(s),
            CanonicalKeyRepr::Callback(c) => CanonicalKey::Callback(c),
        }
    }
}

/// Hold a key weakly. Keys with no separate allocation stay as they are, and so do strings, which
/// Lua counts as values rather than as collectable objects.
fn weaken<'gc>(key: CanonicalKey<'gc>) -> WeakKey<'gc> {
    match key {
        CanonicalKey::Boolean(b) => WeakKey::Immediate(CanonicalKeyRepr::Boolean(b)),
        CanonicalKey::Integer(i) => WeakKey::Immediate(CanonicalKeyRepr::Integer(i)),
        CanonicalKey::Number(n) => WeakKey::Immediate(CanonicalKeyRepr::Number(n)),
        CanonicalKey::Callback(c) => WeakKey::Immediate(CanonicalKeyRepr::Callback(c)),
        CanonicalKey::String(s) => WeakKey::Immediate(CanonicalKeyRepr::String(s)),
        CanonicalKey::Table(t) => WeakKey::Table(Gc::downgrade(t.into_inner())),
        CanonicalKey::Closure(c) => WeakKey::Closure(Gc::downgrade(c.into_inner())),
        CanonicalKey::Thread(t) => WeakKey::Thread(Gc::downgrade(t.into_inner())),
        CanonicalKey::UserData(u) => WeakKey::UserData(Gc::downgrade(u.into_inner())),
    }
}

/// The allocation a key names, when it has one.
fn canonical_ptr<'gc>(key: CanonicalKey<'gc>) -> Option<*const ()> {
    Some(match key {
        CanonicalKey::Boolean(_) | CanonicalKey::Integer(_) | CanonicalKey::Number(_) => {
            return None
        }
        CanonicalKey::String(s) => Gc::as_ptr(s.into_inner()) as *const (),
        CanonicalKey::Table(t) => Gc::as_ptr(t.into_inner()) as *const (),
        CanonicalKey::Closure(c) => Gc::as_ptr(c.into_inner()) as *const (),
        CanonicalKey::Callback(c) => Gc::as_ptr(c.into_inner()) as *const (),
        CanonicalKey::Thread(t) => Gc::as_ptr(t.into_inner()) as *const (),
        CanonicalKey::UserData(u) => Gc::as_ptr(u.into_inner()) as *const (),
    })
}

impl<'gc> Key<'gc> {
    /// The allocation this key names, however it is held.
    fn as_ptr(self) -> Option<*const ()> {
        match self {
            Key::Live(k) => canonical_ptr(k),
            Key::Weak(w, _) => w.as_ptr(),
            Key::Dead(p, _) => Some(p),
        }
    }

    /// Whether two stored keys name the same thing, without upgrading either.
    fn same(self, other: Key<'gc>) -> bool {
        match (self.as_ptr(), other.as_ptr()) {
            (Some(a), Some(b)) => a == b,
            (None, None) => match (self.live_key(), other.live_key()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
            _ => false,
        }
    }

    /// The key as a value, or `None` once the object it named has been collected.
    fn to_value(self, mc: &Mutation<'gc>) -> Option<Value<'gc>> {
        match self {
            Key::Live(k) => Some(k.to_value()),
            Key::Weak(w, _) => w.to_value(mc),
            Key::Dead(..) => None,
        }
    }

    /// The key this entry keeps once its value is removed, if it has to change.
    ///
    /// A key with no separate allocation is kept as it is, so that re-adding it finds this same
    /// entry. Strings are among them: they compare by content, and a tombstone compares by
    /// address, which would make an equal string a second entry for the same Lua key.
    fn kill(self, hash: u64) -> Option<Key<'gc>> {
        let dead = |ptr| Some(Key::Dead(ptr, hash));
        match self {
            Key::Weak(w, _) => w.as_ptr().map(|p| Key::Dead(p, hash)),
            Key::Dead(..) => Some(self),
            Key::Live(v) => match v {
                CanonicalKey::Boolean(_)
                | CanonicalKey::Integer(_)
                | CanonicalKey::Number(_)
                | CanonicalKey::String(_) => None,
                CanonicalKey::Table(t) => dead(Gc::as_ptr(t.into_inner()) as *const ()),
                CanonicalKey::Closure(c) => dead(Gc::as_ptr(c.into_inner()) as *const ()),
                CanonicalKey::Callback(c) => dead(Gc::as_ptr(c.into_inner()) as *const ()),
                CanonicalKey::Thread(t) => dead(Gc::as_ptr(t.into_inner()) as *const ()),
                CanonicalKey::UserData(u) => dead(Gc::as_ptr(u.into_inner()) as *const ()),
            },
        }
    }

    /// Whether this stored key must be replaced by the one being assigned.
    ///
    /// True for a tombstone, and for a weak key that has lost its object — the collector can hand
    /// the freed address straight to the new key, and the entry would otherwise keep a key that
    /// reads as collected while holding a live value.
    ///
    /// Asked of the *variant*, never of [`Key::live_key`]: a weak key holding a live object has no
    /// live form either, and answering from that would turn every re-assignment into a
    /// resurrection, which is how a weak key used to be silently promoted to a strong one.
    fn needs_revival(&self, mc: &Mutation<'gc>) -> bool {
        match self {
            Key::Live(_) => false,
            Key::Weak(w, _) => w.to_value(mc).is_none(),
            Key::Dead(..) => true,
        }
    }

    /// The key as a value, when that is knowable without upgrading.
    ///
    /// A weak key holding an object answers `None`: reading it needs a `Mutation`, and every
    /// caller here only wants to know whether the key is an array index, which an object never is.
    fn live_key(self) -> Option<CanonicalKey<'gc>> {
        match self {
            Key::Live(v) => Some(v),
            Key::Weak(WeakKey::Immediate(repr), _) => Some(repr.to_canonical()),
            Key::Weak(..) | Key::Dead(..) => None,
        }
    }

    fn eq(self, key: CanonicalKey<'gc>) -> bool {
        match (self, key) {
            (Key::Live(a), b) => a == b,
            // By address, exactly as a dead key is: a weak key must answer this without upgrading,
            // because it may be looked up after the object it named has been collected.
            (Key::Weak(w, _), b) => match (w, b) {
                (WeakKey::Immediate(a), b) => a.to_canonical() == b,
                (w, b) => match (w.as_ptr(), canonical_ptr(b)) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                },
            },
            (Key::Dead(a, _), b) => match b {
                CanonicalKey::Table(t) => a == Gc::as_ptr(t.into_inner()) as *const (),
                CanonicalKey::Closure(c) => a == Gc::as_ptr(c.into_inner()) as *const (),
                CanonicalKey::Callback(c) => a == Gc::as_ptr(c.into_inner()) as *const (),
                CanonicalKey::Thread(t) => a == Gc::as_ptr(t.into_inner()) as *const (),
                CanonicalKey::UserData(u) => a == Gc::as_ptr(u.into_inner()) as *const (),
                _ => false,
            },
        }
    }
}

// Returns the closest i64 to a given f64 such that casting the i64 back to an f64 results in an
// equal value, if such an integer exists.
fn f64_to_i64(n: f64) -> Option<i64> {
    let i = n as i64;
    if i as f64 == n {
        Some(i)
    } else {
        None
    }
}

// Parameter must not be NaN, should return a bit-pattern which is always equal when the
// corresponding f64s are equal (-0.0 and 0.0 return the same bit pattern).
fn canonical_float_bytes(f: f64) -> u64 {
    assert!(!f.is_nan());
    if f == 0.0 {
        0.0f64.to_bits()
    } else {
        f.to_bits()
    }
}

// If the given key can live in the array part of the table (integral value between 1 and
// usize::MAX), returns the associated array index.
fn to_array_index<'gc>(key: Value<'gc>) -> Option<usize> {
    let i = match key {
        Value::Integer(i) => i,
        Value::Number(f) => f64_to_i64(f)?,
        _ => return None,
    };

    if i > 0 {
        Some(usize::try_from(i).ok()? - 1)
    } else {
        None
    }
}

// Returns the place of the highest set bit in the given i, i = 0 returns 0, i = 1 returns 1, i = 2
// returns 2, i = 3 returns 2, and so on.
fn highest_bit(i: usize) -> usize {
    i.checked_ilog2().map(|i| i + 1).unwrap_or(0) as usize
}

impl<'gc> RawTable<'gc> {
    /// Switch this table to holding its values weakly.
    ///
    /// Existing entries in the array part move into the map, because weakness is a property of a
    /// map slot and keeping two representations in step would be a second thing to get wrong.
    pub fn make_values_weak(&mut self, mc: &Mutation<'gc>) {
        if self.weak_values {
            return;
        }
        self.weak_values = true;

        let array: Vec<Value<'gc>> = self.array.drain(..).collect();
        for (i, value) in array.into_iter().enumerate() {
            if !value.is_nil() {
                let key = Value::Integer(i as i64 + 1);
                // Cannot fail: an integer is always a valid key.
                let _ = self.set(mc, key, value);
            }
        }

        // Entries already in the map were stored strongly; weaken them in place.
        let keys: Vec<Key<'gc>> = self.order.iter().copied().collect();
        for key in keys {
            let hash = key_hash(&key);
            if let Some(bucket) = self.map.raw_table_mut().find(hash, |(k, _)| k.same(key)) {
                let (_, v) = unsafe { bucket.as_mut() };
                if let Slot::Strong(value) = v.0 {
                    v.0 = Slot::Weak(WeakValue::new(value));
                }
            }
        }
    }

    /// Whether this table holds its values weakly.
    pub fn has_weak_values(&self) -> bool {
        self.weak_values
    }

    /// Revive the value of every entry whose key is still alive, reporting whether anything moved.
    pub(crate) fn revive_live_entries(
        &self,
        fc: &ottavino_gc_arena::Finalization<'gc>,
        roots: &mut Vec<Value<'gc>>,
    ) -> usize {
        if !self.weak_keys {
            return 0;
        }
        let mut live_count = 0;
        for (key, (slot, _)) in &self.map {
            let alive = match key {
                Key::Weak(w, _) => !w.is_dead(fc),
                // A strong key in a weak-key table is one with nothing to collect, so it is
                // always alive.
                Key::Live(_) => true,
                Key::Dead(..) => false,
            };
            if alive {
                live_count += 1;
                if let Slot::Weak(value) = slot {
                    value.resurrect_if_dead(fc);
                    if let Some(value) = value.get(fc) {
                        roots.push(value);
                    }
                }
            }
        }
        live_count
    }

    /// Whether this table holds its keys weakly.
    pub fn has_weak_keys(&self) -> bool {
        self.weak_keys
    }

    /// Hold this table's keys weakly, for `__mode = "k"`.
    ///
    /// Values are weakened too, which is not an implementation shortcut but the ephemeron rule: if
    /// values were traced strongly, a value referring back to its own key would keep that key
    /// alive and the entry could never be collected — the exact leak weak keys exist to avoid.
    /// The values are put back by the finalizer pass, for entries whose key survived on its own.
    pub fn make_keys_weak(&mut self, mc: &Mutation<'gc>) {
        if self.weak_keys {
            return;
        }
        self.weak_keys = true;
        self.make_values_weak(mc);

        // Existing entries were stored strongly; weaken them in place, keeping the hash they were
        // inserted under so lookups and growth still find them.
        let keys: Vec<Key<'gc>> = self.order.iter().copied().collect();
        for (position, key) in keys.into_iter().enumerate() {
            let Some(canonical) = key.live_key() else {
                continue;
            };
            let hash = key_hash(&key);
            let weak = Key::Weak(weaken(canonical), hash);
            if let Some(bucket) = self.map.raw_table_mut().find(hash, |(k, _)| k.same(key)) {
                unsafe { bucket.as_mut() }.0 = weak;
            }
            self.order[position] = weak;
        }
    }
    /// Drop every entry and release the buckets, unlike setting each key to nil.
    pub fn clear(&mut self) {
        self.array.clear();
        self.map.clear();
        self.order.clear();
    }
}

/// What a table stores in one entry.
///
/// A table declared `__mode = "v"` holds its values weakly; every other table holds them strongly.
/// Making that a property of the *slot* rather than of the tracing means the collector needs no
/// special case and no invariant to uphold: a weak slot is a `GcWeak`, so the only way to read it
/// is to upgrade, and a collected value simply answers `None`.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub(super) enum Slot<'gc> {
    Strong(Value<'gc>),
    Weak(WeakValue<'gc>),
}

impl<'gc> Slot<'gc> {
    fn get(self, mc: &Mutation<'gc>) -> Value<'gc> {
        match self {
            Slot::Strong(v) => v,
            // A collected value reads as absent, which is how Lua spells a missing entry anyway.
            Slot::Weak(w) => w.get(mc).unwrap_or(Value::Nil),
        }
    }

    /// Whether this entry is absent — either set to nil, or collected.
    fn is_empty(self, mc: &Mutation<'gc>) -> bool {
        self.get(mc).is_nil()
    }

    /// The value for display, without a `Mutation` to upgrade with. A weak slot shows as nil.
    fn shown(self) -> Value<'gc> {
        match self {
            Slot::Strong(v) => v,
            Slot::Weak(_) => Value::Nil,
        }
    }

    /// Whether this entry is absent, without a `Mutation` to hand.
    ///
    /// A weak slot cannot be judged without upgrading, so it is treated as present; callers that
    /// can be exact use [`Slot::is_empty`].
    fn is_definitely_empty(self) -> bool {
        match self {
            Slot::Strong(v) => v.is_nil(),
            Slot::Weak(_) => false,
        }
    }
}
