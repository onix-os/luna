use super::{Slot, BOOLEAN, INTEGER, NIL, NUMBER, REFERENCE};
use crate::Value;

pub(in crate::jit) fn publish(slots: &[Slot], frame: &mut [Value<'_>]) {
    for (slot, dest) in slots.iter().zip(frame) {
        match slot.tag {
            NIL => *dest = Value::Nil,
            BOOLEAN => *dest = Value::Boolean(slot.bits != 0),
            INTEGER => *dest = Value::Integer(slot.bits as i64),
            NUMBER => *dest = Value::Number(f64::from_bits(slot.bits)),
            REFERENCE => {}
            _ => unreachable!("invalid native scalar tag"),
        }
    }
}

#[cfg(test)]
#[inline(always)]
pub(in crate::jit) fn publish_exact<const N: usize>(slots: &[Slot; N], frame: &mut [Value<'_>; N]) {
    for (slot, dest) in slots.iter().zip(frame) {
        match slot.tag {
            NIL => *dest = Value::Nil,
            BOOLEAN => *dest = Value::Boolean(slot.bits != 0),
            INTEGER => *dest = Value::Integer(slot.bits as i64),
            NUMBER => *dest = Value::Number(f64::from_bits(slot.bits)),
            REFERENCE => {}
            _ => unreachable!("invalid native scalar tag"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Callback, CallbackReturn, Closure, Context, Lua, Table, Thread, UserData};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn values(ctx: Context<'_>) -> Vec<Value<'_>> {
        vec![
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0xfff8_0000_0000_1234)),
            Value::String(ctx.intern(b"publication")),
            Value::Table(Table::new(&ctx)),
            Value::Table(Table::new(&ctx)),
            Value::Function(Closure::load(ctx, None, b"return 42").unwrap().into()),
            Value::Function(Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)).into()),
            Value::Thread(Thread::new(ctx)),
            Value::UserData(UserData::new_static(&ctx, 42i64)),
        ]
    }

    fn compare(slots: &[Slot], initial: &[Value<'_>]) {
        let mut expected = initial.to_vec();
        let mut actual = expected.clone();
        let reference = catch_unwind(AssertUnwindSafe(|| {
            for (slot, dest) in slots.iter().zip(&mut expected) {
                *dest = slot.value(*dest);
            }
        }));
        let candidate = catch_unwind(AssertUnwindSafe(|| publish(slots, &mut actual)));
        assert_eq!(candidate.is_err(), reference.is_err());
        if let (Err(actual), Err(expected)) = (&candidate, &reference) {
            assert_eq!(actual.as_ref().type_id(), expected.as_ref().type_id());
            if let Some(expected) = expected.downcast_ref::<String>() {
                assert_eq!(actual.downcast_ref::<String>(), Some(expected));
            } else if let Some(expected) = expected.downcast_ref::<&'static str>() {
                assert_eq!(actual.downcast_ref::<&'static str>(), Some(expected));
            } else {
                panic!("unexpected scalar panic payload");
            }
        }
        for (actual, expected) in actual.into_iter().zip(expected) {
            super::super::tests::assert_identical(actual, expected);
        }
    }

    #[test]
    fn frame_publication_matches_every_scalar_payload_and_reference_kind() {
        Lua::empty().enter(|ctx| {
            let values = values(ctx);
            for bits in [
                0,
                1,
                2,
                u64::MAX,
                1 << 63,
                0x7ff0_0000_0000_0000,
                0x7ff0_0000_0000_0001,
                0x7ff8_0000_0000_1234,
                0xfff8_0000_0000_5678,
            ] {
                for tag in [NIL, BOOLEAN, INTEGER, NUMBER, REFERENCE] {
                    compare(&vec![Slot { tag, bits }; values.len()], &values);
                }
            }
        });
    }

    #[test]
    fn frame_publication_preserves_all_prefixes_and_unequal_span_tails() {
        Lua::empty().enter(|ctx| {
            let values = values(ctx);
            let initial: Vec<_> = values.iter().copied().cycle().take(260).collect();
            let slots: Vec<_> = (0..260)
                .map(|index| Slot {
                    tag: index % 5,
                    bits: index.wrapping_mul(6364136223846793005),
                })
                .collect();
            for width in 0..=256 {
                compare(&slots[..width], &initial[..width + 3]);
            }
            for width in [0, 1, 3, 7, 8, 16, 63, 64, 255, 256, 260] {
                compare(&slots[..width], &initial[..7]);
                compare(&slots[..7], &initial[..width]);
            }
        });
    }

    #[test]
    fn frame_publication_invalid_tags_preserve_exact_partial_commit_state() {
        Lua::empty().enter(|ctx| {
            let values = values(ctx);
            let initial: Vec<_> = values.iter().copied().cycle().take(256).collect();
            for position in [0, 1, 7, 15, 255] {
                for tag in [5, 6, 255, u64::MAX] {
                    let mut slots = vec![
                        Slot {
                            tag: INTEGER,
                            bits: 73
                        };
                        256
                    ];
                    slots[position].tag = tag;
                    compare(&slots, &initial);
                }
            }
        });
    }

    fn compare_exact<const N: usize>(slots: &[Slot; N], initial: &[Value<'_>; N]) {
        let mut expected = *initial;
        let mut actual = *initial;
        let reference = catch_unwind(AssertUnwindSafe(|| publish(slots, &mut expected)));
        let candidate = catch_unwind(AssertUnwindSafe(|| publish_exact(slots, &mut actual)));
        assert_eq!(candidate.is_err(), reference.is_err());
        if let (Err(actual), Err(expected)) = (&candidate, &reference) {
            assert_eq!(actual.as_ref().type_id(), expected.as_ref().type_id());
            assert_eq!(
                actual.downcast_ref::<&str>(),
                expected.downcast_ref::<&str>()
            );
            assert_eq!(
                actual.downcast_ref::<String>(),
                expected.downcast_ref::<String>()
            );
        }
        for (actual, expected) in actual.into_iter().zip(expected) {
            super::super::tests::assert_identical(actual, expected);
        }
    }

    fn exact_values<const N: usize>(ctx: Context<'_>) {
        for value in values(ctx) {
            let initial = [value; N];
            for bits in [
                0,
                1,
                2,
                u64::MAX,
                1 << 63,
                0x7ff0_0000_0000_0001,
                0xfff8_0000_0000_5678,
            ] {
                for tag in [NIL, BOOLEAN, INTEGER, NUMBER, REFERENCE] {
                    compare_exact(&[Slot { tag, bits }; N], &initial);
                }
            }
            for position in 0..N {
                for tag in [5, 6, 255, u64::MAX] {
                    let mut slots = [Slot {
                        tag: INTEGER,
                        bits: 73,
                    }; N];
                    slots[position].tag = tag;
                    compare_exact(&slots, &initial);
                }
            }
        }
    }

    #[test]
    fn fixed_width_publication_preserves_bits_references_and_partial_panics() {
        Lua::empty().enter(|ctx| {
            exact_values::<0>(ctx);
            exact_values::<1>(ctx);
            exact_values::<2>(ctx);
            exact_values::<3>(ctx);
            exact_values::<4>(ctx);
            exact_values::<5>(ctx);
            exact_values::<6>(ctx);
            exact_values::<7>(ctx);
            exact_values::<8>(ctx);
        });
    }
}
