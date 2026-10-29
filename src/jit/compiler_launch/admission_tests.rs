use super::*;
use crate::jit::{
    resources::{BudgetAllocator, Ledger, LedgerRef},
    CompileError,
};

fn fixture() -> (LedgerRef, BudgetAllocator, Owner) {
    let host = Ledger::try_host(1 << 20).unwrap();
    let metadata = BudgetAllocator(Ledger::try_child(65536, host.clone()).unwrap());
    let bytes = b"workspace admission image";
    let identity = crate::jit::compiler_request::image_identity(&bytes[..], 65536).unwrap();
    let image = Image::shared(bytes, &identity, 65536, host.clone(), metadata.clone())
        .ok()
        .unwrap();
    (host, metadata, image)
}

#[test]
fn each_process_reserves_exact_capacity_without_allocating_or_sharing_workspace() {
    let (host, metadata, image) = fixture();
    let baseline = host.accounted();
    let before = metadata.0.current();
    let descriptor = image.as_raw_fd();
    let identity = *image.identity();
    metadata.0.fail_after(0);
    let one = Admission::new(image.clone(), 12345).unwrap();
    let two = Admission::new(image.clone(), 12345).unwrap();
    assert_eq!(host.accounted(), baseline + 24690);
    assert_eq!(metadata.0.current(), before);
    drop(one);
    assert_eq!(host.accounted(), baseline + 12345);
    drop(two);
    assert_eq!(host.accounted(), baseline);
    assert_eq!(image.as_raw_fd(), descriptor);
    assert_eq!(image.identity(), &identity);
}

#[test]
fn one_byte_host_refusal_rolls_back_and_only_explicit_headroom_recovers() {
    let (host, _, image) = fixture();
    let baseline = host.accounted();
    host.set_limit(baseline + 65535);
    for _ in 0..2 {
        assert!(matches!(
            Admission::new(image.clone(), 65536),
            Err(CompileError::ResourceLimit("compiler workspace"))
        ));
        assert_eq!(host.accounted(), baseline);
    }
    host.set_limit(baseline + 65536);
    let admission = Admission::new(image.clone(), 65536).unwrap();
    assert_eq!(host.accounted(), baseline + 65536);
    host.set_limit(baseline);
    assert_eq!(host.accounted(), baseline + 65536);
    drop(admission);
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn invalid_and_overflowing_limits_never_publish_an_admission() {
    let (host, _, image) = fixture();
    let baseline = host.accounted();
    for limit in [0, libc::RLIM_INFINITY] {
        assert!(matches!(
            Admission::new(image.clone(), limit),
            Err(CompileError::InvalidConfiguration("compiler workspace"))
        ));
        assert_eq!(host.accounted(), baseline);
    }
    assert!(matches!(
        Admission::new(image.clone(), libc::RLIM_INFINITY - 1),
        Err(CompileError::ResourceLimit("compiler workspace"))
    ));
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn a_different_applied_limit_refuses_before_spawn_and_releases_capacity() {
    let (host, _, image) = fixture();
    let baseline = host.accounted();
    let admission = Admission::new(image.clone(), 65536).unwrap();
    assert_eq!(host.accounted(), baseline + 65536);
    let error = Child::spawn_owned(
        admission,
        Limits {
            address_space: 65537,
            cpu_seconds: 3,
            stall: false,
        },
    )
    .err()
    .unwrap();
    assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn concurrent_admissions_never_overcommit_the_shared_image_host() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };
    let (host, _, image) = fixture();
    let baseline = host.accounted();
    host.set_limit(baseline + 2 * 65536);
    let barrier = Barrier::new(5);
    let accepted = AtomicUsize::new(0);
    let mut observed = (0, 0);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let image = &image;
            let barrier = &barrier;
            let accepted = &accepted;
            scope.spawn(move || {
                let admission = Admission::new(image.clone(), 65536);
                if admission.is_ok() {
                    accepted.fetch_add(1, Ordering::Relaxed);
                }
                barrier.wait();
                barrier.wait();
                drop(admission);
            });
        }
        barrier.wait();
        observed = (accepted.load(Ordering::Relaxed), host.accounted());
        barrier.wait();
    });
    assert_eq!(observed, (2, baseline + 2 * 65536));
    assert_eq!(host.accounted(), baseline);
}

#[test]
#[ignore = "requires the static probe built by make jit-worker-workspace"]
fn owned_probe_observes_the_exact_reserved_kernel_limit_until_reaped() {
    use std::time::{Duration, Instant};
    let path = std::env::var_os("LUNA_LAUNCH_PROBE").expect("use make jit-worker-workspace");
    let bytes = std::fs::read(path).unwrap();
    let identity = crate::jit::compiler_request::image_identity(&bytes[..], 512 << 20).unwrap();
    let host = Ledger::try_host(usize::MAX).unwrap();
    let metadata = BudgetAllocator(Ledger::try_child(1 << 20, host.clone()).unwrap());
    let image = Image::shared(&bytes, &identity, 512 << 20, host.clone(), metadata)
        .ok()
        .unwrap();
    let baseline = host.accounted();
    let admission = Admission::new(image.clone(), 512 << 20).unwrap();
    let mut child = Child::spawn_owned(
        admission,
        Limits {
            address_space: 512 << 20,
            cpu_seconds: 3,
            stall: false,
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.startup().unwrap() {
            Ok(true) => break,
            Ok(false) => assert!(Instant::now() < deadline),
            Err(fault) => panic!("owned startup: {fault:?}"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(child.input.take());
    let mut output = child.output.take().unwrap();
    let mut actual = [0; 9];
    let mut length = 0;
    loop {
        match output.read(&mut actual[length..]) {
            Ok(0) => break,
            Ok(count) => {
                length += count;
                assert!(length < actual.len());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => panic!("owned probe read: {error}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(&actual[..length], b"LULNCH01");
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(host.accounted(), baseline + (512 << 20));
    assert!(child.stop().unwrap().success());
    assert_eq!(host.accounted(), baseline + (512 << 20));
    drop(child);
    assert_eq!(host.accounted(), baseline);
}
