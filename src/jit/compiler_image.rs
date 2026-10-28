use std::{
    fs::File,
    io::{self, Write},
    os::fd::{AsRawFd, FromRawFd, RawFd},
};

use super::resources::{BudgetAllocator, LedgerRef, MappingCounter, Reservation};

pub(super) type Owner = super::atomic_owner::AtomicShared<Image>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Configuration,
    Limit,
    Allocation,
    Identity,
    Create,
    Write,
    Seal,
    Signal,
}

pub(super) struct Image {
    file: File,
    identity: [u8; 32],
    _reservation: Reservation,
}

fn rounded(length: usize, maximum: usize, page: usize) -> Result<usize, Error> {
    if maximum == 0 || maximum > isize::MAX as usize || page == 0 {
        return Err(Error::Configuration);
    }
    if length == 0 || length > maximum {
        return Err(Error::Limit);
    }
    length
        .checked_add(page - 1)
        .map(|length| length / page * page)
        .filter(|&length| length <= isize::MAX as usize)
        .ok_or(Error::Limit)
}

impl Image {
    pub fn shared(
        bytes: &[u8],
        expected: &[u8; 32],
        maximum: usize,
        ledger: LedgerRef,
        metadata: BudgetAllocator,
    ) -> Result<Owner, Error> {
        if !MappingCounter::new(ledger.clone()).same_root(&MappingCounter::new(metadata.0.clone()))
        {
            return Err(Error::Configuration);
        }
        let image = Self::new(bytes, expected, maximum, ledger)?;
        Owner::try_new(image, metadata).map_err(|_| Error::Allocation)
    }

    pub fn new(
        bytes: &[u8],
        expected: &[u8; 32],
        maximum: usize,
        ledger: LedgerRef,
    ) -> Result<Self, Error> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = usize::try_from(page).map_err(|_| Error::Configuration)?;
        let charge = rounded(bytes.len(), maximum, page)?;
        let reservation = Reservation::new(ledger, charge).map_err(|_| Error::Allocation)?;
        let identity = super::compiler_request::image_identity(bytes, maximum as u64)
            .map_err(|_| Error::Identity)?;
        if &identity != expected {
            return Err(Error::Identity);
        }
        let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
        let mut fd =
            unsafe { libc::memfd_create(c"luna-jit-worker".as_ptr(), flags | libc::MFD_EXEC) };
        if fd == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            fd = unsafe { libc::memfd_create(c"luna-jit-worker".as_ptr(), flags) };
        }
        if fd == -1 {
            return Err(Error::Create);
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        if unsafe { libc::fchmod(fd, libc::S_IRUSR | libc::S_IXUSR) } != 0 {
            return Err(Error::Create);
        }
        let mut signal = FileSizeSignal::block()?;
        let mut offset = 0;
        while offset < bytes.len() {
            let end = bytes.len().min(offset.saturating_add(16384));
            match file.write(&bytes[offset..end]) {
                Ok(0) => return Err(Error::Write),
                Ok(count) => offset += count,
                Err(error) => {
                    signal.generated = error.raw_os_error() == Some(libc::EFBIG);
                    return Err(Error::Write);
                }
            }
        }
        drop(signal);
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        let mut result = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals | libc::F_SEAL_EXEC) };
        if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            result = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) };
        }
        if result == -1 {
            return Err(Error::Seal);
        }
        let actual = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
        if actual == -1 || actual & seals != seals {
            return Err(Error::Seal);
        }
        Ok(Self {
            file,
            identity,
            _reservation: reservation,
        })
    }

    pub fn identity(&self) -> &[u8; 32] {
        &self.identity
    }
}

impl AsRawFd for Image {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

struct FileSizeSignal {
    set: libc::sigset_t,
    old: libc::sigset_t,
    pending: bool,
    generated: bool,
    active: bool,
}

impl FileSizeSignal {
    fn block() -> Result<Self, Error> {
        let mut value = Self {
            set: unsafe { std::mem::zeroed() },
            old: unsafe { std::mem::zeroed() },
            pending: true,
            generated: false,
            active: false,
        };
        unsafe {
            if libc::sigemptyset(&mut value.set) != 0
                || libc::sigaddset(&mut value.set, libc::SIGXFSZ) != 0
                || libc::pthread_sigmask(libc::SIG_BLOCK, &value.set, &mut value.old) != 0
            {
                return Err(Error::Signal);
            }
        }
        value.active = true;
        let mut pending = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigpending(&mut pending) } != 0 {
            return Err(Error::Signal);
        }
        value.pending = unsafe { libc::sigismember(&pending, libc::SIGXFSZ) } != 0;
        Ok(value)
    }
}

impl Drop for FileSizeSignal {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if self.generated && !self.pending {
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            loop {
                let signal =
                    unsafe { libc::sigtimedwait(&self.set, std::ptr::null_mut(), &timeout) };
                if signal != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
        }
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut()) };
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod owner_tests;
