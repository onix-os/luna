use super::*;
use crate::jit::{abi, helpers};

const READY: u32 = 4;
type Helper = unsafe extern "C" fn(*mut Header, *mut (), u32, u32, u32, u32) -> u32;
type Transition = unsafe extern "C" fn(*mut Header, *mut (), u32, u32, u32, u32) -> u32;

struct Session<'s, 'gc, 'a, 'b, 'p> {
    frame: ptr::NonNull<helpers::Frame<'gc, 'a, 'b, 'p>>,
    required: Mask,
    terminal: Option<u32>,
    borrow: PhantomData<&'s mut helpers::Frame<'gc, 'a, 'b, 'p>>,
}

impl Session<'_, '_, '_, '_, '_> {
    fn bind(&mut self) -> Header {
        if self.terminal.is_some() {
            return Header::retired();
        }
        let frame = unsafe { self.frame.as_mut() };
        Kernel::admit(
            &mut frame.registers.stack_frame[..frame.slot_count],
            self.required,
        )
        .unwrap_or_else(Header::retired)
    }
}

unsafe extern "C" fn helper<const KIND: u32>(
    header: *mut Header,
    data: *mut (),
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    unsafe { header.write(Header::retired()) };
    let session = unsafe { &mut *data.cast::<Session<'_, '_, '_, '_, '_>>() };
    if let Some(status) = session.terminal {
        return status;
    }
    let result = helpers::call_direct::<KIND>(unsafe { session.frame.as_mut() }, a, b, c, pc);
    if result == abi::HELPER_COMPLETED {
        unsafe { header.write(session.bind()) };
    } else {
        session.terminal = Some(result);
    }
    result
}

unsafe extern "C" fn rust_transition<const KIND: u32>(
    header: *mut Header,
    data: *mut (),
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    unsafe { rust_entry::<1, 1>(header, 1) };
    let result = unsafe { helper::<KIND>(header, data, a, b, c, pc) };
    if result != abi::HELPER_COMPLETED {
        return result;
    }
    result | (unsafe { rust_entry::<1, 1>(header, 2) } * READY)
}

fn with_session<R>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    required: Mask,
    body: impl FnOnce(*mut Header, *mut ()) -> R,
) -> R {
    assert!(frame.slot_count <= 256 && frame.slot_count <= frame.registers.stack_frame.len());
    assert!(frame.projection.is_none());
    let terminal = frame.panic.is_some().then_some(abi::HELPER_PANICKED);
    let mut session = Session {
        frame: ptr::NonNull::from(frame),
        required,
        terminal,
        borrow: PhantomData,
    };
    let mut header = session.bind();
    body(
        ptr::from_mut(&mut header),
        ptr::from_mut(&mut session).cast(),
    )
}

pub(in crate::jit) fn probe<const KIND: u32>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    args: [u32; 4],
    native: bool,
) -> u32 {
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let code = native.then(|| native::compile(helper::<KIND>));
    let entry: Transition = rust_transition::<KIND>;
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let entry = code.as_ref().map_or(entry, |code| code.entry);
    #[cfg(miri)]
    assert!(!native);
    let [a, b, c, pc] = args;
    let result = with_session(frame, Mask::new(&[1]).unwrap(), |header, data| unsafe {
        entry(header, data, a, b, c, pc)
    });
    let status = result & !READY;
    assert_eq!(
        result & READY != 0,
        status == abi::HELPER_COMPLETED
            && frame.slot_count > 1
            && matches!(frame.registers.stack_frame[1], Value::Integer(_))
    );
    status
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod native;

mod tests;
