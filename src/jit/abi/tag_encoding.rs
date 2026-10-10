use crate::Value;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Encoded {
    tag: u64,
    bits: u64,
}

#[inline(always)]
fn encode<const OFFSET: u64>(value: Value<'_>) -> Encoded {
    match value {
        Value::Nil => Encoded {
            tag: OFFSET,
            bits: 0,
        },
        Value::Boolean(value) => Encoded {
            tag: OFFSET + 1,
            bits: u64::from(value),
        },
        Value::Integer(value) => Encoded {
            tag: OFFSET + 2,
            bits: value as u64,
        },
        Value::Number(value) => Encoded {
            tag: OFFSET + 3,
            bits: value.to_bits(),
        },
        _ => Encoded {
            tag: OFFSET + 4,
            bits: 0,
        },
    }
}

#[inline(always)]
fn import<const OFFSET: u64>(values: &[Value<'_>], slots: &mut [Encoded; 7]) {
    for (slot, value) in slots.iter_mut().zip(values[..7].iter().copied()) {
        *slot = encode::<OFFSET>(value);
    }
}

#[inline(never)]
fn original_import(values: &[Value<'_>], slots: &mut [Encoded; 7]) {
    import::<0>(values, slots);
}

#[inline(never)]
fn offset_import(values: &[Value<'_>], slots: &mut [Encoded; 7]) {
    import::<2>(values, slots);
}

#[inline(always)]
fn split_encode(value: Value<'_>) -> Encoded {
    let tag = match value {
        Value::Nil => 0,
        Value::Boolean(_) => 1,
        Value::Integer(_) => 2,
        Value::Number(_) => 3,
        _ => 4,
    };
    let bits = match value {
        Value::Boolean(value) => u64::from(value),
        Value::Integer(value) => value as u64,
        Value::Number(value) => value.to_bits(),
        _ => 0,
    };
    Encoded { tag, bits }
}

#[inline(never)]
fn split_import(values: &[Value<'_>], slots: &mut [Encoded; 7]) {
    for (slot, value) in slots.iter_mut().zip(values[..7].iter().copied()) {
        *slot = split_encode(value);
    }
}

#[inline(never)]
fn integer_prefix_import(values: &[Value<'_>], slots: &mut [Encoded; 7]) {
    let values = &values[..7];
    for index in 0..7 {
        if let Value::Integer(value) = values[index] {
            slots[index] = Encoded {
                tag: 2,
                bits: value as u64,
            };
        } else {
            for (slot, value) in slots[index..]
                .iter_mut()
                .zip(values[index..].iter().copied())
            {
                *slot = encode::<0>(value);
            }
            return;
        }
    }
}

#[test]
fn alternative_tags_preserve_payloads_without_value_layout_access() {
    let mut lua = crate::Lua::empty();
    lua.enter(|ctx| {
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            Value::String(crate::String::from_slice(&ctx, b"root")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(
                crate::Closure::load(ctx, None, b"return 42")
                    .unwrap()
                    .into(),
            ),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 42i64)),
        ];
        let expected = [
            (0, 0),
            (1, 0),
            (1, 1),
            (2, i64::MIN as u64),
            (2, i64::MAX as u64),
            (3, (-0.0f64).to_bits()),
            (3, 0x7ff8_0000_0000_1234),
            (4, 0),
            (4, 0),
            (4, 0),
            (4, 0),
            (4, 0),
            (4, 0),
        ];
        for (index, value) in values.into_iter().enumerate() {
            let input = std::hint::black_box([value; 7]);
            let mut original = [Encoded { tag: 99, bits: 99 }; 7];
            let mut offset = original;
            let mut split = original;
            original_import(&input, &mut original);
            offset_import(&input, &mut offset);
            split_import(&input, &mut split);
            assert_eq!(split, original);
            for (original, offset) in original.into_iter().zip(offset) {
                assert_eq!((original.tag, original.bits), expected[index]);
                assert_eq!(
                    (offset.tag, offset.bits),
                    (expected[index].0 + 2, expected[index].1)
                );
                let production = super::Slot::from_value(value);
                assert_eq!((production.tag, production.bits), expected[index]);
            }
            for prefix in 0..=7 {
                let mut input =
                    std::array::from_fn::<_, 7, _>(|lane| Value::Integer(lane as i64 - 3));
                input[prefix..].fill(value);
                let input = std::hint::black_box(input);
                let mut original = [Encoded { tag: 99, bits: 99 }; 7];
                let mut prefix_slots = original;
                original_import(&input, &mut original);
                integer_prefix_import(&input, &mut prefix_slots);
                assert_eq!(prefix_slots, original, "value {index}, prefix {prefix}");
                let mut actual = [std::mem::MaybeUninit::uninit(); 7];
                super::Slot::import_integer_nil_prefix(&mut actual, &input);
                for (slot, expected) in actual.into_iter().zip(original) {
                    let slot = unsafe { slot.assume_init() };
                    assert_eq!((slot.tag, slot.bits), (expected.tag, expected.bits));
                }
            }
        }
    });
}

#[test]
fn alternative_encoding_preserves_generated_numeric_bits() {
    let mut bits = 0x1234_5678_9abc_def0u64;
    for _ in 0..1024 {
        bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
        let integer = encode::<2>(Value::Integer(bits as i64));
        let number = encode::<2>(Value::Number(f64::from_bits(bits)));
        assert_eq!(integer, Encoded { tag: 4, bits });
        assert_eq!(number, Encoded { tag: 5, bits });
        assert_eq!(
            split_encode(Value::Integer(bits as i64)),
            encode::<0>(Value::Integer(bits as i64))
        );
        assert_eq!(
            split_encode(Value::Number(f64::from_bits(bits))),
            encode::<0>(Value::Number(f64::from_bits(bits)))
        );
    }
}

#[test]
fn batch_integer_import_initializes_each_width_and_preserves_canaries() {
    for width in [0, 1, 7, 8, 9, 16, 32, 64, 128, 255, 256] {
        for prefix in 0..=width {
            let values: Vec<_> = (0..width)
                .map(|index| {
                    if index == prefix {
                        Value::Boolean(true)
                    } else {
                        Value::Integer(-(index as i64))
                    }
                })
                .collect();
            let canary = super::Slot {
                tag: 99,
                bits: u64::MAX,
            };
            let mut slots = vec![std::mem::MaybeUninit::new(canary); width + 2];
            super::Slot::import_integer_nil_prefix(&mut slots[1..=width], &values);
            for boundary in [0, width + 1] {
                let slot = unsafe { slots[boundary].assume_init() };
                assert_eq!((slot.tag, slot.bits), (canary.tag, canary.bits));
            }
            for (slot, value) in slots[1..=width].iter().zip(values) {
                let slot = unsafe { slot.assume_init() };
                let expected = encode::<0>(value);
                assert_eq!((slot.tag, slot.bits), (expected.tag, expected.bits));
            }
        }
    }
}
