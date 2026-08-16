use std::hash::{Hash, Hasher};

use ottavino_gc_arena::Collect;

use crate::compiler::string_utils::{read_float, read_integer, trim_whitespace};

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum Constant<S> {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(S),
}

/// Compare an integer with a float exactly.
///
/// `i as f64` loses precision above 2^53, which made `math.maxinteger == math.maxinteger + 0.0`
/// answer `true` and silently corrupted sorts and range checks on large integer ids. Comparing
/// through the float's own integral part keeps every bit.
pub(crate) fn cmp_int_float(i: i64, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if f.is_nan() {
        return None;
    }
    // Anything past the i64 range settles it without further arithmetic.
    if f >= 9_223_372_036_854_775_808.0 {
        return Some(Ordering::Less);
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Some(Ordering::Greater);
    }
    // `trunc` is exactly representable, so this comparison is lossless; the fraction breaks ties.
    let truncated = f.trunc() as i64;
    Some(i.cmp(&truncated).then(if f > f.trunc() {
        Ordering::Less
    } else {
        Ordering::Equal
    }))
}

/// Lua's `//` on integers: division rounding towards negative infinity, `None` on a zero divisor.
///
/// `i64::MIN / -1` and `i64::MIN % -1` both trap in Rust, so every step wraps the way PUC-Rio's
/// `luaV_idiv` does.
fn integer_floor_divide(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    let d = a.wrapping_div(b);
    let r = a.wrapping_rem(b);
    Some(if r != 0 && (r ^ b) < 0 {
        d.wrapping_sub(1)
    } else {
        d
    })
}

/// Lua's `%` on integers, which takes the sign of the divisor rather than the dividend.
fn integer_modulo(a: i64, b: i64) -> Option<i64> {
    match b {
        0 => None,
        // `i64::MIN % -1` overflows the hardware instruction; the answer is always zero anyway.
        -1 => Some(0),
        _ => {
            let r = a % b;
            Some(if r != 0 && (r ^ b) < 0 { r + b } else { r })
        }
    }
}

/// Lua's `%` on floats, transcribed from `luai_nummod` in llimits.h.
///
/// Correcting by `(m + b) % b` instead turns an infinite divisor into NaN.
fn float_modulo(a: f64, b: f64) -> f64 {
    let mut m = a % b;
    let wrong_side = if m > 0.0 { b < 0.0 } else { m < 0.0 && b > 0.0 };
    if wrong_side {
        m += b;
    }
    m
}

impl<S> Constant<S> {
    pub fn to_bool(&self) -> bool {
        match self {
            Self::Nil => false,
            Self::Boolean(false) => false,
            _ => true,
        }
    }

    pub fn not(&self) -> Constant<S> {
        Constant::Boolean(!self.to_bool())
    }

    pub fn as_string_ref(&self) -> Constant<&S> {
        match self {
            Constant::Nil => Constant::Nil,
            Constant::Boolean(b) => Constant::Boolean(*b),
            Constant::Integer(i) => Constant::Integer(*i),
            Constant::Number(n) => Constant::Number(*n),
            Constant::String(s) => Constant::String(s),
        }
    }

    pub fn map_string<S2>(self, f: impl FnOnce(S) -> S2) -> Constant<S2> {
        match self {
            Constant::Nil => Constant::Nil,
            Constant::Boolean(b) => Constant::Boolean(b),
            Constant::Integer(i) => Constant::Integer(i),
            Constant::Number(n) => Constant::Number(n),
            Constant::String(s) => Constant::String(f(s)),
        }
    }
}

impl<S: AsRef<[u8]>> Constant<S> {
    /// Converts the given constant to an integer or number, if possible.
    pub fn to_numeric(&self) -> Option<Constant<S>> {
        match self {
            &Self::Integer(a) => Some(Constant::Integer(a)),
            &Self::Number(a) => Some(Constant::Number(a)),
            Self::String(a) => {
                let a = trim_whitespace(a.as_ref());
                if let Some(i) = read_integer(a) {
                    Some(Constant::Integer(i))
                } else if let Some(n) = read_float(a) {
                    Some(Constant::Number(n))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Interprets Numbers, Integers, and Strings as a Number, if possible.
    pub fn to_number(&self) -> Option<f64> {
        match self.to_numeric() {
            Some(Self::Integer(a)) => Some(a as f64),
            Some(Self::Number(a)) => Some(a),
            _ => None,
        }
    }

    /// Interprets Numbers, Integers, and Strings as an Integer, if possible.
    pub fn to_integer(&self) -> Option<i64> {
        match self.to_numeric() {
            Some(Self::Integer(a)) => Some(a),
            Some(Self::Number(a)) => {
                // `a as i64` saturates, so `2^63` used to answer `maxinteger` even though it is
                // one past the top of the range. The upper bound is exclusive for that reason.
                if a >= -9_223_372_036_854_775_808.0
                    && a < 9_223_372_036_854_775_808.0
                    && a.floor() == a
                {
                    Some(a as i64)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // Mathematical operators

    pub fn add(&self, rhs: &Self) -> Option<Self> {
        Some(match (self, rhs) {
            (&Self::Integer(a), &Self::Integer(b)) => Self::Integer(a.wrapping_add(b)),
            (a, b) => match (a.to_numeric()?, b.to_numeric()?) {
                (Self::Integer(x), Self::Integer(y)) => Self::Integer(x.wrapping_add(y)),
                (x, y) => Self::Number(x.to_number()? + y.to_number()?),
            },
        })
    }

    pub fn subtract(&self, rhs: &Self) -> Option<Self> {
        Some(match (self, rhs) {
            (&Self::Integer(a), &Self::Integer(b)) => Self::Integer(a.wrapping_sub(b)),
            (a, b) => match (a.to_numeric()?, b.to_numeric()?) {
                (Self::Integer(x), Self::Integer(y)) => Self::Integer(x.wrapping_sub(y)),
                (x, y) => Self::Number(x.to_number()? - y.to_number()?),
            },
        })
    }

    pub fn multiply(&self, rhs: &Self) -> Option<Self> {
        Some(match (self, rhs) {
            (&Self::Integer(a), &Self::Integer(b)) => Self::Integer(a.wrapping_mul(b)),
            (a, b) => match (a.to_numeric()?, b.to_numeric()?) {
                (Self::Integer(x), Self::Integer(y)) => Self::Integer(x.wrapping_mul(y)),
                (x, y) => Self::Number(x.to_number()? * y.to_number()?),
            },
        })
    }

    /// This operation always returns a Number, even when called with Integer arguments.
    pub fn float_divide(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Number(self.to_number()? / rhs.to_number()?))
    }

    /// This operation returns an Integer only if both arguments are Integers. Rounding is towards
    /// negative infinity.
    pub fn floor_divide(&self, rhs: &Self) -> Option<Self> {
        match (self, rhs) {
            (&Self::Integer(a), &Self::Integer(b)) => integer_floor_divide(a, b).map(Self::Integer),
            (a, b) => match (a.to_numeric()?, b.to_numeric()?) {
                (Self::Integer(x), Self::Integer(y)) => {
                    integer_floor_divide(x, y).map(Self::Integer)
                }
                (x, y) => Some(Self::Number((x.to_number()? / y.to_number()?).floor())),
            },
        }
    }

    /// Computes the Lua modulus (`%`) operator. This is unlike Rust's `%` operator which computes
    /// the remainder.
    pub fn modulo(&self, rhs: &Self) -> Option<Self> {
        match (self, rhs) {
            (&Self::Integer(a), &Self::Integer(b)) => integer_modulo(a, b).map(Self::Integer),
            (a, b) => match (a.to_numeric()?, b.to_numeric()?) {
                (Self::Integer(x), Self::Integer(y)) => integer_modulo(x, y).map(Self::Integer),
                (x, y) => Some(Self::Number(float_modulo(x.to_number()?, y.to_number()?))),
            },
        }
    }

    /// This operation always returns a Number, even when called with Integer arguments.
    pub fn exponentiate(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Number(self.to_number()?.powf(rhs.to_number()?)))
    }

    pub fn negate(&self) -> Option<Self> {
        match self.to_numeric()? {
            Self::Integer(a) => Some(Self::Integer(a.wrapping_neg())),
            Self::Number(a) => Some(Self::Number(-a)),
            _ => None,
        }
    }

    // Bitwise operators

    /// An integer operand for a bitwise operator.
    ///
    /// Unlike `to_integer`, a string is refused. PUC-Rio raises for `"10" | 1`, and being more
    /// permissive than that means code written against luna breaks elsewhere and a class of typo
    /// goes undetected.
    fn to_integer_bitwise(&self) -> Option<i64> {
        match self {
            Self::Integer(i) => Some(*i),
            Self::Number(_) => self.to_integer(),
            _ => None,
        }
    }

    pub fn bitwise_not(&self) -> Option<Self> {
        Some(Self::Integer(!self.to_integer_bitwise()?))
    }

    pub fn bitwise_and(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(
            self.to_integer_bitwise()? & rhs.to_integer_bitwise()?,
        ))
    }

    pub fn bitwise_or(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(
            self.to_integer_bitwise()? | rhs.to_integer_bitwise()?,
        ))
    }

    pub fn bitwise_xor(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(
            self.to_integer_bitwise()? ^ rhs.to_integer_bitwise()?,
        ))
    }

    pub fn shift_left(&self, rhs: &Self) -> Option<Self> {
        let rhs = rhs.to_integer_bitwise()?;
        if rhs < 0 {
            return None;
        }
        let rhs = rhs.try_into().ok().unwrap_or(u32::MAX);
        Some(Self::Integer(
            self.to_integer_bitwise()?.checked_shl(rhs).unwrap_or(0),
        ))
    }

    pub fn shift_right(&self, rhs: &Self) -> Option<Self> {
        let rhs = rhs.to_integer_bitwise()?;
        if rhs < 0 {
            return None;
        }
        let lhs = self.to_integer_bitwise()? as u64;
        let rhs = rhs.try_into().ok().unwrap_or(u32::MAX);
        Some(Self::Integer(lhs.checked_shr(rhs).unwrap_or(0) as i64))
    }

    // Comparison operators

    pub fn is_equal(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Nil, Self::Nil) => true,
            (Self::Nil, _) => false,

            (Self::Boolean(a), Self::Boolean(b)) => a == b,
            (Self::Boolean(_), _) => false,

            (Self::Integer(a), Self::Integer(b)) => a == b,
            (Self::Integer(a), Self::Number(b)) => {
                cmp_int_float(*a, *b) == Some(std::cmp::Ordering::Equal)
            }
            (Self::Integer(_), _) => false,

            (Self::Number(a), Self::Number(b)) => a == b,
            (Self::Number(a), Self::Integer(b)) => {
                cmp_int_float(*b, *a) == Some(std::cmp::Ordering::Equal)
            }
            (Self::Number(_), _) => false,

            (Self::String(a), Self::String(b)) => a.as_ref() == b.as_ref(),
            (Self::String(_), _) => false,
        }
    }

    pub fn less_than(&self, rhs: &Self) -> Option<bool> {
        Some(match (self, rhs) {
            (Self::Integer(a), Self::Integer(b)) => a < b,
            (Self::Integer(a), Self::Number(b)) => {
                cmp_int_float(*a, *b).is_some_and(|o| o == std::cmp::Ordering::Less)
            }
            (Self::Number(a), Self::Number(b)) => a < b,
            (Self::Number(a), Self::Integer(b)) => {
                cmp_int_float(*b, *a).is_some_and(|o| o == std::cmp::Ordering::Greater)
            }
            (Self::String(a), Self::String(b)) => a.as_ref() < b.as_ref(),
            _ => return None,
        })
    }

    pub fn less_equal(&self, rhs: &Self) -> Option<bool> {
        Some(match (self, rhs) {
            (Self::Integer(a), Self::Integer(b)) => a <= b,
            (Self::Integer(a), Self::Number(b)) => {
                cmp_int_float(*a, *b).is_some_and(|o| o != std::cmp::Ordering::Greater)
            }
            (Self::Number(a), Self::Number(b)) => a <= b,
            (Self::Number(a), Self::Integer(b)) => {
                cmp_int_float(*b, *a).is_some_and(|o| o != std::cmp::Ordering::Less)
            }
            (Self::String(a), Self::String(b)) => a.as_ref() <= b.as_ref(),
            _ => return None,
        })
    }
}

impl<S: AsRef<[u8]>> PartialEq for Constant<S> {
    fn eq(&self, other: &Self) -> bool {
        self.is_equal(other)
    }
}

/// Wrapper for a `Constant` that implements Hash and Eq, and only compares equal when the types are
/// bit for bit identical.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct IdenticalConstant<S>(pub Constant<S>);

impl<S> From<Constant<S>> for IdenticalConstant<S> {
    fn from(value: Constant<S>) -> Self {
        Self(value)
    }
}

impl<S: AsRef<[u8]>> PartialEq for IdenticalConstant<S> {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Constant::Nil, Constant::Nil) => true,
            (Constant::Nil, _) => false,

            (Constant::Boolean(a), Constant::Boolean(b)) => a == b,
            (Constant::Boolean(_), _) => false,

            (Constant::Integer(a), Constant::Integer(b)) => a == b,
            (Constant::Integer(_), _) => false,

            (Constant::Number(a), Constant::Number(b)) => a.to_bits() == b.to_bits(),
            (Constant::Number(_), _) => false,

            (Constant::String(a), Constant::String(b)) => a.as_ref() == b.as_ref(),
            (Constant::String(_), _) => false,
        }
    }
}

impl<S: AsRef<[u8]>> Eq for IdenticalConstant<S> {}

impl<S: AsRef<[u8]>> Hash for IdenticalConstant<S> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match &self.0 {
            Constant::Nil => {
                Hash::hash(&0, state);
            }
            Constant::Boolean(b) => {
                Hash::hash(&1, state);
                b.hash(state);
            }
            Constant::Integer(i) => {
                Hash::hash(&2, state);
                i.hash(state);
            }
            Constant::Number(n) => {
                Hash::hash(&3, state);
                n.to_bits().hash(state);
            }
            Constant::String(s) => {
                Hash::hash(&4, state);
                s.as_ref().hash(state);
            }
        }
    }
}
