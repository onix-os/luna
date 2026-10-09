use super::*;

pub(crate) struct Snapshot<'a, 'gc> {
    roots: &'a [Value<'gc>],
    slots: &'a mut [Slot],
}

impl<'a, 'gc> Snapshot<'a, 'gc> {
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

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn get(&self, index: usize) -> Option<Value<'gc>> {
        let slot = *self.slots.get(index)?;
        Some(if slot.tag == REFERENCE {
            self.roots[slot.bits as usize]
        } else {
            slot.value(Value::Nil)
        })
    }

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
