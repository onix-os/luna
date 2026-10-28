use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use allocator_api2::vec::Vec;

use super::resources::BudgetAllocator;

pub(in crate::jit) use tests::request_packet;

pub(in crate::jit) struct Packet {
    pub source: Vec<u8, BudgetAllocator>,
    pub bytes: super::compiler_reply::Artifact,
    pub environment: Vec<u8, BudgetAllocator>,
    pub request: u64,
    pub build: [u8; 32],
}

impl Packet {
    pub fn binding(&self) -> super::backend::artifact::Binding<'_> {
        super::backend::artifact::Binding {
            request: self.request,
            build: &self.build,
            source: &self.source,
            environment: &self.environment,
        }
    }
}

fn reply_error(failure: super::compiler_reply::Failure) -> super::JitError {
    use super::compiler_reply::{Class, Failure};
    match failure {
        Failure::Protocol(_) => super::JitError::Unavailable("compiler response protocol"),
        Failure::Worker(fault) => match fault.class {
            Class::Resource => super::JitError::ResourceLimit(fault.label()),
            Class::Unavailable | Class::Rejected => super::JitError::Unavailable(fault.label()),
            Class::Configuration => super::JitError::InvalidConfiguration(fault.label()),
            Class::Compilation => super::JitError::Compilation(fault.label().into()),
        },
    }
}

#[derive(Clone, Copy)]
struct Limits {
    request: usize,
    response: usize,
    address_space: libc::rlim_t,
    cpu_seconds: libc::rlim_t,
    wall: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    Configuration,
    RequestLimit,
    Allocation,
    Io,
    ResponseLimit,
    Deadline,
    Exit,
}

#[derive(Debug)]
struct Failure {
    cause: Cause,
    pid: Option<u32>,
    status: Option<ExitStatus>,
}

struct Receipt {
    bytes: Vec<u8, BudgetAllocator>,
    pid: u32,
}

struct Running {
    child: Child,
    status: Option<ExitStatus>,
}

impl Running {
    fn stop(&mut self) {
        if self.status.is_none() {
            unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            let _ = self.child.kill();
            self.status = self.child.wait().ok();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

fn nonblocking(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run(
    mut command: Command,
    request: &[u8],
    limits: Limits,
    allocator: BudgetAllocator,
) -> Result<Receipt, Failure> {
    let fail = |cause| Failure {
        cause,
        pid: None,
        status: None,
    };
    let deadline = Instant::now()
        .checked_add(limits.wall)
        .ok_or_else(|| fail(Cause::Configuration))?;
    if limits.wall.is_zero()
        || [limits.address_space, limits.cpu_seconds]
            .iter()
            .any(|&limit| limit == 0 || limit == libc::RLIM_INFINITY)
    {
        return Err(fail(Cause::Configuration));
    }
    if request.len() > limits.request {
        return Err(fail(Cause::RequestLimit));
    }
    let mut bytes = Vec::new_in(allocator);
    bytes
        .try_reserve_exact(limits.response.min(4096))
        .map_err(|_| fail(Cause::Allocation))?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    unsafe {
        command.pre_exec(move || {
            for (resource, limit) in [
                (libc::RLIMIT_CORE, 0),
                (libc::RLIMIT_AS, limits.address_space),
                (libc::RLIMIT_CPU, limits.cpu_seconds),
            ] {
                let limit = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|_| fail(Cause::Io))?;
    let mut running = Running {
        child,
        status: None,
    };
    let pid = running.child.id();
    let result = drive(&mut running, request, limits, deadline, &mut bytes);
    match result {
        Ok(()) => Ok(Receipt { bytes, pid }),
        Err(cause) => {
            running.stop();
            Err(Failure {
                cause,
                pid: Some(pid),
                status: running.status,
            })
        }
    }
}

fn drive(
    running: &mut Running,
    request: &[u8],
    limits: Limits,
    deadline: Instant,
    bytes: &mut Vec<u8, BudgetAllocator>,
) -> Result<(), Cause> {
    let mut input = running.child.stdin.take();
    let mut output = running.child.stderr.take().ok_or(Cause::Io)?;
    nonblocking(input.as_ref().ok_or(Cause::Io)?.as_raw_fd()).map_err(|_| Cause::Io)?;
    nonblocking(output.as_raw_fd()).map_err(|_| Cause::Io)?;
    let mut sent = 0;
    let mut eof = false;
    let mut scratch = [0; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(Cause::Deadline);
        }
        if let Some(pipe) = &mut input {
            if sent < request.len() {
                match pipe.write(&request[sent..request.len().min(sent + 8192)]) {
                    Ok(0) => return Err(Cause::Io),
                    Ok(count) => sent += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err(Cause::Io),
                }
            }
            if sent == request.len() {
                input = None;
            }
        }
        if !eof {
            match output.read(&mut scratch) {
                Ok(0) => eof = true,
                Ok(count) => {
                    if count > limits.response - bytes.len() {
                        return Err(Cause::ResponseLimit);
                    }
                    let needed = bytes.len().checked_add(count).ok_or(Cause::ResponseLimit)?;
                    if needed > bytes.capacity() {
                        let capacity = bytes
                            .capacity()
                            .saturating_mul(2)
                            .max(needed)
                            .min(limits.response);
                        bytes
                            .try_reserve_exact(capacity - bytes.len())
                            .map_err(|_| Cause::Allocation)?;
                    }
                    bytes.extend_from_slice(&scratch[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(Cause::Io),
            }
        }
        if running.status.is_none() {
            running.status = running.child.try_wait().map_err(|_| Cause::Io)?;
        }
        if let Some(status) = running.status {
            if !status.success() {
                return Err(Cause::Exit);
            }
            if eof {
                return if sent == request.len() {
                    Ok(())
                } else {
                    Err(Cause::Io)
                };
            }
        }
        let mut descriptors = [
            libc::pollfd {
                fd: input.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
            libc::pollfd {
                fd: if eof { -1 } else { output.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 20) as i32;
        if unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                millis,
            )
        } == -1
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return Err(Cause::Io);
        }
    }
}

#[cfg(test)]
mod tests;
