use std::cmp::Ordering;

use crate::{constant::cmp_int_float, Value};

#[inline(always)]
pub(super) fn compare<const LE: bool>(left: Value<'_>, right: Value<'_>) -> Option<bool> {
    Some(match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => {
            if LE {
                a <= b
            } else {
                a < b
            }
        }
        (Value::Number(a), Value::Number(b)) => {
            if LE {
                a <= b
            } else {
                a < b
            }
        }
        (Value::Integer(a), Value::Number(b)) => cmp_int_float(a, b).is_some_and(|o| {
            if LE {
                o != Ordering::Greater
            } else {
                o == Ordering::Less
            }
        }),
        (Value::Number(a), Value::Integer(b)) => cmp_int_float(b, a).is_some_and(|o| {
            if LE {
                o != Ordering::Less
            } else {
                o == Ordering::Greater
            }
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
