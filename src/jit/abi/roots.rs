use super::{Slot, REFERENCE};
use crate::Value;

pub(crate) fn capture<'gc>(
    roots: &mut [Value<'gc>],
    slots: &mut [Slot],
    values: &[Value<'gc>],
) -> bool {
    if roots.len() != slots.len() || slots.len() > 256 || values.len() < slots.len() {
        return false;
    }
    for (index, ((root, slot), value)) in roots.iter_mut().zip(slots).zip(values).enumerate() {
        *root = *value;
        *slot = Slot::from_value(*value);
        if slot.tag == REFERENCE {
            slot.bits = index as u64;
        }
    }
    true
}

pub(crate) fn materialize<'gc>(
    roots: &[Value<'gc>],
    slots: &[Slot],
    values: &mut [Value<'gc>],
) -> bool {
    if roots.len() != slots.len()
        || slots.len() > 256
        || values.len() < slots.len()
        || slots.iter().any(|slot| {
            slot.tag > REFERENCE
                || (slot.tag == REFERENCE
                    && usize::try_from(slot.bits)
                        .ok()
                        .and_then(|index| roots.get(index))
                        .is_none_or(|value| Slot::from_value(*value).tag != REFERENCE))
        })
    {
        return false;
    }
    for (slot, value) in slots.iter().zip(values) {
        *value = if slot.tag == REFERENCE {
            roots[slot.bits as usize]
        } else {
            slot.value(*value)
        };
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Lua, Table};

    fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
        match (actual, expected) {
            (Value::Nil, Value::Nil) => {}
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (Value::String(a), Value::String(b)) => assert_eq!(a, b),
            (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
            (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
            (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
            (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
            _ => panic!("snapshot value mismatch"),
        }
    }

    #[test]
    fn all_value_kinds_preserve_identity_and_bits_at_register_boundaries() {
        Lua::empty().enter(|ctx| {
            let values = [
                Value::Nil,
                Value::Boolean(false),
                Value::Boolean(true),
                Value::Integer(i64::MIN),
                Value::Integer(i64::MAX),
                Value::Number(-0.0),
                Value::Number(f64::from_bits(0x7ff8000000000055)),
                crate::String::from_static(&ctx, b"root").into(),
                Table::new(&ctx).into(),
                crate::Closure::load(ctx, None, &b"return"[..])
                    .unwrap()
                    .into(),
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
                crate::Thread::new(ctx).into(),
                crate::UserData::new_static(&ctx, 7).into(),
            ];
            for value in values {
                for source in [0, 1, 255] {
                    for destination in [0, 1, 255] {
                        let mut canonical = [Value::Nil; 256];
                        canonical[source] = value;
                        let mut roots = [Value::Nil; 256];
                        let mut slots = [Slot::from_value(Value::Nil); 256];
                        assert!(capture(&mut roots, &mut slots, &canonical));
                        slots[destination] = slots[source];
                        canonical.fill(Value::Nil);
                        assert!(materialize(&roots, &slots, &mut canonical));
                        identical(canonical[destination], value);
                        assert!(capture(&mut roots, &mut slots, &canonical));
                        slots[source] = slots[destination];
                        assert!(materialize(&roots, &slots, &mut canonical));
                        identical(canonical[source], value);
                    }
                }
            }
        });
    }

    #[test]
    fn copies_keep_snapshot_identity_after_sources_are_overwritten() {
        Lua::empty().enter(|ctx| {
            let a = Value::Table(Table::new(&ctx));
            let b = Value::Table(Table::new(&ctx));
            let mut values = [a, b, Value::Integer(9), Value::Nil];
            let mut roots = [Value::Nil; 4];
            let mut slots = [Slot::from_value(Value::Nil); 4];
            assert!(capture(&mut roots, &mut slots, &values));
            slots[3] = slots[0];
            slots[0] = slots[1];
            slots[1] = slots[3];
            slots[2] = slots[1];
            values.fill(Value::Nil);
            assert!(materialize(&roots, &slots, &mut values));
            for (actual, expected) in values.into_iter().zip([b, a, a, a]) {
                let (Value::Table(actual), Value::Table(expected)) = (actual, expected) else {
                    panic!()
                };
                assert_eq!(actual, expected);
            }
        });
    }

    #[test]
    fn invalid_indices_tags_and_lengths_refuse_without_writes() {
        let mut values = [Value::Integer(7); 3];
        let roots = [Value::Nil; 3];
        for slot in [
            Slot {
                tag: REFERENCE,
                bits: 0,
            },
            Slot {
                tag: REFERENCE,
                bits: 3,
            },
            Slot {
                tag: REFERENCE,
                bits: u64::MAX,
            },
            Slot {
                tag: REFERENCE + 1,
                bits: 0,
            },
        ] {
            let slots = [
                Slot::from_value(Value::Nil),
                slot,
                Slot::from_value(Value::Nil),
            ];
            assert!(!materialize(&roots, &slots, &mut values));
            assert!(values
                .iter()
                .all(|value| matches!(value, Value::Integer(7))));
        }
        assert!(!materialize(
            &roots[..2],
            &[Slot::from_value(Value::Nil); 3],
            &mut values
        ));
        let mut output = [Slot::from_value(Value::Integer(3)); 3];
        assert!(!capture(&mut [Value::Nil; 2], &mut output, &values));
        assert!(output.iter().all(|slot| slot.bits == 3));
        assert!(!capture(
            &mut [Value::Nil; 257],
            &mut [Slot::from_value(Value::Nil); 257],
            &[Value::Nil; 257]
        ));
    }
}
