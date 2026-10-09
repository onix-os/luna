use crate::{Constant, String, Value};

#[inline(always)]
pub(super) fn value<'gc>(constant: Constant<String<'gc>>) -> Value<'gc> {
    match constant {
        Constant::Integer(value) => Value::Integer(value),
        Constant::Number(value) => Value::Number(value),
        other => non_numeric(other),
    }
}

#[inline(never)]
fn non_numeric<'gc>(constant: Constant<String<'gc>>) -> Value<'gc> {
    constant.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_materialization_preserves_bits_and_string_identity() {
        crate::Lua::empty().enter(|ctx| {
            let string = String::from_slice(&ctx, b"numeric-constant\0identity");
            let mut constants = vec![
                Constant::Nil,
                Constant::Boolean(false),
                Constant::Boolean(true),
                Constant::String(string),
            ];
            constants.extend([i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX].map(Constant::Integer));
            constants.extend(
                [
                    0,
                    1,
                    0x8000000000000000,
                    0x3fe0000000000000,
                    0x7ff0000000000000,
                    0xfff0000000000000,
                    0x7ff8000000000055,
                    0xfff8000000000055,
                ]
                .map(|bits| Constant::Number(f64::from_bits(bits))),
            );
            for constant in constants {
                let actual = value(constant);
                let expected: Value = constant.into();
                match (actual, expected) {
                    (Value::Nil, Value::Nil) => {}
                    (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
                    (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
                    (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
                    (Value::String(a), Value::String(b)) => assert_eq!(a, b),
                    _ => panic!("constant changed kind"),
                }
            }
        });
    }
}
