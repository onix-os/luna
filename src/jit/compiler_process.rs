use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use allocator_api2::vec::Vec;

use super::resources::BudgetAllocator;

pub(in crate::jit) use tests::{request_packet, request_packet_in};

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
    Startup(super::compiler_launch::Fault),
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

enum Launch<'a> {
    Fixture(Command),
    Image(&'a super::compiler_image::Image),
    Stalled(&'a super::compiler_image::Image),
    Owned(
        super::compiler_launch::Admission,
        super::compiler_jobs::Ticket,
    ),
    OwnedStalled(
        super::compiler_launch::Admission,
        super::compiler_jobs::Ticket,
        usize,
        usize,
    ),
}

impl<'a> From<Command> for Launch<'a> {
    fn from(command: Command) -> Self {
        Self::Fixture(command)
    }
}

enum Process<'a> {
    Fixture(Child),
    Image(super::compiler_launch::Child<'a>),
    Owned(
        super::compiler_launch::Child<'static>,
        super::compiler_jobs::Ticket,
    ),
    Empty,
}

enum Input {
    Fixture(std::process::ChildStdin),
    Image(super::compiler_launch::Input),
}

impl Write for Input {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Fixture(input) => input.write(bytes),
            Self::Image(input) => input.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Fixture(input) => input.flush(),
            Self::Image(input) => input.flush(),
        }
    }
}

impl AsRawFd for Input {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Self::Fixture(input) => input.as_raw_fd(),
            Self::Image(input) => input.as_raw_fd(),
        }
    }
}

struct Running<'a> {
    child: Process<'a>,
    input: Option<Input>,
    output: Option<std::fs::File>,
    status: Option<ExitStatus>,
    stopped: bool,
}

impl Running<'_> {
    fn id(&self) -> u32 {
        match &self.child {
            Process::Fixture(child) => child.id(),
            Process::Image(child) => child.id(),
            Process::Owned(child, _) => child.id(),
            Process::Empty => 0,
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.child {
            Process::Fixture(child) => child.try_wait(),
            Process::Image(child) => child.try_wait(),
            Process::Owned(child, _) => child.try_wait(),
            Process::Empty => Err(io::Error::from_raw_os_error(libc::ECHILD)),
        }
    }

    fn startup(&mut self) -> io::Result<Result<bool, super::compiler_launch::Fault>> {
        match &mut self.child {
            Process::Image(child) => child.startup(),
            Process::Owned(child, _) => child.startup(),
            _ => Ok(Ok(true)),
        }
    }

    fn stop(&mut self) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        if matches!(self.child, Process::Owned(..)) {
            if let Process::Owned(mut child, ticket) =
                std::mem::replace(&mut self.child, Process::Empty)
            {
                let result = child.cleanup_once();
                self.stopped = true;
                match result {
                    Ok(Some(status)) => self.status = Some(status),
                    result => {
                        if !child.finished() {
                            ticket.retain(child);
                        }
                        return Err(result
                            .err()
                            .unwrap_or_else(|| io::Error::from_raw_os_error(libc::EAGAIN)));
                    }
                }
            }
            return Ok(());
        }
        match &mut self.child {
            Process::Fixture(child) if self.status.is_none() => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.kill();
                self.status = Some(child.wait()?);
            }
            Process::Image(child) => {
                self.status = Some(child.stop()?);
            }
            _ => {}
        }
        self.stopped = true;
        Ok(())
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn nonblocking(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run<'a>(
    command: impl Into<Launch<'a>>,
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
    let command = command.into();
    let (stall, signal_faults, wait_faults) = match &command {
        Launch::Stalled(_) => (true, 0, 0),
        Launch::OwnedStalled(_, _, signals, waits) => (true, *signals, *waits),
        _ => (false, 0, 0),
    };
    let mut running = match command {
        Launch::Owned(image, ticket) | Launch::OwnedStalled(image, ticket, _, _) => {
            let mut child = super::compiler_launch::Child::spawn_owned(
                image,
                super::compiler_launch::Limits {
                    address_space: limits.address_space,
                    cpu_seconds: limits.cpu_seconds,
                    stall,
                },
            )
            .map_err(|_| fail(Cause::Io))?;
            child.fail_cleanup(signal_faults, wait_faults);
            Running {
                input: child.input.take().map(Input::Image),
                output: child.output.take(),
                child: Process::Owned(child, ticket),
                status: None,
                stopped: false,
            }
        }
        Launch::Image(image) | Launch::Stalled(image) => {
            let mut child = super::compiler_launch::Child::spawn(
                image,
                super::compiler_launch::Limits {
                    address_space: limits.address_space,
                    cpu_seconds: limits.cpu_seconds,
                    stall,
                },
            )
            .map_err(|_| fail(Cause::Io))?;
            Running {
                input: child.input.take().map(Input::Image),
                output: child.output.take(),
                child: Process::Image(child),
                status: None,
                stopped: false,
            }
        }
        Launch::Fixture(mut command) => {
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
            let mut child = command.spawn().map_err(|_| fail(Cause::Io))?;
            Running {
                input: child.stdin.take().map(Input::Fixture),
                output: child.stderr.take().map(|pipe| {
                    let fd: std::os::fd::OwnedFd = pipe.into();
                    fd.into()
                }),
                child: Process::Fixture(child),
                status: None,
                stopped: false,
            }
        }
    };
    let pid = running.id();
    let result = drive(&mut running, request, limits, deadline, &mut bytes);
    match result {
        Ok(()) => {
            running.stop().map_err(|_| Failure {
                cause: Cause::Io,
                pid: Some(pid),
                status: running.status,
            })?;
            Ok(Receipt { bytes, pid })
        }
        Err(cause) => {
            let _ = running.stop();
            Err(Failure {
                cause,
                pid: Some(pid),
                status: running.status,
            })
        }
    }
}

fn drive(
    running: &mut Running<'_>,
    request: &[u8],
    limits: Limits,
    deadline: Instant,
    bytes: &mut Vec<u8, BudgetAllocator>,
) -> Result<(), Cause> {
    let mut input = running.input.take();
    let mut output = running.output.take().ok_or(Cause::Io)?;
    nonblocking(input.as_ref().ok_or(Cause::Io)?.as_raw_fd()).map_err(|_| Cause::Io)?;
    nonblocking(output.as_raw_fd()).map_err(|_| Cause::Io)?;
    let mut sent = 0;
    let mut eof = false;
    let mut scratch = [0; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(Cause::Deadline);
        }
        {
            match running.startup().map_err(|_| Cause::Io)? {
                Err(fault) => return Err(Cause::Startup(fault)),
                Ok(false) => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Ok(true) => {}
            }
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
            running.status = running.try_wait().map_err(|_| Cause::Io)?;
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
