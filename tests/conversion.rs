use luna::{FromMultiValue, FromValue, IntoMultiValue, IntoValue, Table, Value};

#[test]
fn test_conversions() {
    let mut lua = common::core();
    lua.enter(|ctx| {
        let v = (1, true, "hello").into_multi_value(ctx).collect::<Vec<_>>();
        assert!(matches!(
            v.as_slice(),
            [
                Value::Integer(1),
                Value::Boolean(true),
                Value::String(s)
            ] if s == b"hello"
        ));

        let vals = Table::from_value(
            ctx,
            [
                1.into_value(ctx),
                true.into_value(ctx),
                "hello".into_value(ctx),
            ]
            .into_value(ctx),
        )
        .unwrap();

        assert!(matches!(vals.get_value(ctx, 1), Value::Integer(1)));
        assert!(matches!(vals.get_value(ctx, 2), Value::Boolean(true)));
        assert!(matches!(vals.get_value(ctx, 3), Value::String(s) if s == b"hello"));

        let array = <[Value; 3]>::from_value(ctx, vals.into()).unwrap();
        assert!(matches!(
            array.as_slice(),
            [
                Value::Integer(1),
                Value::Boolean(true),
                Value::String(s)
            ] if s == b"hello"
        ));

        let vec = Vec::<Value>::from_value(ctx, vals.into()).unwrap();
        assert!(matches!(
            vec.as_slice(),
            [
                Value::Integer(1),
                Value::Boolean(true),
                Value::String(s)
            ] if s == b"hello"
        ));

        let (a, b, c) = <(i32, bool, String)>::from_multi_value(
            ctx,
            (2, false, "goodbye").into_multi_value(ctx),
        )
        .unwrap();
        assert_eq!((a, b, c), (2, false, "goodbye".to_owned()));
    });
}

#[test]
fn test_result_conversion() {
    let mut lua = common::core();
    lua.enter(|ctx| {
        let a = Ok::<i32, i32>(4).into_multi_value(ctx).collect::<Vec<_>>();
        assert!(matches!(
            a.as_slice(),
            [Value::Boolean(true), Value::Integer(4)]
        ));
        let b = Err::<i32, i32>(7).into_multi_value(ctx).collect::<Vec<_>>();
        assert!(matches!(
            b.as_slice(),
            [Value::Boolean(false), Value::Integer(7)]
        ));
        let c = Ok::<_, i32>((1, 2, 3, 4))
            .into_multi_value(ctx)
            .collect::<Vec<_>>();
        assert!(matches!(
            c.as_slice(),
            [
                Value::Boolean(true),
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
                Value::Integer(4)
            ]
        ));
    });
}

#[test]
fn integer_conversions_preserve_ranges_and_coercion_errors() {
    let mut lua = common::core();
    lua.enter(|ctx| {
        let mut values = vec![
            Value::Nil,
            Value::Boolean(false),
            Value::Table(Table::new(&ctx)),
        ];
        for integer in [
            i64::MIN,
            i64::MIN + 1,
            i32::MIN as i64 - 1,
            i32::MIN as i64,
            -32769,
            -32768,
            -129,
            -128,
            -1,
            0,
            1,
            127,
            128,
            255,
            256,
            32767,
            32768,
            65535,
            65536,
            i32::MAX as i64,
            i32::MAX as i64 + 1,
            u32::MAX as i64,
            u32::MAX as i64 + 1,
            i64::MAX - 1,
            i64::MAX,
        ] {
            values.push(Value::Integer(integer));
            values.push(Value::Number(integer as f64));
        }
        for number in [
            -0.0,
            0.5,
            -0.5,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::from_bits(0x7ff0_0000_0000_0001),
            -9_223_372_036_854_775_808.0,
            9_223_372_036_854_775_808.0,
        ] {
            values.push(Value::Number(number));
        }
        for bytes in [
            &b""[..],
            b"garbage",
            b"1\0",
            b"  -42  ",
            b"0x7f",
            b"0x1p2",
            b"1.5",
            b"1e2",
            b"1e999",
            b"9223372036854775807",
            b"-9223372036854775808",
            b"9223372036854775808",
        ] {
            values.push(Value::String(ctx.intern(bytes)));
        }
        macro_rules! check {
            ($($ty:ty),* $(,)?) => {$({
                for &value in &values {
                    let expected = match value.to_integer() {
                        Some(integer) => <$ty>::try_from(integer)
                            .map_err(|_| ("number", "an integer out of range")),
                        None => Err(("number", if value.to_number().is_some() {
                            "a number with no integer representation"
                        } else {
                            value.type_name()
                        })),
                    };
                    let actual = <$ty>::from_value(ctx, value)
                        .map_err(|error| (error.expected, error.found));
                    assert_eq!(actual, expected, "{}: {value:?}", stringify!($ty));
                }
            })*};
        }
        check!(i64, u64, i32, u32, i16, u16, i8, u8, usize, isize, i128, u128);
        assert_eq!(
            i64::from_value(ctx, Value::Integer(i64::MAX)).unwrap(),
            i64::MAX
        );
        assert_eq!(
            i64::from_value(ctx, Value::Integer(i64::MIN)).unwrap(),
            i64::MIN
        );
        assert!(u64::from_value(ctx, Value::Integer(-1)).is_err());
        assert!(i64::from_value(ctx, Value::Number(9_223_372_036_854_775_808.0)).is_err());
    });
}

mod common;
