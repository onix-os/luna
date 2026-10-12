use super::*;
use crate::jit::abi::tests::assert_identical;

pub(super) fn corpus(kernel: &Kernel<'_>, source: usize, dest: usize, width: usize) {
    let mut bits = 0x1234_5678_9abc_def0u64;
    for round in 0..128 {
        bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
        let value = [i64::MIN, i64::MAX, 0, -1, bits as i64][round % 5];
        let delta = [i64::MAX, i64::MIN, -1, 0, 1, bits as i64][round % 6];
        let mut values = vec![Value::Boolean(true); width + 2];
        values[1 + source] = Value::Integer(value);
        if source != dest {
            values[1 + dest] = Value::Integer(42);
        }
        let mut expected = values.clone();
        expected[1 + dest] = Value::Integer(value.wrapping_add(delta));
        assert_eq!(
            kernel.bind(&mut values[1..=width]).unwrap().invoke(delta),
            1
        );
        for (actual, expected) in values.into_iter().zip(expected) {
            assert_identical(actual, expected);
        }
    }
}

#[test]
fn masks_and_frame_boundaries() {
    let indices = [0, 63, 64, 127, 128, 255];
    let mask = Mask::new(&indices).unwrap();
    assert_eq!(mask.indices().collect::<Vec<_>>(), indices);
    assert_eq!(Mask::new(&[0, 0]), Mask::new(&[0]));
    assert!(Mask::new(&[256]).is_none());
    assert!(Mask::new(&[usize::MAX]).is_none());
    assert!(!mask.contains(usize::MAX));
    assert!(rust_kernel::<0, 1>(Mask::new(&[0]).unwrap()).is_none());
    assert!(rust_kernel::<0, 0>(Mask::new(&[]).unwrap()).is_none());
    assert_eq!(mem::size_of::<Header>(), 3 * mem::size_of::<usize>());
    let kernel = rust_kernel::<0, 0>(Mask::new(&[0]).unwrap()).unwrap();
    for width in [0, 1, 2, 3, 7, 8, 16, 256, 257] {
        let mut values = vec![Value::Integer(7); width];
        let window = kernel.bind(&mut values);
        assert_eq!(window.is_some(), (1..=256).contains(&width));
        if let Some(window) = window {
            assert_eq!(window.invoke(1), 1);
            assert_identical(values[0], Value::Integer(8));
            for &value in &values[1..] {
                assert_identical(value, Value::Integer(7));
            }
        }
    }
}

#[test]
fn rust_field_access_matches_wrapping_and_preserves_neighbors() {
    corpus(
        &rust_kernel::<0, 0>(Mask::new(&[0]).unwrap()).unwrap(),
        0,
        0,
        1,
    );
    corpus(
        &rust_kernel::<1, 5>(Mask::new(&[1, 5]).unwrap()).unwrap(),
        1,
        5,
        7,
    );
    corpus(
        &rust_kernel::<255, 0>(Mask::new(&[255, 0]).unwrap()).unwrap(),
        255,
        0,
        256,
    );
    corpus(
        &rust_kernel::<0, 255>(Mask::new(&[0, 255]).unwrap()).unwrap(),
        0,
        255,
        256,
    );
}

pub(super) fn kinds_and_rebinding(kernel: &Kernel<'_>) {
    crate::Lua::empty().enter(|ctx| {
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Number(42.0),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_1234_5678_9abc)),
            Value::String(crate::String::from_slice(&ctx, b"strided")),
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
        for value in kinds {
            let mut values = [value, Value::Integer(7), value, Value::Integer(9)];
            assert_eq!(kernel.bind(&mut values).unwrap().invoke(3), 1);
            assert_identical(values[0], value);
            assert_identical(values[1], Value::Integer(7));
            assert_identical(values[2], value);
            assert_identical(values[3], Value::Integer(10));
            for index in [1, 3] {
                let original = values[index];
                values[index] = value;
                let before = values;
                assert!(kernel.bind(&mut values).is_none());
                for (actual, expected) in values.into_iter().zip(before) {
                    assert_identical(actual, expected);
                }
                values[index] = original;
            }
            {
                let _unused = kernel.bind(&mut values).unwrap();
            }
            values[1] = Value::Integer(i64::MAX);
            assert_eq!(kernel.bind(&mut values).unwrap().invoke(1), 1);
            assert_identical(values[3], Value::Integer(i64::MIN));
            let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _window = kernel.bind(&mut values).unwrap();
                panic!("drop window");
            }));
            assert!(unwind.is_err());
            values[1] = Value::Integer(73);
            assert_eq!(kernel.bind(&mut values).unwrap().invoke(2), 1);
            assert_identical(values[3], Value::Integer(75));
        }
    });
}

#[test]
fn rust_admission_refuses_nonintegers_and_rebinds_current_values() {
    kinds_and_rebinding(&rust_kernel::<1, 3>(Mask::new(&[1, 3]).unwrap()).unwrap());
    let kernel = rust_kernel::<1, 3>(Mask::new(&[1, 2, 3]).unwrap()).unwrap();
    let mut values = [Value::Integer(1); 4];
    values[2] = Value::Nil;
    assert!(kernel.bind(&mut values).is_none());
    values[2] = Value::Integer(2);
    assert_eq!(kernel.bind(&mut values).unwrap().invoke(7), 1);
    assert_identical(values[3], Value::Integer(8));
}

pub(super) fn entry_guards(kernel: &Kernel<'_>) {
    assert_eq!(unsafe { (kernel.entry)(ptr::null(), 1) }, 0);
    let mut payload = 42i64;
    let short = Header {
        base: ptr::from_mut(&mut payload).cast(),
        stride: 8,
        len: 0,
    };
    assert_eq!(unsafe { (kernel.entry)(&short, 1) }, 0);
    let null = Header {
        base: ptr::null_mut(),
        stride: 8,
        len: 256,
    };
    assert_eq!(unsafe { (kernel.entry)(&null, 1) }, 0);
    assert_eq!(payload, 42);
}

#[test]
fn rust_entry_declines_null_and_short_headers() {
    entry_guards(&rust_kernel::<1, 3>(Mask::new(&[1, 3]).unwrap()).unwrap());
}
