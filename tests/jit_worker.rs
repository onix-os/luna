#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn run(
    limits: [(libc::c_int, libc::rlim_t, libc::rlim_t); 3],
    input: &[u8],
) -> (ExitStatus, Vec<u8>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_luna-jit-worker"));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            for (resource, current, maximum) in limits {
                let limit = libc::rlimit {
                    rlim_cur: current,
                    rlim_max: maximum,
                };
                if libc::setrlimit(resource as _, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut running = Running(command.spawn().unwrap());
    running.0.stdin.take().unwrap().write_all(input).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = running.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "helper startup exceeded deadline"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut bytes = Vec::new();
    running
        .0
        .stderr
        .take()
        .unwrap()
        .take(1025)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= 1024);
    (status, bytes)
}

fn limits() -> [(libc::c_int, libc::rlim_t, libc::rlim_t); 3] {
    [
        (libc::RLIMIT_AS as _, 512 << 20, 512 << 20),
        (libc::RLIMIT_CPU as _, 3, 3),
        (libc::RLIMIT_CORE as _, 0, 0),
    ]
}

#[test]
fn non_test_binary_rejects_eof_and_malformed_requests_without_panicking() {
    for input in [&[][..], &b"not-a-compiler-request"[..]] {
        let (status, bytes) = run(limits(), input);
        assert_eq!(status.code(), Some(67));
        assert!(bytes.is_empty());
    }
}

#[test]
fn startup_rejects_looser_cpu_memory_and_core_limits() {
    for index in 0..3 {
        let mut limits = limits();
        limits[index].1 += 1;
        limits[index].2 += 1;
        let (status, bytes) = run(limits, &[]);
        assert_eq!(status.code(), Some(64));
        assert!(bytes.is_empty());
    }
}

#[test]
fn startup_checks_hard_limits_even_when_soft_limits_are_admitted() {
    let mut limits = limits();
    limits[1].2 += 1;
    let (status, bytes) = run(limits, &[]);
    assert_eq!(status.code(), Some(64));
    assert!(bytes.is_empty());
}
