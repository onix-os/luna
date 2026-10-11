use super::*;

#[path = "packed/scoped.rs"]
pub(crate) mod scoped;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "packed/native.rs"]
mod native;

const MASK: usize = 3;
const INT: usize = 0;
const FLOAT: usize = 1;
const FALSE: usize = 2;
const TRUE: usize = 3;
const EMPTY: usize = 4;
const OPAQUE: usize = 5;

#[repr(transparent)]
#[derive(Clone, Copy)]
pub(crate) struct Packed(*mut ());

impl Packed {
    fn bind(value: &mut Value<'_>) -> Option<Self> {
        let immediate = |bits| Self(std::ptr::without_provenance_mut(bits));
        let (pointer, tag) = match value {
            Value::Integer(value) => (std::ptr::from_mut(value).cast::<()>(), INT),
            Value::Number(value) => (std::ptr::from_mut(value).cast::<()>(), FLOAT),
            Value::Boolean(false) => return Some(immediate(FALSE)),
            Value::Boolean(true) => return Some(immediate(TRUE)),
            Value::Nil => return Some(immediate(EMPTY)),
            _ => return Some(immediate(OPAQUE)),
        };
        if pointer.addr() & MASK != 0 || pointer.addr() <= OPAQUE {
            return None;
        }
        Some(Self(pointer.map_addr(|address| address | tag)))
    }

    fn tag(self) -> Option<u64> {
        match self.0.addr() {
            FALSE | TRUE => Some(BOOLEAN),
            EMPTY => Some(NIL),
            OPAQUE => Some(REFERENCE),
            address if address & !MASK != 0 => match address & MASK {
                INT => Some(INTEGER),
                FLOAT => Some(NUMBER),
                _ => None,
            },
            _ => None,
        }
    }

    /// Numeric descriptors must retain their original live scalar-field loan.
    unsafe fn load(self) -> Option<Slot> {
        let tag = self.tag()?;
        let pointer = self.0.map_addr(|address| address & !MASK);
        let bits = match tag {
            NIL => 0,
            BOOLEAN => u64::from(self.0.addr() == TRUE),
            INTEGER => unsafe { pointer.cast::<i64>().read() as u64 },
            NUMBER => unsafe { pointer.cast::<f64>().read().to_bits() },
            _ => return None,
        };
        Some(Slot { tag, bits })
    }

    /// Numeric descriptors must retain their original exclusive scalar-field loan.
    unsafe fn store_numeric(self, value: Slot) -> bool {
        if self.tag() != Some(value.tag) {
            return false;
        }
        let pointer = self.0.map_addr(|address| address & !MASK);
        match value.tag {
            INTEGER => unsafe { pointer.cast::<i64>().write(value.bits as i64) },
            NUMBER => unsafe { pointer.cast::<f64>().write(f64::from_bits(value.bits)) },
            _ => return false,
        }
        true
    }
}

fn kinds<'gc>(ctx: crate::Context<'gc>) -> [Value<'gc>; 14] {
    let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
    [
        Value::Nil,
        Value::Boolean(false),
        Value::Boolean(true),
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Number(-0.0),
        Value::Number(f64::INFINITY),
        Value::Number(f64::from_bits(0x7ff8000000000042)),
        Value::String(ctx.intern(b"packed")),
        Value::Table(crate::Table::new(&ctx)),
        Value::Function(closure.into()),
        Value::Function(
            crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
        ),
        Value::Thread(crate::Thread::new(ctx)),
        Value::UserData(crate::UserData::new_static(&ctx, 42)),
    ]
}

#[test]
fn packed_descriptors_preserve_all_kinds_and_numeric_provenance() {
    assert_eq!(
        std::mem::size_of::<Packed>(),
        std::mem::size_of::<*mut ()>()
    );
    assert_eq!(
        std::mem::align_of::<Packed>(),
        std::mem::align_of::<*mut ()>()
    );
    crate::Lua::empty().enter(|ctx| {
        for initial in kinds(ctx) {
            let expected = Slot::from_value(initial);
            let mut value = initial;
            let Some(packed) = Packed::bind(&mut value) else {
                assert!(std::mem::align_of::<i64>() < 8 || std::mem::align_of::<f64>() < 8);
                continue;
            };
            assert_eq!(packed.tag(), Some(expected.tag));
            match unsafe { packed.load() } {
                Some(actual) => {
                    assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits))
                }
                None => assert_eq!(expected.tag, REFERENCE),
            }
            for tag in [NIL, BOOLEAN, INTEGER, NUMBER, REFERENCE, u64::MAX] {
                let next = Slot { tag, bits: 1 };
                let wrote = unsafe { packed.store_numeric(next) };
                assert_eq!(
                    wrote,
                    tag == expected.tag && matches!(tag, INTEGER | NUMBER)
                );
                if wrote {
                    let actual = unsafe { packed.load() }.unwrap();
                    assert_eq!((actual.tag, actual.bits), (tag, 1));
                }
            }
        }
    });
}

#[test]
fn packed_nonpointer_encodings_refuse_invalid_dereferences() {
    for address in 0..128 {
        if address >= 8 && matches!(address & MASK, INT | FLOAT) {
            continue;
        }
        let packed = Packed(std::ptr::without_provenance_mut(address));
        assert_eq!(
            packed.tag(),
            match address {
                FALSE | TRUE => Some(BOOLEAN),
                EMPTY => Some(NIL),
                OPAQUE => Some(REFERENCE),
                _ => None,
            }
        );
        assert!(!unsafe {
            packed.store_numeric(Slot {
                tag: INTEGER,
                bits: 42,
            })
        });
        assert!(!unsafe {
            packed.store_numeric(Slot {
                tag: NUMBER,
                bits: 42,
            })
        });
        assert_eq!(
            unsafe { packed.load() }.is_some(),
            matches!(address, FALSE | TRUE | EMPTY)
        );
    }
}

#[test]
fn packed_replacement_rebinds_only_the_destination() {
    crate::Lua::empty().enter(|ctx| {
        for initial in kinds(ctx) {
            for next in kinds(ctx) {
                let mut values = [initial, Value::Integer(1234), Value::Number(-0.0)];
                let base = values.as_mut_ptr();
                let Some(mut dest) = Packed::bind(unsafe { &mut *base }) else {
                    continue;
                };
                let Some(integer) = Packed::bind(unsafe { &mut *base.add(1) }) else {
                    continue;
                };
                let Some(number) = Packed::bind(unsafe { &mut *base.add(2) }) else {
                    continue;
                };
                assert_eq!(dest.tag(), Some(Slot::from_value(initial).tag));
                dest = Packed(std::ptr::without_provenance_mut(EMPTY));
                assert_eq!(unsafe { dest.load() }.unwrap().tag, NIL);
                unsafe { base.write(next) };
                dest = Packed::bind(unsafe { &mut *base }).unwrap();
                let expected = Slot::from_value(next);
                assert_eq!(dest.tag(), Some(expected.tag));
                if expected.tag != REFERENCE {
                    assert_eq!(unsafe { dest.load() }.unwrap().bits, expected.bits);
                }
                assert!(unsafe {
                    integer.store_numeric(Slot {
                        tag: INTEGER,
                        bits: 731,
                    })
                });
                assert!(unsafe {
                    number.store_numeric(Slot {
                        tag: NUMBER,
                        bits: 42f64.to_bits(),
                    })
                });
                assert_eq!(unsafe { integer.load() }.unwrap().bits, 731);
                assert_eq!(unsafe { number.load() }.unwrap().bits, 42f64.to_bits());
                assert!(matches!(values[1], Value::Integer(731)));
                assert!(matches!(values[2], Value::Number(42.0)));
            }
        }
    });
}

#[test]
fn packed_unwind_keeps_canonical_numeric_writes() {
    let mut values = [Value::Integer(0), Value::Number(0.0)];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (left, right) = values.split_at_mut(1);
        let (Some(left), Some(right)) = (Packed::bind(&mut left[0]), Packed::bind(&mut right[0]))
        else {
            return;
        };
        assert!(unsafe {
            left.store_numeric(Slot {
                tag: INTEGER,
                bits: 731,
            })
        });
        assert!(unsafe {
            right.store_numeric(Slot {
                tag: NUMBER,
                bits: (-0.0f64).to_bits(),
            })
        });
        std::panic::panic_any(731u64);
    }));
    if let Err(payload) = result {
        assert_eq!(*payload.downcast::<u64>().unwrap(), 731);
        assert!(matches!(values[0], Value::Integer(731)));
        assert!(matches!(values[1], Value::Number(n) if n.to_bits() == (-0.0f64).to_bits()));
    } else {
        assert!(std::mem::align_of::<i64>() < 8 || std::mem::align_of::<f64>() < 8);
    }
}
