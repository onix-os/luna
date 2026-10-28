use std::{
    io::{ErrorKind, Read},
    os::fd::AsRawFd,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const KEY: &str = "LUNA_JIT_ISOLATED_TEST";

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(in crate::jit) fn enter(name: &str) -> bool {
    let name = name
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(name);
    if std::env::var_os(KEY).as_deref() == Some(std::ffi::OsStr::new(name)) {
        println!("\nLUNA_ISOLATED:{name}");
        return true;
    }
    if std::env::args_os().any(|arg| arg == "--test-threads=1") {
        return true;
    }
    let mut child = Running(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                name,
                "--test-threads=1",
                "--nocapture",
                "--color=never",
            ])
            .env(KEY, name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut output = child.0.stdout.take().unwrap();
    super::nonblocking(output.as_raw_fd()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut bytes = Vec::new();
    let mut scratch = [0; 1024];
    let mut eof = false;
    loop {
        match output.read(&mut scratch) {
            Ok(0) => eof = true,
            Ok(count) => {
                bytes.extend_from_slice(&scratch[..count]);
                assert!(bytes.len() <= 65536, "isolated output exceeded cap: {name}");
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
            Err(error) => panic!("isolated output: {name}: {error}"),
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            if eof {
                let text = String::from_utf8(bytes).unwrap();
                assert!(status.success(), "isolated test: {name}: {status}: {text}");
                let marker = format!("LUNA_ISOLATED:{name}");
                assert_eq!(
                    text.lines().filter(|line| *line == marker).count(),
                    1,
                    "isolated body did not execute exactly once: {name}: {text}"
                );
                assert!(
                    text.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
                    "isolated test count: {name}: {text}"
                );
                return false;
            }
        }
        assert!(
            Instant::now() < deadline,
            "isolated test exceeded deadline: {name}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
