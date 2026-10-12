use super::{Slot, BOOLEAN, INTEGER, NIL, NUMBER, REFERENCE};
use crate::Value;

#[repr(C)]
pub(crate) struct Scratch<const N: usize> {
    bits: [u64; N],
    tags: [u8; N],
}

mod probe;

impl<const N: usize> Scratch<N> {
    pub(crate) fn capture(values: &[Value<'_>; N]) -> Self {
        let mut scratch = Self {
            bits: [0; N],
            tags: [0; N],
        };
        for ((bits, tag), value) in scratch
            .bits
            .iter_mut()
            .zip(scratch.tags.iter_mut())
            .zip(values.iter().copied())
        {
            let slot = Slot::from_value(value);
            *bits = slot.bits;
            *tag = slot.tag as u8;
        }
        scratch
    }

    pub(crate) fn slot(&self, index: usize) -> Slot {
        Slot {
            tag: u64::from(self.tags[index]),
            bits: self.bits[index],
        }
    }

    pub(crate) fn set(&mut self, index: usize, value: Slot) -> bool {
        if index >= N || !matches!(value.tag, NIL | BOOLEAN | INTEGER | NUMBER | REFERENCE) {
            return false;
        }
        self.bits[index] = value.bits;
        self.tags[index] = value.tag as u8;
        true
    }

    #[inline(always)]
    pub(crate) fn restore(&self, values: &mut [Value<'_>; N]) {
        for ((bits, tag), value) in self
            .bits
            .iter()
            .zip(self.tags.iter())
            .zip(values.iter_mut())
        {
            Slot {
                bits: *bits,
                tag: u64::from(*tag),
            }
            .write_back(value);
        }
    }

    pub(crate) fn planes(&mut self) -> (&mut [u64; N], &mut [u8; N]) {
        (&mut self.bits, &mut self.tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Callback, CallbackReturn, Closure, Lua, String, Table, Thread, UserData};

    fn shape<const N: usize>() {
        let aligned = (9 * N).div_ceil(8) * 8;
        assert_eq!(std::mem::size_of::<Scratch<N>>(), aligned);
        assert_eq!(std::mem::align_of::<Scratch<N>>(), 8);
        assert_eq!(std::mem::offset_of!(Scratch<N>, bits), 0);
        assert_eq!(std::mem::offset_of!(Scratch<N>, tags), 8 * N);
        let mut scratch = Scratch::capture(&[Value::Nil; N]);
        let (bits, tags) = scratch.planes();
        assert_eq!(bits.as_ptr() as usize % 8, 0);
        assert_eq!(tags.as_ptr() as usize - bits.as_ptr() as usize, 8 * N);
        for index in 0..N {
            bits[index] = index as u64;
            tags[index] = INTEGER as u8;
        }
        let mut values = [Value::Nil; N];
        scratch.restore(&mut values);
        for (index, value) in values.into_iter().enumerate() {
            assert!(matches!(value, Value::Integer(actual) if actual == index as i64));
        }
    }

    #[test]
    fn planes_have_bounded_storage_and_aligned_payloads() {
        shape::<0>();
        shape::<1>();
        shape::<2>();
        shape::<3>();
        shape::<4>();
        shape::<5>();
        shape::<6>();
        shape::<7>();
        shape::<8>();
        shape::<16>();
        shape::<32>();
        shape::<64>();
        shape::<128>();
        shape::<256>();
        assert_eq!(std::mem::size_of::<Scratch<7>>(), 64);
        assert_eq!(std::mem::size_of::<[Slot; 7]>(), 112);
    }

    #[test]
    fn scalar_bits_roundtrip_without_tag_or_nan_collisions() {
        let patterns = [
            0,
            1,
            u64::MAX,
            1 << 63,
            0x7ff0_0000_0000_0000,
            0x7ff8_0000_0000_0001,
        ];
        let mut random = 0xd1b5_4a32_d192_ed03u64;
        for bits in patterns.into_iter().chain((0..1024).map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random
        })) {
            let original = [
                Value::Integer(bits as i64),
                Value::Number(f64::from_bits(bits)),
            ];
            let scratch = Scratch::capture(&original);
            let mut restored = [Value::Nil; 2];
            scratch.restore(&mut restored);
            for index in 0..2 {
                let expected = Slot::from_value(original[index]);
                let actual = Slot::from_value(restored[index]);
                assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
            }
        }
    }

    #[test]
    fn references_stay_canonical_and_invalid_writes_are_atomic() {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return 1").unwrap();
            let values = [
                Value::Nil,
                Value::Boolean(false),
                Value::Boolean(true),
                Value::String(String::from_static(&ctx, b"42")),
                Value::Table(Table::new(&ctx)),
                Value::Function(closure.into()),
                Value::Function(
                    Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)).into(),
                ),
                Value::Thread(Thread::new(ctx)),
                Value::UserData(UserData::new_static(&ctx, 42)),
            ];
            let mut scratch = Scratch::capture(&values);
            let mut restored = values;
            scratch.restore(&mut restored);
            for (actual, expected) in restored.into_iter().zip(values) {
                super::super::tests::assert_identical(actual, expected);
            }
            for index in 0..values.len() {
                let before = scratch.slot(index);
                for tag in [5, 255, 256, u64::MAX] {
                    assert!(!scratch.set(
                        index,
                        Slot {
                            tag,
                            bits: u64::MAX
                        }
                    ));
                    let after = scratch.slot(index);
                    assert_eq!((before.tag, before.bits), (after.tag, after.bits));
                }
                assert!(!scratch.set(
                    values.len(),
                    Slot {
                        tag: INTEGER,
                        bits: 1
                    }
                ));
                assert!(scratch.set(
                    index,
                    Slot {
                        tag: INTEGER,
                        bits: u64::MAX
                    }
                ));
                scratch.restore(&mut restored);
                assert!(matches!(restored[index], Value::Integer(-1)));
            }
        });
    }
}
