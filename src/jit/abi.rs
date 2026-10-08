use crate::{Constant, Value};

pub(super) mod roots;

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
pub(super) const HELPER_SET_LIST: u32 = 10;

pub(super) const HELPER_DECLINED: u32 = 0;
pub(super) const HELPER_COMPLETED: u32 = 1;
pub(super) const HELPER_PANICKED: u32 = 2;
pub(super) const CONSTANT_OPERAND: u32 = 1 << 16;

pub(super) type HelperEntry = unsafe extern "C" fn(*mut Host, *mut Slot, u32, u32, u32, u32) -> u32;

#[repr(C)]
pub(super) struct Host {
    pub data: *mut std::ffi::c_void,
    pub projection: *mut std::ffi::c_void,
}

pub(super) type Entry = unsafe extern "C" fn(*mut Slot, u64, u32, *mut Exit, *mut Host);

/// Invokes an entry with the original scratch pointer and a bounded instruction budget.
///
/// # Safety
/// `slots` covers the entry's initialized register prefix; host data and code remain live
/// and exclusively accessible for the call. Entry retains no pointers.
pub(super) unsafe fn invoke(
    entry: Entry,
    slots: *mut Slot,
    pc: usize,
    budget: u32,
    host: *mut Host,
) -> Exit {
    let mut exit = Exit::default();
    unsafe { entry(slots, pc as u64, budget.min(64), &mut exit, host) };
    exit
}

const _: () = assert!(std::mem::size_of::<Slot>() == 16);
const _: () = assert!(std::mem::offset_of!(Slot, bits) == 8);
const _: () = assert!(std::mem::size_of::<Exit>() == 16);
const _: () = assert!(std::mem::offset_of!(Exit, instructions) == 8);
const _: () = assert!(std::mem::offset_of!(Exit, reason) == 12);
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<Host>() == 16);
    assert!(std::mem::offset_of!(Host, projection) == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_scratch_entry_preserves_pointer_pc_and_budget_domains() {
        unsafe extern "C" fn entry(
            slots: *mut Slot,
            pc: u64,
            budget: u32,
            exit: *mut Exit,
            host: *mut Host,
        ) {
            unsafe {
                (*slots).bits = pc;
                (*exit).pc = pc;
                (*exit).instructions = budget;
                (*exit).reason = u32::from(host.is_null());
            }
        }
        let mut slots = [Slot {
            tag: INTEGER,
            bits: 0,
        }];
        let pointer = slots.as_mut_ptr();
        for budget in [0, 1, 63, 64, 65, u32::MAX] {
            let exit = unsafe { invoke(entry, pointer, 255, budget, std::ptr::null_mut()) };
            assert_eq!(exit.pc, 255);
            assert_eq!(exit.instructions, budget.min(64));
            assert_eq!(exit.reason, 1);
            assert_eq!(unsafe { pointer.read().bits }, 255);
        }
    }

    #[test]
    fn call_transitions_preserve_register_and_count_domains() {
        use crate::{
            opcode::{CallTransition, OpCode, Operation},
            types::{RegisterIndex, VarCount},
        };
        for register in 0..=u8::MAX {
            for count in 0..=u8::MAX {
                let func = RegisterIndex(register);
                let args = VarCount::try_constant(count).unwrap_or(VarCount::variable());
                let returns =
                    VarCount::try_constant(u8::MAX - count).unwrap_or(VarCount::variable());
                let cases = [
                    (
                        Operation::Call {
                            func,
                            args,
                            returns,
                        },
                        CallTransition::Call {
                            func,
                            args,
                            returns,
                        },
                    ),
                    (
                        Operation::TailCall { func, args },
                        CallTransition::TailCall { func, args },
                    ),
                    (
                        Operation::Return {
                            start: func,
                            count: args,
                        },
                        CallTransition::Return {
                            start: func,
                            count: args,
                        },
                    ),
                ];
                for (operation, expected) in cases {
                    assert_eq!(OpCode::encode(operation).call_transition(), Some(expected));
                }
                assert!(OpCode::encode(Operation::VarArgs {
                    dest: func,
                    count: args
                })
                .call_transition()
                .is_none());
                assert!(OpCode::encode(Operation::Move {
                    dest: func,
                    source: RegisterIndex(count)
                })
                .call_transition()
                .is_none());
            }
        }
    }

    fn assert_identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
        match (actual, expected) {
            (Value::Nil, Value::Nil) => {}
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (Value::String(a), Value::String(b)) => {
                assert!(ottavino_gc_arena::Gc::ptr_eq(
                    a.into_inner(),
                    b.into_inner()
                ));
            }
            (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
            (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
            (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
            (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
            (actual, expected) => panic!("value mismatch: {actual:?} != {expected:?}"),
        }
    }

    #[test]
    fn scalar_materialization_matches_operand_decoding_bit_for_bit() {
        let slots = [
            Slot { tag: NIL, bits: 0 },
            Slot {
                tag: BOOLEAN,
                bits: 0,
            },
            Slot {
                tag: BOOLEAN,
                bits: 9,
            },
            Slot {
                tag: INTEGER,
                bits: i64::MIN as u64,
            },
            Slot {
                tag: INTEGER,
                bits: i64::MAX as u64,
            },
            Slot {
                tag: NUMBER,
                bits: (-0.0f64).to_bits(),
            },
            Slot {
                tag: NUMBER,
                bits: f64::INFINITY.to_bits(),
            },
            Slot {
                tag: NUMBER,
                bits: 0x7ff8_0000_0000_1234,
            },
        ];
        for slot in slots {
            let mut dest = Value::Integer(42);
            let expected = slot.value(dest);
            slot.write_back(&mut dest);
            assert_identical(dest, expected);
        }
    }

    #[test]
    fn scalar_conversions_preserve_payloads_across_destination_types() {
        let integers = [i64::MIN, -1, 0, 1, i64::MAX];
        let number_bits = [
            0,
            (-0.0f64).to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            1,
            u64::MAX,
            0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_1234,
        ];
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut values = vec![
                (Value::Nil, NIL, 0),
                (Value::Boolean(false), BOOLEAN, 0),
                (Value::Boolean(true), BOOLEAN, 1),
            ];
            values.extend(
                integers
                    .into_iter()
                    .map(|value| (Value::Integer(value), INTEGER, value as u64)),
            );
            values.extend(
                number_bits
                    .into_iter()
                    .map(|bits| (Value::Number(f64::from_bits(bits)), NUMBER, bits)),
            );
            let destinations = [
                Value::Nil,
                Value::Boolean(true),
                Value::Integer(-42),
                Value::Number(-0.0),
                Value::String(crate::String::from_slice(&ctx, b"replace")),
                Value::Table(crate::Table::new(&ctx)),
                Value::Function(
                    crate::Closure::load(ctx, None, b"return 42")
                        .unwrap()
                        .into(),
                ),
                Value::Thread(crate::Thread::new(ctx)),
                Value::UserData(crate::UserData::new_static(&ctx, 42i64)),
                Value::Function(
                    crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return))
                        .into(),
                ),
            ];
            for &(expected, tag, bits) in &values {
                let encoded = Slot::from_value(expected);
                assert_eq!(encoded.tag, tag);
                assert_eq!(encoded.bits, bits);
                for original in destinations {
                    let slot = Slot { tag, bits };
                    let mut dest = original;
                    slot.write_back(&mut dest);
                    assert_identical(dest, expected);
                    assert_identical(slot.value(original), expected);
                }
            }
        });
    }

    #[test]
    fn reference_materialization_preserves_canonical_object_identity() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let values = [
                Value::String(crate::String::from_slice(&ctx, b"canonical")),
                Value::Table(crate::Table::new(&ctx)),
                Value::Function(
                    crate::Closure::load(ctx, None, b"return 42")
                        .unwrap()
                        .into(),
                ),
                Value::Thread(crate::Thread::new(ctx)),
                Value::UserData(crate::UserData::new_static(&ctx, 42i64)),
                Value::Function(
                    crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return))
                        .into(),
                ),
            ];
            for original in values {
                let mut canonical = original;
                let slot = Slot::from_value(original);
                assert_eq!(slot.tag, REFERENCE);
                assert_eq!(slot.bits, 0);
                slot.write_back(&mut canonical);
                assert_identical(canonical, original);
                assert_identical(slot.value(canonical), original);
                Slot::from_value(Value::Integer(-7)).write_back(&mut canonical);
                assert_identical(canonical, Value::Integer(-7));
            }
        });
    }

    #[test]
    fn reference_markers_preserve_canonical_scalar_payloads() {
        let mut values = vec![
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(-1),
            Value::Integer(0),
            Value::Integer(i64::MAX),
        ];
        values.extend(
            [
                0,
                (-0.0f64).to_bits(),
                1,
                u64::MAX,
                0x7ff0_0000_0000_0001,
                0x7ff8_0000_0000_1234,
            ]
            .into_iter()
            .map(|bits| Value::Number(f64::from_bits(bits))),
        );
        let slot = Slot {
            tag: REFERENCE,
            bits: 0,
        };
        for original in values {
            let mut canonical = original;
            slot.write_back(&mut canonical);
            assert_identical(canonical, original);
            assert_identical(slot.value(original), original);
        }
    }

    #[test]
    #[should_panic(expected = "invalid native scalar tag")]
    fn invalid_materialization_tag_is_rejected() {
        Slot {
            tag: u64::MAX,
            bits: 0,
        }
        .write_back(&mut Value::Nil);
    }
}
