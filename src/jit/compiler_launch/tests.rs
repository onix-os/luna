use super::*;
use crate::jit::resources::Ledger;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    time::{Duration, Instant},
};

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct Tracking;

fn allocated() {
    let _ = TRACK.try_with(|tracking| {
        if tracking.get() {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated();
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

fn image(bytes: &[u8]) -> Image {
    let identity = crate::jit::compiler_request::image_identity(bytes, 512 << 20).unwrap();
    Image::new(
        bytes,
        &identity,
        512 << 20,
        Ledger::try_host(513 << 20).unwrap(),
    )
    .ok()
    .unwrap()
}

fn limits() -> Limits {
    Limits {
        address_space: 512 << 20,
        cpu_seconds: 3,
        stall: false,
    }
}

fn startup(child: &mut Child<'_>) -> Result<(), Fault> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.startup().unwrap() {
            Ok(true) => return Ok(()),
            Err(fault) => return Err(fault),
            Ok(false) => assert!(Instant::now() < deadline),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn invalid_limits_refuse_before_launch_or_descriptor_growth() {
    if !super::isolation::enter(concat!(
        module_path!(),
        "::invalid_limits_refuse_before_launch_or_descriptor_growth"
    )) {
        return;
    }
    let image = image(b"not an executable");
    let before = fds();
    for (address_space, cpu_seconds) in [
        (0, 1),
        (libc::RLIM_INFINITY, 1),
        (1, 0),
        (1, libc::RLIM_INFINITY),
    ] {
        let result = Child::spawn(
            &image,
            Limits {
                address_space,
                cpu_seconds,
                ..limits()
            },
        );
        assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::EINVAL));
        assert_eq!(fds(), before);
    }
}

#[test]
fn missing_signal_capability_refuses_without_descriptors_or_children() {
    if !super::isolation::enter(concat!(
        module_path!(),
        "::missing_signal_capability_refuses_without_descriptors_or_children"
    )) {
        return;
    }
    let image = image(b"not an executable");
    let before = fds();
    let task = format!("/proc/self/task/{}/children", unsafe {
        libc::syscall(libc::SYS_gettid)
    });
    let children = std::fs::read(&task).unwrap();
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            -1,
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    assert_eq!(result, -1);
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EBADF) {
        let mut child = Child::spawn(&image, limits()).unwrap();
        assert!(startup(&mut child).is_err());
        assert_eq!(child.stop().unwrap().code(), Some(127));
    } else {
        assert_eq!(
            Child::spawn(&image, limits()).err().unwrap().raw_os_error(),
            error.raw_os_error()
        );
    }
    assert_eq!(fds(), before);
    assert_eq!(std::fs::read(&task).unwrap(), children);
}

#[test]
fn invalid_exec_reports_fixed_startup_fault_and_reaps() {
    if !super::isolation::enter(concat!(
        module_path!(),
        "::invalid_exec_reports_fixed_startup_fault_and_reaps"
    )) {
        return;
    }
    let image = image(b"not an executable");
    let before = fds();
    let mut child = Child::spawn(&image, limits()).unwrap();
    assert_eq!(
        startup(&mut child),
        Err(Fault {
            stage: Stage::Exec,
            errno: libc::ENOEXEC
        })
    );
    assert_eq!(child.stop().unwrap().code(), Some(127));
    let mut output = child.output.take().unwrap();
    let mut byte = [0];
    assert_eq!(output.read(&mut byte).unwrap(), 0);
    drop(output);
    drop(child);
    assert_eq!(fds(), before);
}

#[test]
fn cancellation_during_startup_reaps_the_pinned_child() {
    if !super::isolation::enter(concat!(
        module_path!(),
        "::cancellation_during_startup_reaps_the_pinned_child"
    )) {
        return;
    }
    use std::os::unix::process::ExitStatusExt;
    let image = image(b"not executed");
    let before = fds();
    let mut child = Child::spawn(
        &image,
        Limits {
            stall: true,
            ..limits()
        },
    )
    .unwrap();
    assert_eq!(child.startup().unwrap(), Ok(false));
    assert_eq!(child.try_wait().unwrap(), None);
    assert_eq!(child.stop().unwrap().signal(), Some(libc::SIGKILL));
    assert_eq!(
        child.try_wait().err().unwrap().raw_os_error(),
        Some(libc::ECHILD)
    );
    drop(child);
    assert_eq!(fds(), before);
}

#[test]
fn parent_thread_signal_mask_is_restored() {
    let image = image(b"not an executable");
    let mask = Mask::block().unwrap();
    let mut before = 0u64;
    let mut after = 0u64;
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            std::ptr::null::<u64>(),
            &mut before,
            8,
        );
    }
    let mut child = Child::spawn(&image, limits()).unwrap();
    assert!(startup(&mut child).is_err());
    child.stop().unwrap();
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            std::ptr::null::<u64>(),
            &mut after,
            8,
        );
    }
    assert_eq!(before, after);
    drop(mask);
}

#[test]
fn closed_request_socket_returns_epipe_without_pending_sigpipe() {
    let _mask = Mask::block().unwrap();
    let mut before: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut after: libc::sigset_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::sigpending(&mut before) }, 0);
    let [input, peer] = pair(true).unwrap();
    drop(peer);
    assert_eq!(
        Input(input).write(b"x").unwrap_err().raw_os_error(),
        Some(libc::EPIPE)
    );
    assert_eq!(unsafe { libc::sigpending(&mut after) }, 0);
    assert_eq!(
        unsafe { libc::sigismember(&before, libc::SIGPIPE) },
        unsafe { libc::sigismember(&after, libc::SIGPIPE) }
    );
}

#[test]
fn launch_startup_and_reap_have_no_parent_rust_allocations() {
    let image = image(b"not an executable");
    ALLOCATIONS.set(0);
    TRACK.set(true);
    let mut child = Child::spawn(&image, limits()).unwrap();
    let fault = startup(&mut child);
    let result = child.stop();
    drop(child);
    TRACK.set(false);
    assert!(fault.is_err());
    assert_eq!(result.unwrap().code(), Some(127));
    assert_eq!(ALLOCATIONS.get(), 0);
}

#[test]
fn parallel_launches_preserve_pinned_wait_ownership() {
    if !super::isolation::enter(concat!(
        module_path!(),
        "::parallel_launches_preserve_pinned_wait_ownership"
    )) {
        return;
    }
    let image = image(b"not an executable");
    let before = fds();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..4 {
                    let mut child = Child::spawn(&image, limits()).unwrap();
                    assert!(startup(&mut child).is_err());
                    assert_eq!(child.stop().unwrap().code(), Some(127));
                }
            });
        }
    });
    assert_eq!(fds(), before);
}

#[test]
fn closed_stdio_child() {
    if std::env::var_os("LUNA_LAUNCH_CLOSED_STDIO").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    assert_eq!(unsafe { libc::write(2, b"LCLOSE01".as_ptr().cast(), 8) }, 8);
    for fd in 0..3 {
        unsafe {
            libc::close(fd);
        }
    }
    let image = image(b"not an executable");
    assert_eq!(image.as_raw_fd(), 0);
    let mut old = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut old) }, 0);
    let small = libc::rlimit { rlim_cur: 4, ..old };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &small) }, 0);
    assert_eq!(
        Child::spawn(&image, limits()).err().unwrap().raw_os_error(),
        Some(libc::EMFILE)
    );
    for fd in 1..4 {
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) }, 0);
    let mut child = Child::spawn(&image, limits()).unwrap();
    assert_eq!(
        startup(&mut child),
        Err(Fault {
            stage: Stage::Exec,
            errno: libc::ENOEXEC
        })
    );
    assert_eq!(child.stop().unwrap().code(), Some(127));
    drop(child);
    for fd in 1..3 {
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    }
    unsafe {
        libc::_exit(0);
    }
}

#[test]
fn closed_stdio_and_descriptor_refusal_recover_in_an_isolated_parent() {
    use std::process::{Command, Stdio};
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Guard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jit::compiler_launch::tests::closed_stdio_child",
                "--test-threads=1",
                "--quiet",
            ])
            .env("LUNA_LAUNCH_CLOSED_STDIO", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = child.0.stderr.take().unwrap();
    nonblocking(output.as_raw_fd()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut actual = std::vec::Vec::new();
    let mut scratch = [0; 64];
    let mut eof = false;
    loop {
        match output.read(&mut scratch) {
            Ok(0) => eof = true,
            Ok(count) => {
                actual.extend_from_slice(&scratch[..count]);
                assert!(actual.len() <= 32768);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("{error}"),
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            if eof {
                assert!(
                    status.success(),
                    "{status}: {}",
                    String::from_utf8_lossy(&actual)
                );
                assert_eq!(actual, b"LCLOSE01");
                break;
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "requires the static probe built by make jit-compiler-launch"]
fn real_executable_has_isolated_environment_descriptors_and_limits() {
    let path = std::env::var_os("LUNA_LAUNCH_PROBE").expect("use make jit-compiler-launch");
    let bytes = std::fs::read(path).unwrap();
    let image = image(&bytes);
    let sentinel = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert!(sentinel >= 3);
    let sentinel = unsafe { OwnedFd::from_raw_fd(sentinel) };
    let mut child = Child::spawn(&image, limits()).unwrap();
    startup(&mut child).unwrap();
    drop(child.input.take());
    let mut output = child.output.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut actual = [0; 9];
    let mut count = 0;
    loop {
        match output.read(&mut actual[count..]) {
            Ok(0) => break,
            Ok(n) => {
                count += n;
                assert!(count < actual.len());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("{error}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(&actual[..count], b"LULNCH01");
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(child.stop().unwrap().success());
    assert_ne!(
        unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_GETFD) },
        -1
    );
}
