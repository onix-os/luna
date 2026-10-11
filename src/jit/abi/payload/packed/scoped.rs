use std::mem::MaybeUninit;

use super::*;
use crate::jit::{abi, helpers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Bounds,
    Alignment,
    Stopped,
    Projection,
}

struct Scope<'s, 'gc, const CAPACITY: usize> {
    values: NonNull<Value<'gc>>,
    descriptors: NonNull<Packed>,
    count: usize,
    len: usize,
    stopped: bool,
    borrow: PhantomData<(
        &'s mut [Value<'gc>],
        &'s mut [MaybeUninit<Packed>; CAPACITY],
    )>,
}

fn with_scope<'gc, const CAPACITY: usize, R>(
    values: &mut [Value<'gc>],
    count: usize,
    body: impl for<'s> FnOnce(&mut Scope<'s, 'gc, CAPACITY>) -> R,
) -> Result<R, Refusal> {
    let mut descriptors = [MaybeUninit::uninit(); CAPACITY];
    let mut scope = Scope::<CAPACITY>::new(values, count, &mut descriptors)?;
    Ok(body(&mut scope))
}

impl<'s, 'gc, const CAPACITY: usize> Scope<'s, 'gc, CAPACITY> {
    fn new(
        values: &'s mut [Value<'gc>],
        count: usize,
        descriptors: &'s mut [MaybeUninit<Packed>; CAPACITY],
    ) -> Result<Self, Refusal> {
        if CAPACITY > 256 || count > CAPACITY || count > values.len() {
            return Err(Refusal::Bounds);
        }
        let mut scope = Self {
            values: NonNull::new(values.as_mut_ptr()).unwrap(),
            descriptors: NonNull::new(descriptors.as_mut_ptr().cast()).unwrap(),
            count,
            len: values.len(),
            stopped: true,
            borrow: PhantomData,
        };
        scope.bind_all()?;
        scope.stopped = false;
        Ok(scope)
    }

    fn bind_one(&mut self, index: usize) -> Result<(), Refusal> {
        let value = unsafe { &mut *self.values.as_ptr().add(index) };
        let descriptor = Packed::bind(value).ok_or(Refusal::Alignment)?;
        unsafe { self.descriptors.as_ptr().add(index).write(descriptor) };
        Ok(())
    }

    fn bind_all(&mut self) -> Result<(), Refusal> {
        for index in 0..self.count {
            self.bind_one(index)?;
        }
        Ok(())
    }

    fn check(&self, index: usize) -> Result<(), Refusal> {
        if self.stopped {
            return Err(Refusal::Stopped);
        }
        if index >= self.count {
            return Err(Refusal::Bounds);
        }
        Ok(())
    }

    fn retire(&mut self, index: usize) {
        unsafe {
            self.descriptors
                .as_ptr()
                .add(index)
                .write(Packed(std::ptr::without_provenance_mut(EMPTY)));
        }
    }

    fn read(&self, index: usize) -> Result<Value<'gc>, Refusal> {
        self.check(index)?;
        let descriptor = unsafe { self.descriptors.as_ptr().add(index).read() };
        if matches!(descriptor.0.addr(), EMPTY | OPAQUE) {
            return Ok(unsafe { self.values.as_ptr().add(index).read() });
        }
        let slot = unsafe { descriptor.load() }.unwrap();
        Ok(slot.value(Value::Nil))
    }

    fn store_numeric(&mut self, index: usize, value: Slot) -> Result<bool, Refusal> {
        self.check(index)?;
        let descriptor = unsafe { self.descriptors.as_ptr().add(index).read() };
        Ok(unsafe { descriptor.store_numeric(value) })
    }

    fn replace(&mut self, index: usize, value: Value<'gc>, terminal: bool) -> Result<(), Refusal> {
        self.check(index)?;
        self.stopped = true;
        self.retire(index);
        unsafe { self.values.as_ptr().add(index).write(value) };
        if !terminal {
            self.bind_one(index)?;
        }
        self.stopped = false;
        Ok(())
    }

    fn with_destination<R>(
        &mut self,
        index: usize,
        body: impl FnOnce(&mut Value<'gc>) -> R,
    ) -> Result<R, Refusal> {
        self.check(index)?;
        self.stopped = true;
        self.retire(index);
        let result = body(unsafe { &mut *self.values.as_ptr().add(index) });
        self.bind_one(index)?;
        self.stopped = false;
        Ok(result)
    }

    fn with_canonical<R>(
        &mut self,
        body: impl FnOnce(&mut [Value<'gc>]) -> R,
    ) -> Result<R, Refusal> {
        if self.stopped {
            return Err(Refusal::Stopped);
        }
        self.stopped = true;
        for index in 0..self.count {
            self.retire(index);
        }
        let result =
            body(unsafe { std::slice::from_raw_parts_mut(self.values.as_ptr(), self.len) });
        self.bind_all()?;
        self.stopped = false;
        Ok(result)
    }
}

struct FrameScope<'s, 'gc, 'a, 'b, 'p, const CAPACITY: usize> {
    scope: Scope<'s, 'gc, CAPACITY>,
    frame: NonNull<helpers::Frame<'gc, 'a, 'b, 'p>>,
    refusal: Option<Refusal>,
    materializations: u64,
}

fn with_frame_scope<'gc, const CAPACITY: usize, R>(
    frame: &mut helpers::Frame<'gc, '_, '_, '_>,
    body: impl for<'s> FnOnce(&mut FrameScope<'s, 'gc, '_, '_, '_, CAPACITY>) -> R,
) -> Result<R, Refusal> {
    if frame.projection.is_some() {
        return Err(Refusal::Projection);
    }
    let pointer = NonNull::from(&mut *frame);
    let stopped = frame.panic.is_some();
    let mut descriptors = [MaybeUninit::uninit(); CAPACITY];
    let mut scope = Scope::new(
        frame.registers.stack_frame,
        frame.slot_count,
        &mut descriptors,
    )?;
    scope.stopped = stopped;
    let mut bound = FrameScope {
        scope,
        frame: pointer,
        refusal: None,
        materializations: 0,
    };
    Ok(body(&mut bound))
}

impl<const CAPACITY: usize> FrameScope<'_, '_, '_, '_, '_, CAPACITY> {
    fn helper<const KIND: u32>(&mut self, a: u32, b: u32, c: u32, pc: u32) -> Result<u32, Refusal> {
        if self.scope.stopped {
            return if unsafe { self.frame.as_ref() }.panic.is_some() {
                Ok(abi::HELPER_PANICKED)
            } else {
                Err(Refusal::Stopped)
            };
        }
        self.scope.stopped = true;
        for index in 0..self.scope.count {
            self.scope.retire(index);
        }
        let frame = unsafe { self.frame.as_mut() };
        let result = helpers::call_direct::<KIND>(frame, a, b, c, pc);
        if result != abi::HELPER_PANICKED {
            self.scope.len = frame.registers.stack_frame.len();
            self.scope.values = NonNull::new(frame.registers.stack_frame.as_mut_ptr()).unwrap();
            if self.scope.count > self.scope.len {
                return Err(Refusal::Bounds);
            }
            self.scope.bind_all()?;
            self.scope.stopped = false;
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "scoped_tests.rs"]
mod tests;

#[path = "bridge.rs"]
pub(crate) mod bridge;
