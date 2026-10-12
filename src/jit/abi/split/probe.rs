use super::*;
use std::hint::black_box;

#[inline(never)]
fn step_aos(slots: &mut [Slot; 7]) {
    if slots[0].tag == INTEGER {
        slots[0].bits = slots[0].bits.wrapping_add(1);
    }
}

#[inline(never)]
fn step_split(scratch: &mut Scratch<7>) {
    if scratch.tags[0] == INTEGER as u8 {
        scratch.bits[0] = scratch.bits[0].wrapping_add(1);
    }
}

#[inline(never)]
fn transport_probe_aos(values: &mut [Value<'_>; 7], count: usize) {
    for _ in 0..count {
        let mut storage = [std::mem::MaybeUninit::<Slot>::uninit(); 7];
        for (slot, value) in storage.iter_mut().zip(values.iter().copied()) {
            slot.write(Slot::from_value(value));
        }
        // All seven slots are initialized before this typed borrow.
        let slots = unsafe { &mut *storage.as_mut_ptr().cast::<[Slot; 7]>() };
        black_box(step_aos as fn(&mut [Slot; 7]))(slots);
        for (slot, value) in slots.iter().copied().zip(values.iter_mut()) {
            slot.write_back(value);
        }
        black_box(&*values);
    }
}

#[inline(never)]
fn transport_probe_split(values: &mut [Value<'_>; 7], count: usize) {
    for _ in 0..count {
        let mut scratch = Scratch::capture(values);
        black_box(step_split as fn(&mut Scratch<7>))(&mut scratch);
        scratch.restore(values);
        black_box(&*values);
    }
}

fn check<'gc>(values: [Value<'gc>; 7], count: usize) {
    let mut aos = values;
    let mut split = values;
    transport_probe_aos(&mut aos, count);
    transport_probe_split(&mut split, count);
    for (left, right) in aos.into_iter().zip(split) {
        super::super::tests::assert_identical(left, right);
    }
    assert!(matches!(aos[0], Value::Integer(value) if value == count as i64));
}

#[test]
fn both_transports_match_mixed_values_across_opaque_calls() {
    crate::Lua::empty().enter(|ctx| {
        let values = [
            Value::Integer(0),
            Value::Integer(i64::MIN),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            Value::Table(crate::Table::new(&ctx)),
            Value::Boolean(true),
            Value::Nil,
        ];
        for count in [0, 1, 2, 17, 64, 257] {
            check(values, count);
        }
    });
}

#[test]
#[ignore = "synthetic transport cost probe, not a native performance gate"]
fn profile_roundtrip_transport() {
    let split = match std::env::var("LUNA_SPLIT_SCRATCH").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("LUNA_SPLIT_SCRATCH must be 0 or 1"),
    };
    let mixed = match std::env::var("LUNA_SPLIT_MIXED").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("LUNA_SPLIT_MIXED must be 0 or 1"),
    };
    crate::Lua::empty().enter(|ctx| {
        let mut values = if mixed {
            [
                Value::Integer(0),
                Value::Number(-0.0),
                Value::Number(1.5),
                Value::Table(crate::Table::new(&ctx)),
                Value::Boolean(true),
                Value::Nil,
                Value::String(crate::String::from_static(&ctx, b"reference")),
            ]
        } else {
            [Value::Integer(0); 7]
        };
        let before = values;
        if split {
            transport_probe_split(&mut values, 100_000);
        } else {
            transport_probe_aos(&mut values, 100_000);
        }
        assert!(matches!(values[0], Value::Integer(100_000)));
        for index in 1..7 {
            super::super::tests::assert_identical(values[index], before[index]);
        }
        eprintln!("split={split} mixed={mixed} steps=100000 result=100000");
    });
}
