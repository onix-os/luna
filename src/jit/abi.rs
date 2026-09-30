use crate::{Constant, Value};

pub(super) const NIL: u64 = 0;
pub(super) const BOOLEAN: u64 = 1;
pub(super) const INTEGER: u64 = 2;
pub(super) const NUMBER: u64 = 3;
pub(super) const REFERENCE: u64 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct Slot {
    pub tag: u64,
    pub bits: u64,
}

impl Slot {
    pub fn from_value(value: Value<'_>) -> Self {
        match value {
            Value::Nil => Self { tag: NIL, bits: 0 },
            Value::Boolean(value) => Self {
                tag: BOOLEAN,
                bits: u64::from(value),
            },
            Value::Integer(value) => Self {
                tag: INTEGER,
                bits: value as u64,
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

    pub fn from_constant(value: Constant<crate::String<'_>>) -> Self {
        Self::from_value(value.into())
    }

    pub fn write_back(self, dest: &mut Value<'_>) {
        *dest = self.value(*dest);
    }

    pub fn value<'gc>(self, reference: Value<'gc>) -> Value<'gc> {
        match self.tag {
            NIL => Value::Nil,
            BOOLEAN => Value::Boolean(self.bits != 0),
            INTEGER => Value::Integer(self.bits as i64),
            NUMBER => Value::Number(f64::from_bits(self.bits)),
            REFERENCE => reference,
            _ => unreachable!("invalid native scalar tag"),
        }
    }
}

#[repr(C)]
#[derive(Default, Debug)]
pub(super) struct Exit {
    pub pc: u64,
    pub instructions: u32,
    pub reason: u32,
}

pub(super) const HELPER_MOVE: u32 = 1;
pub(super) const HELPER_CONSTANT: u32 = 2;
pub(super) const HELPER_NEW_TABLE: u32 = 3;
pub(super) const HELPER_GET_TABLE: u32 = 4;
pub(super) const HELPER_SET_TABLE: u32 = 5;
pub(super) const HELPER_GET_UP_TABLE: u32 = 6;
pub(super) const HELPER_SET_UP_TABLE: u32 = 7;
pub(super) const HELPER_GET_UPVALUE: u32 = 8;
pub(super) const HELPER_SET_UPVALUE: u32 = 9;

pub(super) const HELPER_DECLINED: u32 = 0;
pub(super) const HELPER_COMPLETED: u32 = 1;
pub(super) const HELPER_PANICKED: u32 = 2;
pub(super) const CONSTANT_OPERAND: u32 = 1 << 16;

pub(super) type HelperEntry = unsafe extern "C" fn(*mut Host, *mut Slot, u32, u32, u32, u32) -> u32;

#[repr(C)]
pub(super) struct Host {
    pub data: *mut std::ffi::c_void,
}

pub(super) type Entry = unsafe extern "C" fn(*mut Slot, u64, u32, *mut Exit, *mut Host);

const _: () = assert!(std::mem::size_of::<Slot>() == 16);
const _: () = assert!(std::mem::offset_of!(Slot, bits) == 8);
const _: () = assert!(std::mem::size_of::<Exit>() == 16);
const _: () = assert!(std::mem::offset_of!(Exit, instructions) == 8);
const _: () = assert!(std::mem::offset_of!(Exit, reason) == 12);
