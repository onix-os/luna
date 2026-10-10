use std::{marker::PhantomData, ptr::NonNull};

use super::{Slot, BOOLEAN, INTEGER, NIL, NUMBER, REFERENCE};
use crate::Value;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "payload/native.rs"]
mod native;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "payload/runtime.rs"]
pub(crate) mod runtime;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Payload {
    tag: u64,
    pointer: *mut (),
}

pub(crate) const POINTER_OFFSET: usize = std::mem::offset_of!(Payload, pointer);

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) fn check_native_read(entry: unsafe extern "C" fn(*const Payload) -> u64) {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        for value in [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"payload")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ] {
            let expected = Slot::from_value(value).bits;
            let mut values = [value];
            let mut frame = Frame::new(&mut values);
            let descriptor = frame.bind(0).unwrap();
            assert_eq!(unsafe { entry(&descriptor) }, expected);
        }
        for tag in [NIL, BOOLEAN, INTEGER, NUMBER, REFERENCE, u64::MAX] {
            let descriptor = Payload {
                tag,
                pointer: std::ptr::null_mut(),
            };
            assert_eq!(unsafe { entry(&descriptor) }, 0);
        }
        let mut value = u64::MAX;
        for tag in [NIL, REFERENCE, u64::MAX] {
            let descriptor = Payload {
                tag,
                pointer: std::ptr::from_mut(&mut value).cast(),
            };
            assert_eq!(unsafe { entry(&descriptor) }, 0);
        }
    });
}

struct Frame<'a, 'gc> {
    values: NonNull<Value<'gc>>,
    len: usize,
    borrow: PhantomData<&'a mut [Value<'gc>]>,
}

impl<'a, 'gc> Frame<'a, 'gc> {
    fn new(values: &'a mut [Value<'gc>]) -> Self {
        Self {
            values: NonNull::new(values.as_mut_ptr()).unwrap(),
            len: values.len(),
            borrow: PhantomData,
        }
    }

    fn bind(&mut self, index: usize) -> Option<Payload> {
        if index >= self.len {
            return None;
        }
        let value = unsafe { &mut *self.values.as_ptr().add(index) };
        let (tag, pointer) = match value {
            Value::Integer(value) => (INTEGER, std::ptr::from_mut(value).cast()),
            Value::Number(value) => (NUMBER, std::ptr::from_mut(value).cast()),
            Value::Boolean(value) => (BOOLEAN, std::ptr::from_mut(value).cast()),
            Value::Nil => (NIL, std::ptr::null_mut()),
            _ => (REFERENCE, std::ptr::null_mut()),
        };
        Some(Payload { tag, pointer })
    }

    fn read(&mut self, slots: &mut [Payload], index: usize) -> Option<Value<'gc>> {
        if slots.len() != self.len || index >= self.len {
            return None;
        }
        let value = unsafe { self.values.as_ptr().add(index).read() };
        slots[index] = self.bind(index).unwrap();
        Some(value)
    }

    fn store(&mut self, slots: &mut [Payload], index: usize, value: Value<'gc>) -> bool {
        if slots.len() != self.len || index >= self.len {
            return false;
        }
        unsafe { self.values.as_ptr().add(index).write(value) };
        slots[index] = self.bind(index).unwrap();
        true
    }
}

/// The descriptor must be live and rebound after canonical access to its value.
unsafe fn load(slot: Payload) -> Option<Slot> {
    let bits = match slot.tag {
        NIL if slot.pointer.is_null() => 0,
        INTEGER if !slot.pointer.is_null() => unsafe { slot.pointer.cast::<i64>().read() as u64 },
        NUMBER if !slot.pointer.is_null() => unsafe { slot.pointer.cast::<f64>().read().to_bits() },
        BOOLEAN if !slot.pointer.is_null() => unsafe {
            u64::from(slot.pointer.cast::<bool>().read())
        },
        _ => return None,
    };
    Some(Slot {
        tag: slot.tag,
        bits,
    })
}

/// The descriptor must be live and rebound after canonical access to its value.
unsafe fn store(slot: Payload, value: Slot) -> bool {
    if slot.tag != value.tag {
        return false;
    }
    match slot.tag {
        NIL if slot.pointer.is_null() => true,
        INTEGER if !slot.pointer.is_null() => {
            unsafe { slot.pointer.cast::<i64>().write(value.bits as i64) };
            true
        }
        NUMBER if !slot.pointer.is_null() => {
            unsafe { slot.pointer.cast::<f64>().write(f64::from_bits(value.bits)) };
            true
        }
        BOOLEAN if !slot.pointer.is_null() && value.bits <= 1 => {
            unsafe { slot.pointer.cast::<bool>().write(value.bits != 0) };
            true
        }
        _ => false,
    }
}

struct Gateway<'a, 'gc> {
    frame: Frame<'a, 'gc>,
    panic: Option<Box<dyn std::any::Any + Send>>,
}

type MoveHelper = unsafe extern "C" fn(*mut (), *mut Payload, u32, u32, bool) -> u32;

unsafe extern "C" fn move_helper(
    host: *mut (),
    slots: *mut Payload,
    dest: u32,
    source: u32,
    panic_after_store: bool,
) -> u32 {
    let host = unsafe { &mut *host.cast::<Gateway<'_, '_>>() };
    if host.panic.is_some() {
        return super::HELPER_PANICKED;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if dest as usize >= host.frame.len || source as usize >= host.frame.len {
            return false;
        }
        let slots = unsafe { std::slice::from_raw_parts_mut(slots, host.frame.len) };
        let value = host.frame.read(slots, source as usize).unwrap();
        let completed = host.frame.store(slots, dest as usize, value);
        if panic_after_store {
            panic!("payload helper unwind probe");
        }
        completed
    }));
    match result {
        Ok(true) => super::HELPER_COMPLETED,
        Ok(false) => super::HELPER_DECLINED,
        Err(panic) => {
            host.panic = Some(panic);
            super::HELPER_PANICKED
        }
    }
}

unsafe extern "C" fn gateway_entry(slots: *mut Payload, host: *mut (), helper: MoveHelper) -> u32 {
    unsafe {
        if !store(
            slots.read(),
            Slot {
                tag: INTEGER,
                bits: 41,
            },
        ) || helper(host, slots, 2, 0, false) != super::HELPER_COMPLETED
            || load(slots.add(2).read()).unwrap().bits != 41
            || !store(
                slots.read(),
                Slot {
                    tag: INTEGER,
                    bits: 42,
                },
            )
            || helper(host, slots, 0, 1, false) != super::HELPER_COMPLETED
            || !store(
                slots.read(),
                Slot {
                    tag: NUMBER,
                    bits: (-0.0f64).to_bits(),
                },
            )
            || helper(host, slots, 1, 1, false) != super::HELPER_COMPLETED
            || load(slots.add(1).read()).unwrap().bits != 2.0f64.to_bits()
            || helper(host, slots, 2, 3, false) != super::HELPER_COMPLETED
            || load(slots.add(2).read()).is_some()
            || helper(host, slots, 4, 0, false) != super::HELPER_DECLINED
            || helper(host, slots, 0, 4, false) != super::HELPER_DECLINED
        {
            return 0;
        }
        if helper(host, slots, 2, 0, true) != super::HELPER_PANICKED
            || helper(host, slots, 1, 0, false) != super::HELPER_PANICKED
        {
            return 0;
        }
    }
    1
}

#[test]
fn raw_gateway_reloads_rebound_descriptors_and_contains_helper_panics() {
    crate::Lua::empty().enter(|ctx| {
        let table = crate::Table::new(&ctx);
        let mut values = [
            Value::Integer(1),
            Value::Number(2.0),
            Value::Nil,
            Value::Table(table),
        ];
        {
            let mut host = Gateway {
                frame: Frame::new(&mut values),
                panic: None,
            };
            let mut slots: Vec<_> = (0..host.frame.len)
                .map(|i| host.frame.bind(i).unwrap())
                .collect();
            let original = slots.as_mut_ptr();
            let host_pointer = std::ptr::from_mut(&mut host).cast();
            let entry: unsafe extern "C" fn(*mut Payload, *mut (), MoveHelper) -> u32 =
                gateway_entry;
            assert_eq!(unsafe { entry(original, host_pointer, move_helper) }, 1);
            let panic = host.panic.take().expect("helper panic was not retained");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"payload helper unwind probe")
            );
        }
        assert!(matches!(values[0], Value::Number(n) if n.to_bits() == (-0.0f64).to_bits()));
        assert!(matches!(values[1], Value::Number(n) if n.to_bits() == 2.0f64.to_bits()));
        assert!(matches!(values[2], Value::Number(n) if n.to_bits() == (-0.0f64).to_bits()));
        assert!(matches!(values[3], Value::Table(t) if t == table));
    });
}

#[test]
fn direct_scalar_writes_preserve_types_and_float_bits() {
    let mut values = [
        Value::Integer(i64::MIN),
        Value::Number(-0.0),
        Value::Boolean(false),
        Value::Nil,
    ];
    {
        let mut frame = Frame::new(&mut values);
        let mut slots: Vec<_> = (0..frame.len).map(|i| frame.bind(i).unwrap()).collect();
        for bits in [0, 1, u64::MAX, 0x7ff8000000000042, 0x8000000000000000] {
            for (index, tag) in [(0, INTEGER), (1, NUMBER)] {
                assert!(unsafe { store(slots[index], Slot { tag, bits }) });
                assert_eq!(unsafe { load(slots[index]).unwrap() }.bits, bits);
                let value = frame.read(&mut slots, index).unwrap();
                assert_eq!(Slot::from_value(value).bits, bits);
                assert!(unsafe { store(slots[index], Slot { tag, bits: !bits }) });
            }
        }
        assert!(!unsafe {
            store(
                slots[2],
                Slot {
                    tag: BOOLEAN,
                    bits: 2,
                },
            )
        });
        assert!(unsafe {
            store(
                slots[2],
                Slot {
                    tag: BOOLEAN,
                    bits: 1,
                },
            )
        });
        assert!(!unsafe {
            store(
                slots[0],
                Slot {
                    tag: NUMBER,
                    bits: 0,
                },
            )
        });
        assert!(unsafe { load(slots[3]).is_some() });
    }
    assert!(matches!(values[0], Value::Integer(_)));
    assert!(matches!(values[1], Value::Number(_)));
    assert!(matches!(values[2], Value::Boolean(true)));
    assert!(matches!(values[3], Value::Nil));
}

#[test]
fn helper_rebinding_preserves_other_payloads_and_reference_identity() {
    crate::Lua::empty().enter(|ctx| {
        let table = crate::Table::new(&ctx);
        let mut values = [Value::Integer(1), Value::Table(table), Value::Number(2.0)];
        {
            let mut frame = Frame::new(&mut values);
            let mut slots: Vec<_> = (0..frame.len).map(|i| frame.bind(i).unwrap()).collect();
            assert!(slots[1].pointer.is_null());
            for index in 0..32 {
                assert!(unsafe {
                    store(
                        slots[0],
                        Slot {
                            tag: INTEGER,
                            bits: index,
                        },
                    )
                });
                assert!(frame.store(&mut slots, 1, Value::Integer(index as i64)));
                assert!(unsafe {
                    store(
                        slots[1],
                        Slot {
                            tag: INTEGER,
                            bits: index + 1,
                        },
                    )
                });
                assert_eq!(unsafe { load(slots[0]).unwrap() }.bits, index);
                assert!(frame.store(&mut slots, 1, Value::Table(table)));
                assert!(slots[1].pointer.is_null());
                assert!(unsafe { load(slots[1]).is_none() });
                assert!(matches!(frame.read(&mut slots, 1), Some(Value::Table(t)) if t == table));
                assert_eq!(unsafe { load(slots[2]).unwrap() }.bits, 2.0f64.to_bits());
            }
            assert!(frame.bind(3).is_none());
            assert!(frame.read(&mut slots, 3).is_none());
            assert!(!frame.store(&mut slots, 3, Value::Nil));
            assert!(frame.read(&mut slots[..2], 0).is_none());
            assert!(!frame.store(&mut slots[..2], 0, Value::Nil));
        }
        assert!(matches!(values[1], Value::Table(t) if t == table));
    });
}

#[test]
fn partial_native_writes_remain_canonical_after_unwinding() {
    let mut values = [Value::Integer(1), Value::Number(2.0)];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut frame = Frame::new(&mut values);
        let slots: Vec<_> = (0..frame.len).map(|i| frame.bind(i).unwrap()).collect();
        assert!(unsafe {
            store(
                slots[0],
                Slot {
                    tag: INTEGER,
                    bits: 42,
                },
            )
        });
        panic!("payload unwind probe");
    }));
    assert!(result.is_err());
    assert!(matches!(values[0], Value::Integer(42)));
    assert!(matches!(values[1], Value::Number(n) if n.to_bits() == 2.0f64.to_bits()));
}

#[test]
fn helper_type_changes_rebind_each_scalar_kind() {
    let mut values = [Value::Nil, Value::Integer(7)];
    {
        let mut frame = Frame::new(&mut values);
        let mut slots: Vec<_> = (0..frame.len).map(|i| frame.bind(i).unwrap()).collect();
        for value in [
            Value::Integer(i64::MIN),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::Boolean(true),
            Value::Nil,
            Value::Number(-0.0),
            Value::Integer(i64::MAX),
        ] {
            assert!(frame.store(&mut slots, 0, value));
            let actual = unsafe { load(slots[0]).unwrap() };
            let expected = Slot::from_value(value);
            assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
            assert!(unsafe { store(slots[0], expected) });
            assert_eq!(unsafe { load(slots[1]).unwrap() }.bits, 7);
        }
    }
    assert!(matches!(values[0], Value::Integer(i64::MAX)));
    assert!(matches!(values[1], Value::Integer(7)));
    let mut empty = [];
    let mut frame = Frame::new(&mut empty);
    assert!(frame.bind(0).is_none());
    assert!(frame.read(&mut [], 0).is_none());
    assert!(!frame.store(&mut [], 0, Value::Nil));
}
