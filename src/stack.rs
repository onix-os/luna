use std::{
    cell::{Ref, RefMut},
    marker::PhantomData,
    ops::{Bound, RangeBounds},
};

use allocator_api2::vec;
use ottavino_gc_arena::{allocator_api::MetricsAlloc, lock::RefLock, Gc};

use crate::{Context, FromMultiValue, FromValue, IntoMultiValue, IntoValue, TypeError, Value};

/// A thread's value stack.
pub(crate) type StackVec<'gc> = vec::Vec<Value<'gc>, MetricsAlloc<'gc>>;

/// The mechanism through which all callbacks receive parameters and return values.
///
/// Each [`Thread`](crate::Thread) has its own internal stack of [`Value`]s, and this stack is
/// shared for all running Lua functions *and* callbacks.
///
/// A `Stack` is a *view* of the top of that stack, starting at some bottom index. Values below the
/// bottom belong to the frames underneath and cannot be reached through it.
///
/// # Why this holds a lock rather than a reference
///
/// The obvious implementation is `&'a mut Vec<Value>`, and that is what this was. It cannot be,
/// because an *open upvalue* aliases a stack slot: Lua driven re-entrantly from inside a callback
/// may read or write an upvalue pointing into the very stack that callback is using. A borrow held
/// across the call would make that a panic — which is why callbacks used to run on a detached copy
/// of their arguments instead.
///
/// So each operation takes the lock and releases it again, and nothing is held between calls. The
/// callback shares the real stack, no copy in or out, and a re-entrant upvalue access simply takes
/// the lock in between. The cost is a borrow flag check per operation, which is noise next to the
/// dispatch around it.
///
/// The exception is [`Stack::drain`], whose iterator has to hold the lock while it lives. Consume
/// it before running Lua again, which is what every use in the tree does anyway.
pub struct Stack<'gc, 'a> {
    ctx: Context<'gc>,
    values: Gc<'gc, RefLock<StackVec<'gc>>>,
    bottom: usize,
    // Preserves the invariance and borrow behaviour the `&'a mut Vec` field used to give this type.
    _marker: PhantomData<&'a mut ()>,
}

impl<'gc, 'a> Stack<'gc, 'a> {
    pub(crate) fn new(
        ctx: Context<'gc>,
        values: Gc<'gc, RefLock<StackVec<'gc>>>,
        bottom: usize,
    ) -> Self {
        assert!(values.borrow().len() >= bottom);
        Self {
            ctx,
            values,
            bottom,
            _marker: PhantomData,
        }
    }

    fn read(&self) -> Ref<'_, StackVec<'gc>> {
        self.values.borrow()
    }

    fn write(&self) -> RefMut<'_, StackVec<'gc>> {
        self.values.borrow_mut(&self.ctx)
    }

    pub fn reborrow(&mut self) -> Stack<'gc, '_> {
        Stack {
            ctx: self.ctx,
            values: self.values,
            bottom: self.bottom,
            _marker: PhantomData,
        }
    }

    pub fn sub_stack(&mut self, bottom: usize) -> Stack<'gc, '_> {
        Stack {
            ctx: self.ctx,
            values: self.values,
            bottom: self.bottom + bottom,
            _marker: PhantomData,
        }
    }

    pub fn get(&self, i: usize) -> Value<'gc> {
        self.read()
            .get(self.bottom + i)
            .copied()
            .unwrap_or_default()
    }

    /// Overwrite the value at `i`.
    ///
    /// Replaces the `IndexMut` impl this type used to have, which cannot be written over a lock.
    ///
    /// # Panics
    ///
    /// If `i` is out of bounds, as indexing did.
    pub fn set(&mut self, i: usize, value: Value<'gc>) {
        let bottom = self.bottom;
        self.write()[bottom + i] = value;
    }

    /// Copy this stack's values out into a `Vec`.
    ///
    /// Replaces slicing (`&stack[..]`) for the cases that need to hand a `&[Value]` to something
    /// else while the stack stays reachable.
    pub fn to_vec(&self) -> std::vec::Vec<Value<'gc>> {
        self.read()[self.bottom..].to_vec()
    }

    /// Reverse this stack's values in place.
    pub fn reverse(&mut self) {
        let bottom = self.bottom;
        self.write()[bottom..].reverse();
    }

    pub fn push_back(&mut self, value: Value<'gc>) {
        self.write().push(value);
    }

    pub fn push_front(&mut self, value: Value<'gc>) {
        let bottom = self.bottom;
        self.write().insert(bottom, value);
    }

    pub fn pop_back(&mut self) -> Option<Value<'gc>> {
        let mut values = self.write();
        if values.len() > self.bottom {
            Some(values.pop().unwrap())
        } else {
            None
        }
    }

    pub fn pop_front(&mut self) -> Option<Value<'gc>> {
        let bottom = self.bottom;
        let mut values = self.write();
        if values.len() > bottom {
            Some(values.remove(bottom))
        } else {
            None
        }
    }

    pub fn remove(&mut self, i: usize) -> Option<Value<'gc>> {
        let index = self.bottom + i;
        let mut values = self.write();
        if index < values.len() {
            Some(values.remove(index))
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.read().len() - self.bottom
    }

    pub fn is_empty(&self) -> bool {
        self.read().len() == self.bottom
    }

    pub fn clear(&mut self) {
        let bottom = self.bottom;
        self.write().truncate(bottom);
    }

    pub fn resize(&mut self, size: usize) {
        let len = self.bottom + size;
        self.write().resize(len, Value::Nil);
    }

    pub fn reserve(&mut self, additional: usize) {
        self.write().reserve(additional);
    }

    pub fn capacity(&self) -> usize {
        self.read().capacity() - self.bottom
    }

    /// Remove a range of values, yielding them.
    ///
    /// The returned iterator holds the stack lock for as long as it lives, so it must not outlive a
    /// re-entrant call into Lua. Every use in the tree consumes it immediately.
    pub fn drain<R: RangeBounds<usize>>(&mut self, range: R) -> Drain<'gc, '_> {
        let start = match range.start_bound().cloned() {
            Bound::Included(r) => self.bottom + r,
            Bound::Excluded(r) => self.bottom + r + 1,
            Bound::Unbounded => self.bottom,
        };
        let values = self.write();
        let end = match range.end_bound().cloned() {
            Bound::Included(r) => self.bottom + r + 1,
            Bound::Excluded(r) => self.bottom + r,
            Bound::Unbounded => values.len(),
        };
        assert!(
            start <= end && end <= values.len(),
            "drain range out of bounds"
        );
        Drain {
            values,
            start,
            next: start,
            end,
        }
    }

    pub fn into_back(&mut self, ctx: Context<'gc>, v: impl IntoMultiValue<'gc>) {
        // The lock is held across the conversions, exactly as the `&mut Vec` field used to be.
        // `IntoValue` does not run Lua, so there is nothing here that can re-enter.
        let mut values = self.write();
        for v in v.into_multi_value(ctx) {
            values.push(v.into_value(ctx));
        }
    }

    pub fn into_front(&mut self, ctx: Context<'gc>, v: impl IntoMultiValue<'gc>) {
        let bottom = self.bottom;
        let mut values = self.write();
        let mut count = 0;
        for v in v.into_multi_value(ctx) {
            count += 1;
            values.push(v.into_value(ctx));
        }
        values[bottom..].rotate_right(count);
    }

    pub fn from_back<V: FromValue<'gc>>(&mut self, ctx: Context<'gc>) -> Result<V, TypeError> {
        V::from_value(ctx, self.pop_back().unwrap_or_default())
    }

    pub fn from_front<V: FromValue<'gc>>(&mut self, ctx: Context<'gc>) -> Result<V, TypeError> {
        V::from_value(ctx, self.pop_front().unwrap_or_default())
    }

    pub fn replace(&mut self, ctx: Context<'gc>, v: impl IntoMultiValue<'gc>) {
        self.clear();
        self.into_back(ctx, v);
    }

    pub fn consume<V: FromMultiValue<'gc>>(&mut self, ctx: Context<'gc>) -> Result<V, TypeError> {
        V::from_multi_value(ctx, self.drain(..))
    }
}

/// The iterator returned by [`Stack::drain`]. Holds the stack lock while it lives.
pub struct Drain<'gc, 'a> {
    values: RefMut<'a, StackVec<'gc>>,
    start: usize,
    next: usize,
    end: usize,
}

impl<'gc, 'a> Iterator for Drain<'gc, 'a> {
    type Item = Value<'gc>;

    fn next(&mut self) -> Option<Value<'gc>> {
        if self.next < self.end {
            let value = self.values[self.next];
            self.next += 1;
            Some(value)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.end - self.next;
        (remaining, Some(remaining))
    }
}

impl<'gc, 'a> ExactSizeIterator for Drain<'gc, 'a> {}

impl<'gc, 'a> DoubleEndedIterator for Drain<'gc, 'a> {
    fn next_back(&mut self) -> Option<Value<'gc>> {
        if self.next < self.end {
            self.end -= 1;
            Some(self.values[self.end])
        } else {
            None
        }
    }
}

impl<'gc, 'a> Drop for Drain<'gc, 'a> {
    fn drop(&mut self) {
        // The whole range goes, iterated or not, matching `Vec::drain`.
        self.values.drain(self.start..self.end.max(self.next));
    }
}

/// Iterating a `Stack` by reference. Holds the lock, like [`Drain`].
pub struct Iter<'gc, 'a> {
    values: Ref<'a, StackVec<'gc>>,
    next: usize,
}

impl<'gc, 'a> Iterator for Iter<'gc, 'a> {
    type Item = Value<'gc>;

    fn next(&mut self) -> Option<Value<'gc>> {
        let value = self.values.get(self.next).copied();
        if value.is_some() {
            self.next += 1;
        }
        value
    }
}

impl<'gc: 'b, 'a, 'b> IntoIterator for &'b Stack<'gc, 'a> {
    type Item = Value<'gc>;
    type IntoIter = Iter<'gc, 'b>;

    fn into_iter(self) -> Self::IntoIter {
        Iter {
            values: self.read(),
            next: self.bottom,
        }
    }
}

impl<'gc, 'a> Extend<Value<'gc>> for Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = Value<'gc>>>(&mut self, iter: T) {
        self.write().extend(iter);
    }
}

impl<'gc, 'a, 'b> Extend<Value<'gc>> for &'b mut Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = Value<'gc>>>(&mut self, iter: T) {
        self.write().extend(iter);
    }
}

impl<'gc, 'a> Extend<&'a Value<'gc>> for Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = &'a Value<'gc>>>(&mut self, iter: T) {
        self.write().extend(iter.into_iter().copied());
    }
}

impl<'gc: 'b, 'a, 'b, 'c> Extend<&'b Value<'gc>> for &'c mut Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = &'b Value<'gc>>>(&mut self, iter: T) {
        self.write().extend(iter.into_iter().copied());
    }
}
