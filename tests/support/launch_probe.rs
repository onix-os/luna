use std::io::Write;

unsafe extern "C" {
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn getpid() -> i32;
    fn getpgid(pid: i32) -> i32;
    fn getrlimit(resource: u32, limit: *mut [u64; 2]) -> i32;
    fn syscall(number: i64, ...) -> i64;
}

fn main() -> std::process::ExitCode {
    if std::env::vars_os().count() != 0
        || std::env::args_os().collect::<Vec<_>>() != ["luna-jit-worker"]
        || unsafe { getpid() != getpgid(0) }
    {
        return std::process::ExitCode::from(70);
    }
    for fd in 3..4096 {
        if unsafe { fcntl(fd, 1) } != -1 {
            return std::process::ExitCode::from(71);
        }
    }
    for (resource, expected) in [(9, 512 << 20), (0, 3), (4, 0)] {
        let mut limit = [0; 2];
        if unsafe { getrlimit(resource, &mut limit) } != 0 || limit != [expected; 2] {
            return std::process::ExitCode::from(72);
        }
    }
    #[cfg(target_arch = "x86_64")]
    let rt_sigprocmask = 14;
    #[cfg(target_arch = "aarch64")]
    let rt_sigprocmask = 135;
    let mut mask = u64::MAX;
    if unsafe { syscall(rt_sigprocmask, 2, std::ptr::null::<u64>(), &mut mask, 8) } != 0
        || mask != 0
    {
        return std::process::ExitCode::from(73);
    }
    let mut output = std::io::stdout().lock();
    if output
        .write_all(b"LULN")
        .and_then(|_| output.flush())
        .is_err()
        || std::io::stderr().write_all(b"CH01").is_err()
    {
        return std::process::ExitCode::from(74);
    }
    std::process::ExitCode::SUCCESS
}
