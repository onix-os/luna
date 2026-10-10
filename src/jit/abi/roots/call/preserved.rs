use super::*;

pub(in crate::jit) struct Preserved<'a, 'gc> {
    roots: &'a mut [Value<'gc>],
    slots: &'a mut [Slot],
}

impl<'a, 'gc> Preserved<'a, 'gc> {
    pub(in crate::jit) fn new(roots: &'a mut [Value<'gc>], slots: &'a mut [Slot]) -> Option<Self> {
        Snapshot::new(roots, slots)?;
        Some(Self { roots, slots })
    }

    pub(in crate::jit) fn snapshot(&mut self) -> Snapshot<'_, 'gc> {
        debug_assert!(Snapshot::new(self.roots, self.slots).is_some());
        Snapshot {
            roots: self.roots,
            slots: self.slots,
        }
    }

    pub(in crate::jit) fn recapture(&mut self, values: &[Value<'gc>]) -> bool {
        super::super::capture(self.roots, self.slots, values)
    }

    pub(in crate::jit) fn materialize(&self, values: &mut [Value<'gc>]) -> bool {
        super::super::materialize(self.roots, self.slots, values)
    }

    /// # Safety
    /// Writes must stay within the slot prefix and preserve tags and root indices.
    /// No use of this pointer may follow another borrow of the register file.
    pub(in crate::jit) unsafe fn native_slots(&mut self) -> *mut Slot {
        self.slots.as_mut_ptr()
    }
}

#[test]
fn malformed_initial_files_refuse_without_mutation() {
    for width in [0usize, 1, 2, 8, 255, 256, 257] {
        for delta in [0usize, 1] {
            let mut roots = vec![Value::Nil; width + delta];
            let mut slots = vec![Slot::from_value(Value::Nil); width];
            assert_eq!(
                Preserved::new(&mut roots, &mut slots).is_some(),
                delta == 0 && width <= 256
            );
            for index in 0..width {
                if cfg!(miri) && index != 0 && index != width / 2 && index != width - 1 {
                    continue;
                }
                for invalid in [
                    Slot {
                        tag: REFERENCE + 1,
                        bits: 0,
                    },
                    Slot {
                        tag: u64::MAX,
                        bits: u64::MAX,
                    },
                    Slot {
                        tag: REFERENCE,
                        bits: index as u64,
                    },
                    Slot {
                        tag: REFERENCE,
                        bits: width as u64,
                    },
                    Slot {
                        tag: REFERENCE,
                        bits: u64::MAX,
                    },
                ] {
                    slots[index] = invalid;
                    assert!(Preserved::new(&mut roots, &mut slots).is_none());
                    for (position, slot) in slots.iter().enumerate() {
                        let expected = if position == index {
                            invalid
                        } else {
                            Slot::from_value(Value::Nil)
                        };
                        assert_eq!((slot.tag, slot.bits), (expected.tag, expected.bits));
                    }
                    assert!(roots.iter().all(|value| matches!(value, Value::Nil)));
                    slots[index] = Slot::from_value(Value::Nil);
                }
            }
        }
    }
}

#[test]
fn recapture_calls_and_native_reborrows_preserve_root_identity() {
    crate::Lua::empty().enter(|ctx| {
        let a = Value::Table(crate::Table::new(&ctx));
        let b = Value::Table(crate::Table::new(&ctx));
        let values = [
            a,
            Value::Integer(1),
            b,
            Value::Number(-0.0),
            a,
            Value::Nil,
            b,
            Value::Nil,
        ];
        let mut roots = [Value::Nil; 8];
        let mut slots = [Slot::from_value(Value::Nil); 8];
        assert!(super::super::capture(&mut roots, &mut slots, &values));
        let mut file = Preserved::new(&mut roots, &mut slots).unwrap();
        for round in 0..16 {
            let before: Vec<_> = file
                .slots
                .iter()
                .map(|slot| (slot.tag, slot.bits))
                .collect();
            assert!(!file.recapture(&values[..7]));
            assert_eq!(
                file.slots
                    .iter()
                    .map(|slot| (slot.tag, slot.bits))
                    .collect::<Vec<_>>(),
                before
            );
            assert!(file.recapture(&values));
            let pointer = unsafe { file.native_slots() };
            unsafe {
                pointer.add(7).write(pointer.add(2).read());
                pointer
                    .add(2)
                    .write(Slot::from_value(Value::Integer(round)));
            }
            let mut expected = values;
            expected[7] = b;
            expected[2] = Value::Integer(round);
            let mut actual = [Value::Nil; 8];
            assert!(file.snapshot().publish(&mut actual));
            for (actual, expected) in actual.into_iter().zip(expected) {
                super::super::tests::identical(actual, expected);
            }
            let pointer = unsafe { file.native_slots() };
            unsafe {
                pointer.add(6).write(pointer.add(4).read());
            }
            assert!(file.snapshot().call(3, 2, 4, 0, 2, Some(1), (7, 9, 11)));
            assert!(Snapshot::new(file.roots, file.slots).is_some());
            let mut published = [Value::Nil; 8];
            assert!(file.snapshot().publish(&mut published));
            assert!(file.materialize(&mut actual));
            for (actual, expected) in actual.into_iter().zip(published) {
                super::super::tests::identical(actual, expected);
            }
        }
        assert_eq!(
            std::mem::size_of::<Preserved<'_, '_>>(),
            4 * std::mem::size_of::<usize>()
        );
    });
}
