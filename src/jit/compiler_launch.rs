use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::ExitStatusExt,
    },
    process::ExitStatus,
};

use super::compiler_image::{Image, Owner};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub address_space: libc::rlim_t,
    pub cpu_seconds: libc::rlim_t,
    #[cfg(test)]
    pub stall: bool,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Signals = 1,
    Group = 2,
    Limits = 3,
    Descriptors = 4,
    Exec = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Fault {
    pub stage: Stage,
    pub errno: i32,
}

pub(super) struct Input(OwnedFd);

impl Write for Input {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = unsafe {
            libc::send(
                self.0.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if count < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsRawFd for Input {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub(super) struct Child<'a> {
    pid: libc::pid_t,
    pidfd: OwnedFd,
    pub input: Option<Input>,
    pub output: Option<std::fs::File>,
    startup: std::fs::File,
    message: [u8; 8],
    received: usize,
    ready: bool,
    reaped: bool,
    _image: Lease<'a>,
    #[cfg(test)]
    kill_failures: usize,
    #[cfg(test)]
    wait_failures: usize,
}

enum Lease<'a> {
    Borrowed(&'a Image),
    Owned(Owner),
}

impl AsRawFd for Lease<'_> {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Borrowed(image) => image.as_raw_fd(),
            Self::Owned(image) => image.as_raw_fd(),
        }
    }
}

fn high(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() >= 3 {
        return Ok(fd);
    }
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

fn pair(socket: bool) -> io::Result<[OwnedFd; 2]> {
    let mut fds = [-1; 2];
    let result = unsafe {
        if socket {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        } else {
            libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC)
        }
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let a = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let b = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok([high(a)?, high(b)?])
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct Mask {
    old: u64,
    active: bool,
}

impl Mask {
    fn block() -> io::Result<Self> {
        let all = u64::MAX;
        let mut old = 0u64;
        if unsafe {
            libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::SIG_SETMASK,
                &all,
                &mut old,
                8,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { old, active: true })
    }

    fn restore(&mut self) -> io::Result<()> {
        if unsafe {
            libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::SIG_SETMASK,
                &self.old,
                std::ptr::null_mut::<u64>(),
                8,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for Mask {
    fn drop(&mut self) {
        if self.active {
            let _ = self.restore();
        }
    }
}

#[repr(C)]
struct Action {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

fn fail(fd: RawFd, stage: Stage) -> ! {
    let errno = unsafe { *libc::__errno_location() };
    let message = [stage as u32, errno as u32];
    unsafe {
        libc::syscall(libc::SYS_write, fd, message.as_ptr(), 8);
        loop {
            libc::syscall(libc::SYS_exit_group, 127);
            libc::syscall(libc::SYS_exit, 127);
        }
    }
}

fn child(image: RawFd, input: RawFd, output: RawFd, status: RawFd, limits: Limits) -> ! {
    #[cfg(test)]
    if limits.stall {
        loop {
            unsafe {
                libc::syscall(
                    libc::SYS_ppoll,
                    std::ptr::null::<libc::pollfd>(),
                    0,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u64>(),
                    8,
                );
            }
        }
    }
    let action = Action {
        handler: libc::SIG_DFL,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    for signal in 1..=64 {
        if signal != libc::SIGKILL
            && signal != libc::SIGSTOP
            && unsafe {
                libc::syscall(
                    libc::SYS_rt_sigaction,
                    signal,
                    &action,
                    std::ptr::null::<Action>(),
                    8,
                )
            } != 0
        {
            fail(status, Stage::Signals);
        }
    }
    if unsafe { libc::syscall(libc::SYS_setpgid, 0, 0) } != 0 {
        fail(status, Stage::Group);
    }
    for (resource, value) in [
        (libc::RLIMIT_AS, limits.address_space),
        (libc::RLIMIT_CPU, limits.cpu_seconds),
        (libc::RLIMIT_CORE, 0),
    ] {
        let limit = libc::rlimit {
            rlim_cur: value,
            rlim_max: value,
        };
        if unsafe {
            libc::syscall(
                libc::SYS_prlimit64,
                0,
                resource,
                &limit,
                std::ptr::null::<libc::rlimit>(),
            )
        } != 0
        {
            fail(status, Stage::Limits);
        }
    }
    for (from, to) in [(input, 0), (output, 1), (output, 2)] {
        if unsafe { libc::syscall(libc::SYS_dup3, from, to, 0) } == -1 {
            fail(status, Stage::Descriptors);
        }
    }
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 1u32 << 2) } != 0 {
        fail(status, Stage::Descriptors);
    }
    let empty = 0u64;
    if unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            &empty,
            std::ptr::null::<u64>(),
            8,
        )
    } != 0
    {
        fail(status, Stage::Signals);
    }
    let arguments = [c"luna-jit-worker".as_ptr(), std::ptr::null()];
    let environment = [std::ptr::null::<libc::c_char>()];
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            image,
            c"".as_ptr(),
            arguments.as_ptr(),
            environment.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    fail(status, Stage::Exec)
}

impl<'a> Child<'a> {
    pub fn spawn(image: &'a Image, limits: Limits) -> io::Result<Self> {
        Self::spawn_inner(Lease::Borrowed(image), limits)
    }

    fn spawn_inner(image: Lease<'a>, limits: Limits) -> io::Result<Self> {
        if [limits.address_space, limits.cpu_seconds]
            .iter()
            .any(|&value| value == 0 || value == libc::RLIM_INFINITY)
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let signal = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                -1,
                0,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if signal != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
            return Err(if signal == -1 {
                io::Error::last_os_error()
            } else {
                io::Error::from_raw_os_error(libc::EPROTO)
            });
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let wait = unsafe {
            libc::syscall(
                libc::SYS_waitid,
                libc::P_PIDFD,
                libc::c_int::MAX,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                std::ptr::null_mut::<libc::rusage>(),
            )
        };
        if wait != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
            return Err(if wait == -1 {
                io::Error::last_os_error()
            } else {
                io::Error::from_raw_os_error(libc::EPROTO)
            });
        }
        let executable = unsafe { libc::fcntl(image.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if executable == -1 {
            return Err(io::Error::last_os_error());
        }
        let executable = unsafe { OwnedFd::from_raw_fd(executable) };
        let [input, child_input] = pair(true)?;
        let [output, child_output] = pair(false)?;
        let [startup, child_startup] = pair(false)?;
        nonblocking(input.as_raw_fd())?;
        nonblocking(output.as_raw_fd())?;
        nonblocking(startup.as_raw_fd())?;
        let mut mask = Mask::block()?;
        let mut pidfd: RawFd = -1;
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone,
                libc::SIGCHLD | libc::CLONE_PIDFD,
                0usize,
                &mut pidfd,
                0usize,
                0usize,
            )
        };
        if pid == 0 {
            child(
                executable.as_raw_fd(),
                child_input.as_raw_fd(),
                child_output.as_raw_fd(),
                child_startup.as_raw_fd(),
                limits,
            );
        }
        if pid == -1 {
            return Err(io::Error::last_os_error());
        }
        let value = Self {
            pid: pid as libc::pid_t,
            pidfd: unsafe { OwnedFd::from_raw_fd(pidfd) },
            input: Some(Input(input)),
            output: Some(output.into()),
            startup: startup.into(),
            message: [0; 8],
            received: 0,
            ready: false,
            reaped: false,
            _image: image,
            #[cfg(test)]
            kill_failures: 0,
            #[cfg(test)]
            wait_failures: 0,
        };
        mask.restore()?;
        Ok(value)
    }

    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    pub fn startup(&mut self) -> io::Result<Result<bool, Fault>> {
        if self.ready {
            return Ok(Ok(true));
        }
        match self.startup.read(&mut self.message[self.received..]) {
            Ok(0) if self.received == 0 => {
                self.ready = true;
                Ok(Ok(true))
            }
            Ok(0) => Err(io::Error::from_raw_os_error(libc::EPROTO)),
            Ok(count) => {
                self.received += count;
                if self.received != 8 {
                    return Ok(Ok(false));
                }
                let stage = match u32::from_ne_bytes(self.message[..4].try_into().unwrap()) {
                    1 => Stage::Signals,
                    2 => Stage::Group,
                    3 => Stage::Limits,
                    4 => Stage::Descriptors,
                    5 => Stage::Exec,
                    _ => return Err(io::Error::from_raw_os_error(libc::EPROTO)),
                };
                Ok(Err(Fault {
                    stage,
                    errno: i32::from_ne_bytes(self.message[4..].try_into().unwrap()),
                }))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(Ok(false))
            }
            Err(error) => Err(error),
        }
    }

    fn observe(&self, options: libc::c_int) -> io::Result<Option<ExitStatus>> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::syscall(
                libc::SYS_waitid,
                libc::P_PIDFD,
                self.pidfd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED | options,
                std::ptr::null_mut::<libc::rusage>(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        let status = unsafe { info.si_status() };
        let raw = match info.si_code {
            libc::CLD_EXITED => status << 8,
            libc::CLD_KILLED => status,
            libc::CLD_DUMPED => status | 0x80,
            _ => return Err(io::Error::from_raw_os_error(libc::EPROTO)),
        };
        Ok(Some(ExitStatus::from_raw(raw)))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.observe(libc::WNOHANG | libc::WNOWAIT)
    }

    pub fn finished(&self) -> bool {
        self.reaped
    }

    #[cfg(test)]
    pub fn fail_cleanup(&mut self, signals: usize, waits: usize) {
        self.kill_failures = signals;
        self.wait_failures = waits;
    }

    fn signal(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if self.kill_failures != 0 {
            self.kill_failures -= 1;
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } == -1
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    fn ready(&self, millis: i32) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, millis) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }

    fn reap(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(test)]
        if self.wait_failures != 0 {
            self.wait_failures -= 1;
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        match self.observe(libc::WNOHANG) {
            Ok(Some(status)) => {
                self.reaped = true;
                Ok(Some(status))
            }
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.reaped = self.ready(0)?;
                Err(error)
            }
            result => result,
        }
    }

    pub fn cleanup_once(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.reaped {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        let signal = self.signal();
        match self.reap() {
            Ok(None) => signal.map(|()| None),
            result => result,
        }
    }

    pub fn stop(&mut self) -> io::Result<ExitStatus> {
        loop {
            match self.cleanup_once()? {
                Some(status) => return Ok(status),
                None => {
                    self.ready(20)?;
                }
            }
        }
    }
}

impl Child<'static> {
    pub fn spawn_owned(image: Owner, limits: Limits) -> io::Result<Self> {
        Self::spawn_inner(Lease::Owned(image), limits)
    }
}

impl Drop for Child<'_> {
    fn drop(&mut self) {
        while !self.reaped {
            let _ = self.cleanup_once();
            if !self.reaped {
                if self.ready(20).unwrap_or(true) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
