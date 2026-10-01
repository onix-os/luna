use std::io;

use cranelift_jit::{BranchProtection, JITMemoryKind};
use memmap2::{Mmap, MmapMut};

pub(super) struct Request {
    pub bytes: usize,
    align: usize,
    size: usize,
}

impl Request {
    pub fn new(size: usize, align: u64, page: usize) -> io::Result<Self> {
        let align =
            usize::try_from(align).map_err(|_| io::Error::other("native alignment overflow"))?;
        if !align.is_power_of_two() || !page.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid native alignment or page size",
            ));
        }
        let bytes = size
            .max(1)
            .checked_add(page - 1)
            .map(|size| size / page * page)
            .and_then(|size| size.checked_add(align.saturating_sub(page)))
            .filter(|size| *size <= isize::MAX as usize)
            .ok_or_else(|| io::Error::other("native allocation size overflow"))?;
        Ok(Self {
            bytes,
            align,
            size: size.max(1),
        })
    }

    fn offset(&self, base: usize) -> usize {
        base.wrapping_neg() & (self.align - 1)
    }
}

enum Pages {
    Writable(MmapMut),
    Protected(Mmap),
}

#[derive(Clone, Copy)]
enum Kind {
    Executable,
    ReadOnly,
    Writable,
}

pub(super) struct Segment {
    pages: Option<Pages>,
    kind: Kind,
    pub bytes: usize,
    finalized: bool,
}

impl Segment {
    pub fn new(request: Request, kind: JITMemoryKind) -> io::Result<(Self, *mut u8)> {
        let mut map = MmapMut::map_anon(request.bytes)?;
        let base = map.as_mut_ptr();
        let offset = request.offset(base as usize);
        if !offset
            .checked_add(request.size)
            .is_some_and(|end| end <= map.len())
        {
            return Err(io::Error::other("native alignment outside mapping"));
        }
        let pointer = unsafe { base.add(offset) };
        let kind = match kind {
            JITMemoryKind::Executable => Kind::Executable,
            JITMemoryKind::ReadOnly => Kind::ReadOnly,
            JITMemoryKind::Writable => Kind::Writable,
        };
        Ok((
            Self {
                pages: Some(Pages::Writable(map)),
                kind,
                bytes: request.bytes,
                finalized: false,
            },
            pointer,
        ))
    }

    pub fn finalize(&mut self, protection: BranchProtection) -> io::Result<()> {
        if self.finalized {
            return Ok(());
        }
        if matches!(self.kind, Kind::Writable) {
            self.finalized = true;
            return Ok(());
        }
        if matches!(self.kind, Kind::Executable) {
            let Some(Pages::Writable(map)) = &self.pages else {
                return Err(io::Error::other("native segment unavailable"));
            };
            unsafe { wasmtime_jit_icache_coherence::clear_cache(map.as_ptr().cast(), map.len()) }
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        let Some(Pages::Writable(map)) = self.pages.take() else {
            return Err(io::Error::other("native segment unavailable"));
        };
        let map = match self.kind {
            Kind::Executable => map.make_exec()?,
            Kind::ReadOnly => map.make_read_only()?,
            Kind::Writable => unreachable!(),
        };
        self.pages = Some(Pages::Protected(map));
        if matches!(self.kind, Kind::Executable) {
            self.enable_bti(protection)?;
        }
        self.finalized = true;
        Ok(())
    }

    fn enable_bti(&self, protection: BranchProtection) -> io::Result<()> {
        let Some(Pages::Protected(map)) = &self.pages else {
            return Err(io::Error::other("native segment unavailable"));
        };
        #[cfg(target_arch = "aarch64")]
        if protection == BranchProtection::BTI && std::arch::is_aarch64_feature_detected!("bti") {
            if unsafe {
                libc::mprotect(
                    map.as_ptr().cast_mut().cast(),
                    map.len(),
                    libc::PROT_READ | libc::PROT_EXEC | 0x10,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        let _ = (protection, map);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_request_preserves_every_page_aligned_base_and_payload_bound() {
        for page in [4096, 16384, 65536] {
            for align in [1, 8, 64, 4096, 8192, 131072] {
                for size in [0, 1, 63, 4095, 4096, 4097, 65537] {
                    let request = Request::new(size, align, page).unwrap();
                    assert_eq!(request.bytes % page, 0);
                    for index in 0..32 {
                        let base = index * page;
                        let offset = request.offset(base);
                        assert_eq!((base + offset) % align as usize, 0);
                        assert!(offset + size.max(1) <= request.bytes);
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_and_overflowing_requests_refuse_without_os_mapping() {
        for (size, align, page) in [
            (1, 0, 4096),
            (1, 3, 4096),
            (1, 8, 0),
            (1, 8, 4095),
            (usize::MAX, 1, 4096),
            (isize::MAX as usize, 8192, 4096),
            (1, 1u64 << 63, 4096),
        ] {
            assert!(Request::new(size, align, page).is_err());
        }
        assert_eq!(Request::new(0, 1, 4096).unwrap().bytes, 4096);
        assert_eq!(Request::new(4097, 1, 4096).unwrap().bytes, 8192);
        assert_eq!(Request::new(1, 8192, 4096).unwrap().bytes, 8192);
    }
}
