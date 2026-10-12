use std::{marker::PhantomData, mem, ptr};

use crate::{
    jit::{abi, helpers},
    Value,
};

#[repr(C)]
pub(in crate::jit) struct Header {
    pub base: *mut u8,
    pub offsets: [usize; 5],
    pub tags: [u8; 256],
}

impl Header {
    fn empty() -> Self {
        Self {
            base: ptr::null_mut(),
            offsets: [0; 5],
            tags: [0; 256],
        }
    }

    fn retire(&mut self) {
        self.base = ptr::null_mut();
    }

    fn bind(&mut self, values: &mut [Value<'_>]) {
        assert!(values.len() <= self.tags.len());
        self.retire();
        let mut seen = 0;
        for (index, value) in values.iter().enumerate() {
            let (tag, field, width, alignment) = match value {
                Value::Nil => (abi::NIL, ptr::null(), 0, 1),
                Value::Boolean(v) => (abi::BOOLEAN, ptr::from_ref(v).cast::<u8>(), 1, 1),
                Value::Integer(v) => (
                    abi::INTEGER,
                    ptr::from_ref(v).cast::<u8>(),
                    8,
                    mem::align_of::<i64>(),
                ),
                Value::Number(v) => (
                    abi::NUMBER,
                    ptr::from_ref(v).cast::<u8>(),
                    8,
                    mem::align_of::<f64>(),
                ),
                _ => (abi::REFERENCE, ptr::null(), 0, 1),
            };
            self.tags[index] = tag as u8;
            if !field.is_null() {
                let offset = field
                    .addr()
                    .checked_sub(ptr::from_ref(value).addr())
                    .unwrap();
                assert!(offset + width <= mem::size_of::<Value<'_>>());
                assert_eq!(offset % alignment, 0);
                if seen & (1 << tag) != 0 {
                    assert_eq!(self.offsets[tag as usize], offset);
                }
                self.offsets[tag as usize] = offset;
                seen |= 1 << tag;
            }
        }
        self.base = values.as_mut_ptr().cast();
    }

    unsafe fn pointer(&self, index: usize) -> *mut u8 {
        let tag = self.tags[index] as usize;
        if self.base.is_null() || !(1..=3).contains(&tag) {
            ptr::null_mut()
        } else {
            unsafe {
                self.base
                    .add(index * mem::size_of::<Value<'_>>() + self.offsets[tag])
            }
        }
    }
}

type Materialize = unsafe extern "C" fn(*mut Bridge, *mut Header, u32, u64, u64);

#[repr(C)]
pub(in crate::jit) struct Functions {
    helpers: [abi::HelperEntry; 10],
    write: Materialize,
    terminal: Materialize,
    copy: Materialize,
}

#[repr(C)]
pub(in crate::jit) struct Bridge {
    base: abi::Host,
    read: usize,
    functions: &'static Functions,
}

const fn functions<const PREPARED: bool>() -> Functions {
    Functions {
        helpers: [
            helper::<1, false>,
            helper::<2, false>,
            helper::<3, false>,
            helper::<4, false>,
            helper::<5, false>,
            helper::<6, PREPARED>,
            helper::<7, false>,
            helper::<8, false>,
            helper::<9, false>,
            helper::<10, false>,
        ],
        write: materialize::<false>,
        terminal: materialize::<false>,
        copy: materialize::<true>,
    }
}

static GENERIC: Functions = functions::<false>();
static PREPARED: Functions = functions::<true>();

struct Session<'s, 'gc, 'a, 'b, 'p> {
    frame: ptr::NonNull<helpers::Frame<'gc, 'a, 'b, 'p>>,
    terminal: Option<u32>,
    borrow: PhantomData<&'s mut helpers::Frame<'gc, 'a, 'b, 'p>>,
}

impl Session<'_, '_, '_, '_, '_> {
    fn bind(&mut self, header: &mut Header) {
        header.retire();
        if self.terminal.is_none() {
            let frame = unsafe { self.frame.as_mut() };
            header.bind(&mut frame.registers.stack_frame[..frame.slot_count]);
        }
    }
}

unsafe extern "C" fn helper<const KIND: u32, const PREHASHED: bool>(
    host: *mut abi::Host,
    slots: *mut abi::Slot,
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    let header = unsafe { &mut *slots.cast::<Header>() };
    header.retire();
    let session = unsafe { &mut *(*host).data.cast::<Session<'_, '_, '_, '_, '_>>() };
    if let Some(status) = session.terminal {
        return status;
    }
    let status = helpers::call_direct_selected::<KIND, PREHASHED>(
        unsafe { session.frame.as_mut() },
        a,
        b,
        c,
        pc,
    );
    if status != abi::HELPER_COMPLETED {
        session.terminal = Some(status);
    }
    session.bind(header);
    status
}

unsafe extern "C" fn materialize<const COPY: bool>(
    host: *mut Bridge,
    header: *mut Header,
    index: u32,
    tag: u64,
    bits: u64,
) {
    let header = unsafe { &mut *header };
    header.retire();
    let session = unsafe { &mut *(*host).base.data.cast::<Session<'_, '_, '_, '_, '_>>() };
    assert!(session.terminal.is_none());
    let frame = unsafe { session.frame.as_mut() };
    assert!((index as usize) < frame.slot_count);
    let value = if COPY {
        assert!(tag < frame.slot_count as u64);
        frame.registers.stack_frame[tag as usize]
    } else {
        match tag {
            abi::NIL => Value::Nil,
            abi::BOOLEAN if bits <= 1 => Value::Boolean(bits != 0),
            abi::INTEGER => Value::Integer(bits as i64),
            abi::NUMBER => Value::Number(f64::from_bits(bits)),
            _ => unreachable!("invalid strided scalar materialization"),
        }
    };
    frame.registers.stack_frame[index as usize] = value;
    session.bind(header);
}

pub(in crate::jit) fn with_frame<R>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    prepared: bool,
    body: impl FnOnce(*mut Header, *mut Bridge) -> R,
) -> R {
    assert!(frame.slot_count <= 256 && frame.slot_count <= frame.registers.stack_frame.len());
    assert!(frame.projection.is_none());
    assert!(frame.panic.is_none());
    let mut session = Session {
        frame: ptr::NonNull::from(frame),
        terminal: None,
        borrow: PhantomData,
    };
    let mut header = Header::empty();
    session.bind(&mut header);
    let mut bridge = Bridge {
        base: abi::Host {
            data: ptr::from_mut(&mut session).cast(),
            projection: ptr::null_mut(),
        },
        read: 0,
        functions: if prepared { &PREPARED } else { &GENERIC },
    };
    body(&mut header, &mut bridge)
}

const _: () = {
    use abi::payload::runtime as payload;
    assert!(mem::offset_of!(Bridge, functions) == mem::offset_of!(payload::Bridge, functions));
    assert!(mem::offset_of!(Functions, helpers) == mem::offset_of!(payload::Functions, helpers));
    assert!(mem::offset_of!(Functions, write) == mem::offset_of!(payload::Functions, write));
    assert!(mem::offset_of!(Functions, terminal) == mem::offset_of!(payload::Functions, terminal));
    assert!(mem::offset_of!(Functions, copy) == mem::offset_of!(payload::Functions, copy));
};

#[cfg(test)]
mod tests;
