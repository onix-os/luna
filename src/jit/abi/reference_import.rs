use super::*;

impl Slot {
    pub(super) fn from_ref(value: &Value<'_>) -> Self {
        match value {
            Value::Nil => Self { tag: NIL, bits: 0 },
            Value::Boolean(value) => Self {
                tag: BOOLEAN,
                bits: u64::from(*value),
            },
            Value::Integer(value) => Self {
                tag: INTEGER,
                bits: *value as u64,
            },
            Value::Number(value) => Self {
                tag: NUMBER,
                bits: value.to_bits(),
            },
            _ => Self {
                tag: REFERENCE,
                bits: 0,
            },
        }
    }

    #[inline(always)]
    pub(super) fn from_ref_numeric(value: &Value<'_>) -> Self {
        if let Value::Integer(value) = value {
            return Self {
                tag: INTEGER,
                bits: *value as u64,
            };
        }
        if let Value::Number(value) = value {
            return Self {
                tag: NUMBER,
                bits: value.to_bits(),
            };
        }
        Self::from_ref_non_numeric(value)
    }

    #[cold]
    #[inline(never)]
    fn from_ref_non_numeric(value: &Value<'_>) -> Self {
        match value {
            Value::Nil => Self { tag: NIL, bits: 0 },
            Value::Boolean(value) => Self {
                tag: BOOLEAN,
                bits: u64::from(*value),
            },
            _ => Self {
                tag: REFERENCE,
                bits: 0,
            },
        }
    }
}
