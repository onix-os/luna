use crate::{meta_ops, Constant, Context, String, Value};

#[inline(always)]
pub(super) fn add<'gc>(
    ctx: Context<'gc>,
    left: Value<'gc>,
    right: Constant<String<'gc>>,
) -> Result<meta_ops::MetaResult<'gc, 2>, meta_ops::MetaOperatorError> {
    let value = match (left, right) {
        (Value::Integer(a), Constant::Integer(b)) => Value::Integer(a.wrapping_add(b)),
        (Value::Number(a), Constant::Number(b)) => Value::Number(a + b),
        (Value::Integer(a), Constant::Number(b)) => Value::Number(a as f64 + b),
        (Value::Number(a), Constant::Integer(b)) => Value::Number(a + b as f64),
        _ => return meta_ops::add(ctx, left, right.into()),
    };
    Ok(value.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Callback, Function, Table};

    fn same_value<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
        match (actual, expected) {
            (Value::Nil, Value::Nil) => {}
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Number(a), Value::Number(b)) => {
                assert!(a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()));
            }
            (Value::String(a), Value::String(b)) => {
                assert!(ottavino_gc_arena::Gc::ptr_eq(
                    a.into_inner(),
                    b.into_inner()
                ));
            }
            (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
            pair => panic!("different results: {pair:?}"),
        }
    }

    #[test]
    fn addition_matches_canonical_values_errors_and_calls() {
        crate::Lua::empty().enter(|ctx| {
            let mut constants = vec![
                Constant::Nil,
                Constant::Boolean(false),
                Constant::Boolean(true),
            ];
            for n in [
                i64::MIN,
                i64::MAX,
                -9_007_199_254_740_993,
                9_007_199_254_740_993,
                -1,
                0,
                1,
            ] {
                constants.extend([Constant::Integer(n), Constant::Number(n as f64)]);
            }
            for bits in [
                0,
                1,
                0x000f_ffff_ffff_ffff,
                0x0010_0000_0000_0000,
                0x3fe0_0000_0000_0000,
                0x7fef_ffff_ffff_ffff,
                0x7ff0_0000_0000_0000,
                0x7ff0_0000_0000_0001,
                0x7ff8_0000_0000_1234,
            ] {
                constants
                    .extend([bits, bits | (1 << 63)].map(|b| Constant::Number(f64::from_bits(b))));
            }
            for text in [
                &b""[..],
                b" \t42\n",
                b"-0.0",
                b"0x1.8p+2",
                b"1e400",
                b"1e-400",
                b"9223372036854775808",
                b"9007199254740993",
                b"nan",
                b"inf",
                b"1x",
                b"\xff",
            ] {
                constants.push(Constant::String(ctx.intern(text)));
            }
            let object = Table::new(&ctx);
            let mt = Table::new(&ctx);
            let callback =
                Callback::from_fn(&ctx, |_, _, _| panic!("model must not execute calls"));
            mt.set(ctx, "__add", Function::Callback(callback)).unwrap();
            object.set_metatable(ctx, Some(mt));
            let mut left: Vec<Value<'_>> = constants.iter().copied().map(Value::from).collect();
            left.extend([Value::Table(Table::new(&ctx)), Value::Table(object)]);
            for a in left {
                for &b in &constants {
                    match (add(ctx, a, b), meta_ops::add(ctx, a, b.into())) {
                        (
                            Ok(meta_ops::MetaResult::Value(a)),
                            Ok(meta_ops::MetaResult::Value(b)),
                        ) => same_value(a, b),
                        (Ok(meta_ops::MetaResult::Call(a)), Ok(meta_ops::MetaResult::Call(b))) => {
                            assert_eq!(a.function, b.function);
                            for (a, b) in a.args.into_iter().zip(b.args) {
                                same_value(a, b);
                            }
                        }
                        (Err(a), Err(b)) => assert_eq!(format!("{a:?}"), format!("{b:?}")),
                        pair => panic!("different addition for {a:?}, {b:?}: {pair:?}"),
                    }
                }
            }
        });
    }
}
