use super::*;

pub(crate) struct Snapshot<'a, 'gc> {
    roots: &'a [Value<'gc>],
    slots: &'a mut [Slot],
}

impl<'a, 'gc> Snapshot<'a, 'gc> {
    #[inline(always)]
    pub(crate) fn new(roots: &'a [Value<'gc>], slots: &'a mut [Slot]) -> Option<Self> {
        if roots.len() != slots.len()
            || slots.len() > 256
            || slots.iter().any(|slot| {
                slot.tag > REFERENCE
                    || (slot.tag == REFERENCE
                        && usize::try_from(slot.bits)
                            .ok()
                            .and_then(|index| roots.get(index))
                            .is_none_or(|value| Slot::from_value(*value).tag != REFERENCE))
            })
        {
            return None;
        }
        Some(Self { roots, slots })
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    #[inline(always)]
    pub(crate) fn get(&self, index: usize) -> Option<Value<'gc>> {
        let slot = *self.slots.get(index)?;
        Some(if slot.tag == REFERENCE {
            self.roots[slot.bits as usize]
        } else {
            slot.value(Value::Nil)
        })
    }

    #[cfg(test)]
    pub(crate) fn integer(&self, index: usize) -> Option<i64> {
        let slot = self.slots.get(index)?;
        (slot.tag == super::super::INTEGER).then_some(slot.bits as i64)
    }

    #[inline(always)]
    pub(crate) fn publish(&self, values: &mut [Value<'gc>]) -> bool {
        if values.len() < self.slots.len() {
            return false;
        }
        for (slot, value) in self.slots.iter().zip(values) {
            *value = if slot.tag == REFERENCE {
                self.roots[slot.bits as usize]
            } else {
                slot.value(Value::Nil)
            };
        }
        true
    }

    #[inline(always)]
    pub(crate) fn call(
        &mut self,
        function: usize,
        arguments: usize,
        width: usize,
        read: usize,
        result: usize,
        capture: Option<usize>,
        output: (i64, i64, i64),
    ) -> bool {
        if function >= self.slots.len()
            || arguments > self.slots.len() - function - 1
            || arguments > width
            || width > 256
            || read >= width
            || result >= width
            || capture.is_some_and(|index| index >= function)
        {
            return false;
        }
        self.slots
            .copy_within(function + 1..function + 1 + arguments, function);
        self.slots[function + arguments..].fill(Slot::from_value(Value::Nil));
        for (index, value) in [(function + read, output.0), (function + result, output.1)] {
            if let Some(slot) = self.slots.get_mut(index) {
                *slot = Slot::from_value(Value::Integer(value));
            }
        }
        if let Some(index) = capture {
            self.slots[index] = Slot::from_value(Value::Integer(output.2));
        }
        true
    }
}

#[test]
fn virtual_call_matches_full_argument_shift_resize_and_scalar_writes() {
    crate::Lua::empty().enter(|ctx| {
        let reference = Value::Table(crate::Table::new(&ctx));
        for caller_width in [1, 2, 8, 16, 255, 256] {
            for function in [0, caller_width / 2, caller_width - 1] {
                for callee_width in [1, 2, 7, 16, 255, 256] {
                    for arguments in [0, (caller_width - function - 1).min(callee_width)] {
                        for (read, result) in [(0, 0), (0, callee_width - 1), (callee_width - 1, 0)]
                        {
                            let initial: Vec<_> = (0..caller_width)
                                .map(|index| {
                                    if index % 2 == 0 {
                                        reference
                                    } else {
                                        Value::Integer(index as i64)
                                    }
                                })
                                .collect();
                            let mut roots = vec![Value::Nil; caller_width];
                            let mut slots = vec![Slot::from_value(Value::Nil); caller_width];
                            assert!(capture(&mut roots, &mut slots, &initial));
                            let capture = (function > 0).then_some(0);
                            let mut snapshot = Snapshot::new(&roots, &mut slots).unwrap();
                            assert!(snapshot.call(
                                function,
                                arguments,
                                callee_width,
                                read,
                                result,
                                capture,
                                (7, 9, 11)
                            ));
                            let mut expected = initial.clone();
                            expected.copy_within(function + 1..function + 1 + arguments, function);
                            expected.resize(function + callee_width, Value::Nil);
                            expected[function + arguments..].fill(Value::Nil);
                            expected[function + read] = Value::Integer(7);
                            expected[function + result] = Value::Integer(9);
                            if let Some(index) = capture {
                                expected[index] = Value::Integer(11);
                            }
                            expected.resize(caller_width, Value::Nil);
                            let mut actual = vec![Value::Nil; caller_width];
                            assert!(materialize(&roots, &slots, &mut actual));
                            for (actual, expected) in actual.into_iter().zip(expected) {
                                super::tests::identical(actual, expected);
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn malformed_snapshots_and_call_spans_refuse_without_slot_changes() {
    let nil = Slot::from_value(Value::Nil);
    for invalid in [
        Slot {
            tag: REFERENCE + 1,
            bits: 0,
        },
        Slot {
            tag: REFERENCE,
            bits: 0,
        },
        Slot {
            tag: REFERENCE,
            bits: u64::MAX,
        },
    ] {
        for position in [0, 3] {
            let mut slots = [nil; 4];
            slots[position] = invalid;
            assert!(Snapshot::new(&[Value::Nil; 4], &mut slots).is_none());
            assert_eq!(
                (slots[position].tag, slots[position].bits),
                (invalid.tag, invalid.bits)
            );
        }
    }
    assert!(Snapshot::new(&[Value::Nil; 3], &mut [nil; 4]).is_none());
    assert!(Snapshot::new(&[Value::Nil; 257], &mut [nil; 257]).is_none());
    for (function, arguments, width, read, result, capture) in [
        (4, 0, 1, 0, 0, None),
        (usize::MAX, 0, 1, 0, 0, None),
        (1, usize::MAX, 3, 0, 0, None),
        (1, 2, 1, 0, 0, None),
        (1, 0, 257, 0, 0, None),
        (1, 0, 0, 0, 0, None),
        (1, 0, 2, 2, 0, None),
        (1, 0, 2, 0, 2, None),
        (1, 0, 2, 0, 0, Some(1)),
        (1, 0, 2, 0, 0, Some(usize::MAX)),
    ] {
        let mut slots = [Slot::from_value(Value::Integer(7)); 4];
        let mut snapshot = Snapshot::new(&[Value::Nil; 4], &mut slots).unwrap();
        assert!(!snapshot.call(function, arguments, width, read, result, capture, (1, 2, 3)));
        assert!(slots
            .iter()
            .all(|slot| slot.tag == super::super::INTEGER && slot.bits == 7));
    }
}

#[test]
fn snapshot_reads_preserve_all_scalar_bits_and_reference_identities() {
    crate::Lua::empty().enter(|ctx| {
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000055)),
            crate::String::from_static(&ctx, b"rooted-call").into(),
            crate::Table::new(&ctx).into(),
            crate::Closure::load(ctx, None, &b"return"[..])
                .unwrap()
                .into(),
            crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            crate::Thread::new(ctx).into(),
            crate::UserData::new_static(&ctx, 7).into(),
        ];
        let mut roots = [Value::Nil; 13];
        let mut slots = [Slot::from_value(Value::Nil); 13];
        assert!(capture(&mut roots, &mut slots, &values));
        slots.reverse();
        let snapshot = Snapshot::new(&roots, &mut slots).unwrap();
        let mut short = [Value::Integer(99); 12];
        assert!(!snapshot.publish(&mut short));
        assert!(short
            .iter()
            .all(|value| matches!(value, Value::Integer(99))));
        let mut published = [Value::Integer(99); 14];
        assert!(snapshot.publish(&mut published));
        assert!(matches!(published[13], Value::Integer(99)));
        for (index, expected) in values.into_iter().rev().enumerate() {
            super::tests::identical(snapshot.get(index).unwrap(), expected);
            assert_eq!(
                snapshot.integer(index),
                match snapshot.get(index).unwrap() {
                    Value::Integer(value) => Some(value),
                    _ => None,
                }
            );
            super::tests::identical(published[index], expected);
        }
        assert!(snapshot.get(13).is_none());
        assert!(snapshot.integer(13).is_none());
        assert!(snapshot.integer(usize::MAX).is_none());
    });
}
