use std::cell::Cell;

use super::{float_modulo, integer_floor_divide, integer_modulo, Constant};

type Value = Constant<&'static [u8]>;

fn normalized_number<S: AsRef<[u8]>>(value: &Constant<S>) -> Option<f64> {
    match value.to_numeric() {
        Some(Constant::Integer(value)) => Some(value as f64),
        Some(Constant::Number(value)) => Some(value),
        _ => None,
    }
}

fn assert_conversion(value: Value) {
    assert_eq!(
        value.to_number().map(f64::to_bits),
        normalized_number(&value).map(f64::to_bits),
        "{value:?}"
    );
    let extracted = match value {
        Constant::Integer(_) | Constant::Number(_) => normalized_number(&value),
        _ => None,
    };
    assert_eq!(
        value.as_number().map(f64::to_bits),
        extracted.map(f64::to_bits)
    );
}

fn values() -> Vec<Value> {
    let mut values = vec![
        Constant::Nil,
        Constant::Boolean(false),
        Constant::Boolean(true),
    ];
    values.extend(
        [
            i64::MIN,
            i64::MIN + 1,
            -9_007_199_254_740_993,
            -1,
            0,
            1,
            9_007_199_254_740_991,
            9_007_199_254_740_992,
            9_007_199_254_740_993,
            i64::MAX - 1,
            i64::MAX,
        ]
        .map(Constant::Integer),
    );
    values.extend(
        [
            0,
            1,
            0x000f_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0x3fe0_0000_0000_0000,
            0x3ff0_0000_0000_0000,
            0x4340_0000_0000_0000,
            0x43e0_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_1234,
        ]
        .into_iter()
        .flat_map(|bits| {
            [bits, bits | (1 << 63)].map(|bits| Constant::Number(f64::from_bits(bits)))
        }),
    );
    for bytes in [
        &b""[..],
        b" ",
        b"0",
        b"-0",
        b"-0.0",
        b"+0.0",
        b"1.5",
        b"-2.5",
        b"  \t42\r\n",
        b"0x10",
        b"-0x10",
        b"0x1.8p+2",
        b"0x1p-1074",
        b"9223372036854775807",
        b"-9223372036854775808",
        b"9223372036854775808",
        b"9007199254740993",
        b"1e309",
        b"-1e309",
        b"1e-400",
        b"-1e-400",
        b"nan",
        b"inf",
        b"1\0",
        b"1x",
        b"0x",
        b"1e",
        b"--1",
        b"true",
        b"1 2",
        b"\xff",
        b"\xc2\xa01",
    ] {
        values.push(Constant::String(bytes));
    }
    values
}

#[test]
fn numeric_conversions_preserve_all_representations() {
    for value in values() {
        assert_conversion(value);
    }
    let mut state = 0x2348_690a_bcef_d123u64;
    for _ in 0..16_384 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        assert_conversion(Constant::Integer(state as i64));
        assert_conversion(Constant::Number(f64::from_bits(state)));
    }
}

#[test]
fn numeric_string_conversion_borrows_bytes_once() {
    struct Counted<'a> {
        bytes: &'a [u8],
        reads: &'a Cell<usize>,
    }
    impl AsRef<[u8]> for Counted<'_> {
        fn as_ref(&self) -> &[u8] {
            self.reads.set(self.reads.get() + 1);
            self.bytes
        }
    }
    for value in values() {
        let Constant::String(bytes) = value else {
            continue;
        };
        let reads = Cell::new(0);
        let counted = Constant::String(Counted {
            bytes,
            reads: &reads,
        });
        assert_eq!(counted.as_number(), None);
        assert_eq!(reads.get(), 0);
        let actual = counted.to_number();
        assert_eq!(reads.get(), 1);
        assert_eq!(
            actual.map(f64::to_bits),
            normalized_number(&value).map(f64::to_bits)
        );
    }
}

fn assert_arithmetic(actual: Option<Value>, expected: Option<Value>) {
    match (actual, expected) {
        (Some(Constant::Number(a)), Some(Constant::Number(b))) => {
            assert!(
                a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()),
                "{a:?} != {b:?}"
            );
        }
        (Some(Constant::Integer(a)), Some(Constant::Integer(b))) => assert_eq!(a, b),
        (None, None) => {}
        values => panic!("different arithmetic representations: {values:?}"),
    }
}

type IntegerOperation = fn(i64, i64) -> Option<i64>;
type FloatOperation = fn(f64, f64) -> f64;
type Operation = fn(&Value, &Value) -> Option<Value>;

fn normalized_arithmetic(
    a: Value,
    b: Value,
    integer: IntegerOperation,
    float: FloatOperation,
) -> Option<Value> {
    match (a.to_numeric()?, b.to_numeric()?) {
        (Constant::Integer(a), Constant::Integer(b)) => integer(a, b).map(Constant::Integer),
        (a, b) => Some(Constant::Number(float(
            normalized_number(&a)?,
            normalized_number(&b)?,
        ))),
    }
}

#[test]
fn arithmetic_keeps_normalization_tags_and_float_results() {
    let operations: [(Operation, IntegerOperation, FloatOperation); 5] = [
        (Value::add, |a, b| Some(a.wrapping_add(b)), |a, b| a + b),
        (
            Value::subtract,
            |a, b| Some(a.wrapping_sub(b)),
            |a, b| a - b,
        ),
        (
            Value::multiply,
            |a, b| Some(a.wrapping_mul(b)),
            |a, b| a * b,
        ),
        (Value::floor_divide, integer_floor_divide, |a, b| {
            (a / b).floor()
        }),
        (Value::modulo, integer_modulo, float_modulo),
    ];
    let values = values();
    for &a in &values {
        for &b in &values {
            for (actual, integer, float) in operations {
                assert_arithmetic(actual(&a, &b), normalized_arithmetic(a, b, integer, float));
            }
            assert_arithmetic(
                a.float_divide(&b),
                normalized_number(&a)
                    .zip(normalized_number(&b))
                    .map(|(a, b)| Constant::Number(a / b)),
            );
            assert_arithmetic(
                a.exponentiate(&b),
                normalized_number(&a)
                    .zip(normalized_number(&b))
                    .map(|(a, b)| Constant::Number(a.powf(b))),
            );
        }
    }
}
