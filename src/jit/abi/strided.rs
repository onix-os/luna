use std::{marker::PhantomData, mem, ptr};

use crate::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mask([u64; 4]);

impl Mask {
    fn new(indices: &[usize]) -> Option<Self> {
        let mut mask = Self([0; 4]);
        for &index in indices {
            if index >= 256 {
                return None;
            }
            mask.0[index / 64] |= 1 << (index % 64);
        }
        Some(mask)
    }

    fn contains(self, index: usize) -> bool {
        index < 256 && self.0[index / 64] & (1 << (index % 64)) != 0
    }

    fn indices(self) -> impl Iterator<Item = usize> {
        self.0
            .into_iter()
            .enumerate()
            .flat_map(|(group, mut word)| {
                std::iter::from_fn(move || {
                    if word == 0 {
                        return None;
                    }
                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;
                    Some(64 * group + bit)
                })
            })
    }
}

#[repr(C)]
struct Header {
    base: *mut u8,
    stride: usize,
    len: usize,
}

type Entry = unsafe extern "C" fn(*const Header, i64) -> u32;

struct Kernel<'code> {
    entry: Entry,
    required: Mask,
    owner: PhantomData<&'code ()>,
}

struct Window<'frame, 'gc, 'code> {
    header: Header,
    kernel: &'code Kernel<'code>,
    borrow: PhantomData<&'frame mut [Value<'gc>]>,
}

impl<'code> Kernel<'code> {
    fn bind<'frame, 'gc>(
        &'code self,
        values: &'frame mut [Value<'gc>],
    ) -> Option<Window<'frame, 'gc, 'code>> {
        if values.len() > 256 {
            return None;
        }
        let stride = mem::size_of::<Value<'gc>>();
        let origin = values.as_ptr().addr();
        let mut offset = None;
        for index in self.required.indices() {
            let Value::Integer(payload) = values.get(index)? else {
                return None;
            };
            let start = origin.checked_add(index.checked_mul(stride)?)?;
            let field = ptr::from_ref(payload).addr().checked_sub(start)?;
            if field.checked_add(mem::size_of::<i64>())? > stride
                || field % mem::align_of::<i64>() != 0
                || offset.is_some_and(|previous| previous != field)
            {
                return None;
            }
            offset = Some(field);
        }
        let offset = offset?;
        let len = values.len();
        let base = values.as_mut_ptr().cast::<u8>();
        // Derive the field address from the full mutable slice's provenance.
        let base = unsafe { base.add(offset) };
        Some(Window {
            header: Header { base, stride, len },
            kernel: self,
            borrow: PhantomData,
        })
    }
}

impl Window<'_, '_, '_> {
    fn invoke(self, delta: i64) -> u32 {
        // The scoped kernel accesses only admitted integer fields.
        unsafe { (self.kernel.entry)(&self.header, delta) }
    }
}

unsafe extern "C" fn rust_entry<const SOURCE: usize, const DEST: usize>(
    header: *const Header,
    delta: i64,
) -> u32 {
    if header.is_null() {
        return 0;
    }
    let header = unsafe { &*header };
    if SOURCE >= header.len || DEST >= header.len || header.base.is_null() {
        return 0;
    }
    let source = unsafe { header.base.add(SOURCE * header.stride).cast::<i64>() };
    let dest = unsafe { header.base.add(DEST * header.stride).cast::<i64>() };
    unsafe { dest.write(source.read().wrapping_add(delta)) };
    1
}

fn rust_kernel<const SOURCE: usize, const DEST: usize>(required: Mask) -> Option<Kernel<'static>> {
    if !required.contains(SOURCE) || !required.contains(DEST) {
        return None;
    }
    Some(Kernel {
        entry: rust_entry::<SOURCE, DEST>,
        required,
        owner: PhantomData,
    })
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod native;

mod tests;
