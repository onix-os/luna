use super::*;
use crate::jit::abi::{Slot, INTEGER};
use std::hint::black_box;

#[inline(never)]
fn packed<const N: usize>(
    kernel: &Kernel<'_>,
    values: &mut [Value<'_>],
    source: usize,
    dest: usize,
) -> u32 {
    let mut scratch = [mem::MaybeUninit::<Slot>::uninit(); N];
    for (slot, value) in scratch.iter_mut().zip(values[..N].iter().copied()) {
        slot.write(Slot::from_value(value));
    }
    let slots = unsafe { std::slice::from_raw_parts_mut(scratch.as_mut_ptr().cast::<Slot>(), N) };
    if slots[source].tag != INTEGER || slots[dest].tag != INTEGER {
        return 0;
    }
    let header = Header {
        base: slots
            .as_mut_ptr()
            .cast::<u8>()
            .wrapping_add(mem::offset_of!(Slot, bits)),
        stride: mem::size_of::<Slot>(),
        len: N,
    };
    let status = unsafe { (kernel.entry)(&header, 1) };
    for (slot, value) in slots.iter().zip(&mut values[..N]) {
        slot.write_back(value);
    }
    status
}

#[inline(never)]
fn strided(kernel: &Kernel<'_>, values: &mut [Value<'_>]) -> u32 {
    kernel.bind(values).map_or(0, |window| window.invoke(1))
}

#[inline(never)]
fn profile_transport<const N: usize>(
    kernel: &Kernel<'_>,
    values: &mut [Value<'_>],
    source: usize,
    dest: usize,
    use_strided: bool,
    count: usize,
) -> u32 {
    let mut completed = 0;
    for _ in 0..count {
        completed += if use_strided {
            strided(black_box(kernel), black_box(values))
        } else {
            packed::<N>(black_box(kernel), black_box(values), source, dest)
        };
    }
    black_box(completed)
}

fn run<const N: usize>(shape: &str, use_strided: bool, dense: bool) {
    let (source, dest) = if N == 1 { (0, 0) } else { (1, N - 2) };
    let required = if dense {
        Mask::new(&(0..N).collect::<Vec<_>>()).unwrap()
    } else {
        Mask::new(&[source, dest]).unwrap()
    };
    let code = compile(source, dest, required).unwrap();
    crate::Lua::empty().enter(|ctx| {
        let reference = Value::Table(crate::Table::new(&ctx));
        let mut values = (0..N)
            .map(|index| {
                if dense {
                    Value::Integer(index as i64)
                } else {
                    [
                        Value::Nil,
                        Value::Boolean(true),
                        Value::Number(-0.0),
                        reference,
                    ][index % 4]
                }
            })
            .collect::<Vec<_>>();
        values[source] = Value::Integer(73);
        values[dest] = Value::Integer(73);
        let original = values.clone();
        let kernel = code.kernel();
        let completed =
            profile_transport::<N>(&kernel, &mut values, source, dest, use_strided, 100_000);
        assert_eq!(completed, 100_000);
        for (index, (&actual, mut expected)) in values.iter().zip(original).enumerate() {
            if index == dest {
                expected = Value::Integer(if source == dest { 100_073 } else { 74 });
            }
            crate::jit::abi::tests::assert_identical(actual, expected);
        }
        println!(
            "shape={shape} strided={use_strided} calls=100000 completed={completed} validated=true"
        );
    });
}

#[test]
#[ignore]
fn profile_generated_transport() {
    let shape = std::env::var("LUNA_STRIDED_SHAPE").unwrap();
    let use_strided = match std::env::var("LUNA_STRIDED").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("LUNA_STRIDED must be 0 or 1"),
    };
    match shape.as_str() {
        "single" => run::<1>(&shape, use_strided, true),
        "mixed7" => run::<7>(&shape, use_strided, false),
        "dense7" => run::<7>(&shape, use_strided, true),
        "mixed16" => run::<16>(&shape, use_strided, false),
        "dense16" => run::<16>(&shape, use_strided, true),
        "mixed256" => run::<256>(&shape, use_strided, false),
        "dense256" => run::<256>(&shape, use_strided, true),
        _ => panic!("unknown strided shape"),
    }
}
