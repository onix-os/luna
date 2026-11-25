use std::mem::MaybeUninit;

use super::{Slot, NUMBER};
use crate::Value;

pub(in crate::jit) struct Inputs<'a> {
    slots: &'a mut [Slot],
    tags: Option<u64>,
}

impl<'a> Inputs<'a> {
    #[inline(always)]
    pub(in crate::jit) fn marshal<const CLASSIFY: bool>(
        storage: &'a mut [MaybeUninit<Slot>],
        values: &[Value<'_>],
    ) -> Self {
        let storage = &mut storage[..values.len()];
        let mut tags = 0;
        for (dest, value) in storage.iter_mut().zip(values.iter().copied()) {
            let slot = Slot::from_value(value);
            if CLASSIFY {
                tags |= slot.tag;
            }
            dest.write(slot);
        }
        let slots = unsafe {
            std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<Slot>(), storage.len())
        };
        Self {
            slots,
            tags: CLASSIFY.then_some(tags),
        }
    }

    #[inline(always)]
    pub(in crate::jit) fn slots(&self) -> &[Slot] {
        self.slots
    }

    #[inline(always)]
    pub(in crate::jit) fn slots_mut(&mut self) -> &mut [Slot] {
        self.tags = None;
        self.slots
    }

    #[inline(always)]
    pub(in crate::jit) fn take_scalar(&mut self) -> Option<&mut [Slot]> {
        (self.tags.take()? <= NUMBER).then_some(&mut *self.slots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Callback, CallbackReturn, Closure, Lua, Table, Thread, UserData};

    #[test]
    fn marshal_matches_each_value_kind_and_scalar_lane_without_touching_suffix() {
        Lua::empty().enter(|ctx| {
            let values = [
                Value::Nil,
                Value::Boolean(false),
                Value::Boolean(true),
                Value::Integer(i64::MIN),
                Value::Integer(i64::MAX),
                Value::Number(-0.0),
                Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
                Value::String(ctx.intern(b"reference")),
                Value::Table(Table::new(&ctx)),
                Closure::load(ctx, None, b"return 1").unwrap().into(),
                Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)).into(),
                Value::Thread(Thread::new(ctx)),
                Value::UserData(UserData::new_static(&ctx, 9u32)),
            ];
            let widths: &[usize] = if cfg!(miri) {
                &[0, 1, 7, 17]
            } else {
                &[0, 1, 7, 17, 64, 255, 256]
            };
            for &width in widths {
                for value in values {
                    for lane in 0..width.max(1) {
                        let mut initial = vec![Value::Integer(19); width];
                        if width != 0 {
                            initial[lane] = value;
                        }
                        let sentinel = Slot {
                            tag: u64::MAX,
                            bits: 73,
                        };
                        let mut storage = vec![MaybeUninit::new(sentinel); width + 2];
                        let mut inputs = Inputs::marshal::<true>(&mut storage, &initial);
                        let expected: Vec<_> =
                            initial.iter().copied().map(Slot::from_value).collect();
                        assert_eq!(inputs.slots().len(), width);
                        for (a, b) in inputs.slots().iter().zip(&expected) {
                            assert_eq!((a.tag, a.bits), (b.tag, b.bits));
                        }
                        assert_eq!(
                            inputs.take_scalar().is_some(),
                            expected.iter().all(|s| s.tag <= NUMBER)
                        );
                        assert!(inputs.take_scalar().is_none());
                        drop(inputs);
                        for slot in &storage[width..] {
                            let slot = unsafe { slot.assume_init() };
                            assert_eq!((slot.tag, slot.bits), (sentinel.tag, sentinel.bits));
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn mutable_access_consumes_the_certificate_even_when_tags_stay_scalar() {
        let mut storage = [MaybeUninit::uninit(); 2];
        let mut inputs = Inputs::marshal::<true>(&mut storage, &[Value::Nil, Value::Integer(3)]);
        assert_eq!(inputs.slots()[1].bits, 3);
        inputs.slots_mut()[1].bits = 7;
        assert!(inputs.take_scalar().is_none());
        let mut plain = Inputs::marshal::<false>(&mut storage, &[Value::Boolean(true)]);
        assert!(plain.take_scalar().is_none());
        assert_eq!(
            (plain.slots()[0].tag, plain.slots()[0].bits),
            (super::super::BOOLEAN, 1)
        );
    }

    #[test]
    fn insufficient_storage_panics_before_initializing_any_element() {
        let sentinel = Slot {
            tag: u64::MAX,
            bits: 17,
        };
        let mut storage = [MaybeUninit::new(sentinel)];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Inputs::marshal::<true>(&mut storage, &[Value::Nil, Value::Nil]);
        }));
        assert!(result.is_err());
        let actual = unsafe { storage[0].assume_init() };
        assert_eq!((actual.tag, actual.bits), (sentinel.tag, sentinel.bits));
    }
}
