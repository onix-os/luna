use super::*;
use crate::jit::resources::Ledger;
use std::{
    io::Read,
    os::unix::{
        fs::{FileExt, PermissionsExt},
        process::CommandExt,
    },
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const CHILD: &str = "LUNA_IMAGE_FILESIZE_CHILD";

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn identity(bytes: &[u8]) -> [u8; 32] {
    crate::jit::compiler_request::image_identity(bytes, 1 << 20).unwrap()
}

fn ledger() -> LedgerRef {
    Ledger::try_host(1 << 20).unwrap()
}

fn create(bytes: &[u8], ledger: LedgerRef) -> Image {
    Image::new(bytes, &identity(bytes), 1 << 20, ledger)
        .ok()
        .unwrap()
}

fn mask() -> libc::sigset_t {
    let mut mask = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) },
        0
    );
    mask
}

fn same_mask(before: &libc::sigset_t) {
    let after = mask();
    for signal in 1..=64 {
        assert_eq!(unsafe { libc::sigismember(before, signal) }, unsafe {
            libc::sigismember(&after, signal)
        });
    }
}

fn pending() -> bool {
    let mut signals = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::sigpending(&mut signals) }, 0);
    unsafe { libc::sigismember(&signals, libc::SIGXFSZ) != 0 }
}

#[test]
fn byte_caps_and_page_rounding_are_checked_before_creation() {
    assert_eq!(rounded(1, 0, 4096), Err(Error::Configuration));
    assert_eq!(rounded(1, usize::MAX, 4096), Err(Error::Configuration));
    assert_eq!(rounded(1, 1, 0), Err(Error::Configuration));
    assert_eq!(rounded(0, 1, 4096), Err(Error::Limit));
    assert_eq!(rounded(2, 1, 4096), Err(Error::Limit));
    assert_eq!(
        rounded(isize::MAX as usize, isize::MAX as usize, 4096),
        Err(Error::Limit)
    );
    assert_eq!(rounded(4096, 8192, 4096), Ok(4096));
    assert_eq!(rounded(4097, 8192, 4096), Ok(8192));
    assert_eq!(rounded(5000, 5000, 3000), Ok(6000));
    let ledger = ledger();
    let before = ledger.accounted();
    assert!(matches!(
        Image::new(&[], &[0; 32], 1, ledger.clone()),
        Err(Error::Limit)
    ));
    assert!(matches!(
        Image::new(b"ab", &[0; 32], 1, ledger.clone()),
        Err(Error::Limit)
    ));
    assert_eq!(ledger.accounted(), before);
}

#[test]
fn exact_image_identity_and_payload_survive_sealing() {
    let bytes: Vec<u8> = (0..5000).map(|n| (n % 251) as u8).collect();
    let ledger = ledger();
    let before = ledger.accounted();
    let image = create(&bytes, ledger.clone());
    assert_eq!(image.identity(), &identity(&bytes));
    let mut actual = vec![0; bytes.len()];
    image.file.read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(actual, bytes);
    assert_eq!(image.file.metadata().unwrap().len(), bytes.len() as u64);
    assert_eq!(
        image.file.metadata().unwrap().permissions().mode() & 0o777,
        0o500
    );
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    assert_eq!(
        ledger.accounted() - before,
        rounded(bytes.len(), 1 << 20, page).unwrap()
    );
    assert_ne!(
        unsafe { libc::fcntl(image.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    drop(image);
    assert_eq!(ledger.accounted(), before);
}

#[test]
fn seals_reject_content_length_and_shared_write_mutation() {
    let image = create(b"immutable-image", ledger());
    let fd = image.as_raw_fd();
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    assert_eq!(seals & required, required);
    if seals & libc::F_SEAL_EXEC != 0 {
        assert_eq!(unsafe { libc::fchmod(fd, libc::S_IRUSR) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    }
    for length in [0, 65536] {
        assert_eq!(unsafe { libc::ftruncate(fd, length) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    }
    assert_eq!(unsafe { libc::pwrite(fd, b"x".as_ptr().cast(), 1, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    assert_eq!(mapping, libc::MAP_FAILED);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    let mut actual = [0; 15];
    image.file.read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(&actual, b"immutable-image");
}

#[test]
fn quota_boundaries_overlap_and_lowered_limits_preserve_live_images() {
    let ledger = ledger();
    let before = ledger.accounted();
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let bytes = b"quota-image";
    ledger.set_limit(before + page - 1);
    assert!(matches!(
        Image::new(bytes, &identity(bytes), 100, ledger.clone()),
        Err(Error::Allocation)
    ));
    assert_eq!(ledger.accounted(), before);
    assert_eq!(ledger.refusals(), 1);
    ledger.set_limit(before + page);
    let image = create(bytes, ledger.clone());
    assert!(matches!(
        Image::new(bytes, &identity(bytes), 100, ledger.clone()),
        Err(Error::Allocation)
    ));
    ledger.set_limit(before);
    let mut actual = [0; 11];
    image.file.read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(&actual, bytes);
    assert_eq!(ledger.accounted(), before + page);
    drop(image);
    assert_eq!(ledger.accounted(), before);
    assert_eq!(ledger.refusals(), 2);
}

#[test]
fn identity_refusals_and_repeated_drop_reclaim_descriptors_and_charges() {
    let descriptors = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    let before_fd = descriptors();
    let ledger = ledger();
    let before = ledger.accounted();
    for _ in 0..64 {
        assert!(matches!(
            Image::new(b"wrong-image", &[0; 32], 100, ledger.clone()),
            Err(Error::Identity)
        ));
        assert_eq!(ledger.accounted(), before);
        drop(create(b"right-image", ledger.clone()));
        assert_eq!(ledger.accounted(), before);
    }
    assert_eq!(descriptors(), before_fd);
}

#[test]
fn image_reservation_survives_thread_transfer_until_final_descriptor_drop() {
    let ledger = ledger();
    let before = ledger.accounted();
    let image = create(b"thread-image", ledger.clone());
    let charged = ledger.accounted();
    let image = std::thread::spawn(move || {
        let mut actual = [0; 12];
        image.file.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(&actual, b"thread-image");
        image
    })
    .join()
    .unwrap();
    assert_eq!(ledger.accounted(), charged);
    drop(image);
    assert_eq!(ledger.accounted(), before);
}

#[test]
fn successful_materialization_restores_the_calling_thread_signal_mask() {
    let before = mask();
    drop(create(b"signal-image", ledger()));
    same_mask(&before);
}

#[test]
fn file_size_refusals_do_not_terminate_the_embedding_process() {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "jit::compiler_image::tests::file_size_child",
            "--test-threads=1",
            "--nocapture",
            "--color=never",
        ])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    unsafe {
        child.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = Running(child.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            panic!("file-size fixture exceeded deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "file-size fixture failed: {status}");
    let mut output = Vec::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .take(1025)
        .read_to_end(&mut output)
        .unwrap();
    assert!(output.len() <= 1024);
    let output = std::str::from_utf8(&output).unwrap();
    assert_eq!(
        output
            .lines()
            .filter(|line| *line == "LUNA_IMAGE_REFUSALS_V1")
            .count(),
        1
    );
    assert!(output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
}

#[test]
fn file_size_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let ledger = ledger();
    let before = ledger.accounted();
    let original = mask();
    let mut set = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGXFSZ);
    }
    for blocked in [false, true] {
        assert_eq!(
            unsafe {
                libc::pthread_sigmask(
                    if blocked {
                        libc::SIG_BLOCK
                    } else {
                        libc::SIG_UNBLOCK
                    },
                    &set,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let before_mask = mask();
        assert!(!pending());
        assert!(matches!(
            Image::new(b"refused", &identity(b"refused"), 100, ledger.clone()),
            Err(Error::Write)
        ));
        assert!(!pending());
        same_mask(&before_mask);
        assert_eq!(ledger.accounted(), before);
    }
    assert_eq!(unsafe { libc::raise(libc::SIGXFSZ) }, 0);
    assert!(pending());
    assert!(matches!(
        Image::new(b"refused", &identity(b"refused"), 100, ledger.clone()),
        Err(Error::Write)
    ));
    assert!(pending());
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::sigtimedwait(&set, std::ptr::null_mut(), &timeout) },
        libc::SIGXFSZ
    );
    assert_eq!(
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &original, std::ptr::null_mut()) },
        0
    );
    assert_eq!(ledger.accounted(), before);
    let mut nofile = unsafe { std::mem::zeroed::<libc::rlimit>() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut nofile) },
        0
    );
    let refused = libc::rlimit {
        rlim_cur: 0,
        rlim_max: nofile.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &refused) }, 0);
    assert!(matches!(
        Image::new(b"descriptor", &identity(b"descriptor"), 100, ledger.clone()),
        Err(Error::Create)
    ));
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) }, 0);
    assert_eq!(ledger.accounted(), before);
    println!("\nLUNA_IMAGE_REFUSALS_V1");
}
