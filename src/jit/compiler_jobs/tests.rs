use super::*;
use crate::jit::{
    compiler_image::Image,
    compiler_launch::Limits,
    resources::{Ledger, LedgerRef},
};
use std::time::{Duration, Instant};

fn allocators() -> (LedgerRef, BudgetAllocator) {
    let host = Ledger::try_host(16 << 20).unwrap();
    let metadata = BudgetAllocator(Ledger::try_child(1 << 20, host.clone()).unwrap());
    (host, metadata)
}

fn storage(maximum: usize) -> usize {
    maximum * std::mem::size_of::<Slot>() + AtomicShared::<Mutex<Queue>>::allocation_bytes()
}

fn image(host: LedgerRef, metadata: BudgetAllocator) -> crate::jit::compiler_image::Owner {
    let bytes = b"not an executable";
    let identity = crate::jit::compiler_request::image_identity(&bytes[..], 1 << 20).unwrap();
    Image::shared(bytes, &identity, 1 << 20, host, metadata)
        .ok()
        .unwrap()
}

fn child(host: LedgerRef, metadata: BudgetAllocator, stall: bool) -> Child<'static> {
    Child::spawn_owned(
        image(host, metadata),
        Limits {
            address_space: 512 << 20,
            cpu_seconds: 3,
            stall,
        },
    )
    .unwrap()
}

fn drain(supervisor: &Supervisor) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while supervisor.poll() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn budget_exact_slots_headers_ticket_rollback_and_reconfiguration() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let supervisor = Supervisor::new(2, metadata.clone()).unwrap();
    supervisor.ensure_host(&host).unwrap();
    assert!(supervisor
        .ensure_host(&Ledger::try_host(65536).unwrap())
        .is_err());
    assert_eq!(metadata.0.current(), storage(2));
    assert_eq!(host.accounted(), baseline + storage(2));
    let one = supervisor.reserve().unwrap();
    let two = supervisor.reserve().unwrap();
    assert!(matches!(
        supervisor.reserve(),
        Err(JitError::ResourceLimit("compiler job capacity"))
    ));
    supervisor.configure(1).unwrap();
    drop(one);
    assert!(supervisor.reserve().is_err());
    drop(two);
    metadata.0.fail_after(0);
    assert!(matches!(
        supervisor.configure(8),
        Err(JitError::ResourceLimit("compiler jobs metadata"))
    ));
    assert_eq!(supervisor.occupied(), 0);
    let ticket = supervisor.reserve().unwrap();
    assert!(supervisor.reserve().is_err());
    drop(ticket);
    metadata.0.fail_after(usize::MAX);
    supervisor.configure(8).unwrap();
    let tickets = (0..8)
        .map(|_| supervisor.reserve().unwrap())
        .collect::<std::vec::Vec<_>>();
    assert!(supervisor.reserve().is_err());
    drop(tickets);
    drop(supervisor);
    assert_eq!(host.accounted(), baseline);
    assert_eq!(metadata.0.current(), 0);
}

#[test]
fn budget_allocator_and_host_refusals_have_no_partial_supervisor() {
    if !crate::jit::compiler_launch::isolation::enter(concat!(
        module_path!(),
        "::budget_allocator_and_host_refusals_have_no_partial_supervisor"
    )) {
        return;
    }
    for cause in 0..4 {
        let (host, metadata) = allocators();
        let baseline = host.accounted();
        let before = fds();
        match cause {
            0 => metadata.0.fail_after(0),
            1 => metadata.0.fail_after(1),
            2 => metadata.0.set_limit(storage(2) - 1),
            _ => host.set_limit(baseline + storage(2) - 1),
        }
        assert!(matches!(
            Supervisor::new(2, metadata.clone()),
            Err(JitError::ResourceLimit("compiler jobs metadata"))
        ));
        assert_eq!(host.accounted(), baseline);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(fds(), before);
        host.set_limit(16 << 20);
        metadata.0.set_limit(1 << 20);
        metadata.0.fail_after(usize::MAX);
        drop(Supervisor::new(2, metadata.clone()).unwrap());
        assert_eq!(host.accounted(), baseline);
    }
    let (_, metadata) = allocators();
    assert!(matches!(
        Supervisor::new(0, metadata.clone()),
        Err(JitError::InvalidConfiguration(_))
    ));
    assert!(matches!(
        Supervisor::new(usize::MAX, metadata),
        Err(JitError::ResourceLimit(_))
    ));
}

#[test]
fn budget_scope_restores_nested_and_unwinding_supervisors() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    assert!(current().is_none());
    let outer = Scope::enter(Some(supervisor.clone()));
    assert!(current().is_some());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _inner = Scope::enter(None);
        assert!(current().is_none());
        panic!("scope unwind");
    }));
    assert!(result.is_err());
    assert!(current().is_some());
    drop(outer);
    assert!(current().is_none());
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = supervisor.lock();
        panic!("poisoned supervisor");
    }));
    assert!(poisoned.is_err());
    drop(supervisor.reserve().unwrap());
    drop(supervisor);
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn native_failed_cleanup_retains_image_charges_and_exhausts_admission_until_recovery() {
    if !crate::jit::compiler_launch::isolation::enter(concat!(
        module_path!(),
        "::native_failed_cleanup_retains_image_charges_and_exhausts_admission_until_recovery"
    )) {
        return;
    }
    let (host, metadata) = allocators();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    let baseline = host.accounted();
    let before = fds();
    let ticket = supervisor.reserve().unwrap();
    let mut child = child(host.clone(), metadata.clone(), true);
    child.fail_cleanup(100, 100);
    let charged = host.accounted();
    assert!(charged > baseline);
    assert!(child.cleanup_once().is_err());
    ticket.retain(child);
    assert_eq!(supervisor.occupied(), 1);
    assert!(supervisor.reserve().is_err());
    assert_eq!(host.accounted(), charged);
    supervisor.fail_cleanup(0, 0);
    drain(&supervisor);
    assert_eq!(host.accounted(), baseline);
    assert_eq!(fds(), before);
    drop(supervisor.reserve().unwrap());
}

#[test]
fn native_wait_refusal_keeps_an_exited_image_owned_until_confirmed_reap() {
    let (host, metadata) = allocators();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    let baseline = host.accounted();
    let ticket = supervisor.reserve().unwrap();
    let mut child = child(host.clone(), metadata.clone(), false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    child.fail_cleanup(0, 100);
    ticket.retain(child);
    assert_eq!(supervisor.poll(), 1);
    assert!(host.accounted() > baseline);
    supervisor.fail_cleanup(0, 0);
    drain(&supervisor);
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn native_final_supervisor_drop_finishes_cleanup_before_releasing_storage() {
    if !crate::jit::compiler_launch::isolation::enter(concat!(
        module_path!(),
        "::native_final_supervisor_drop_finishes_cleanup_before_releasing_storage"
    )) {
        return;
    }
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let before = fds();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    let ticket = supervisor.reserve().unwrap();
    let mut child = child(host.clone(), metadata.clone(), true);
    child.fail_cleanup(2, 2);
    ticket.retain(child);
    drop(supervisor);
    assert_eq!(host.accounted(), baseline);
    assert_eq!(fds(), before);
}

#[test]
fn native_external_reaping_requires_pinned_exit_confirmation() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let mut child = child(host.clone(), metadata, false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child.id() as i32, &mut status, 0) },
        child.id() as i32
    );
    assert!(!child.finished());
    assert_eq!(
        child.cleanup_once().err().unwrap().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert!(child.finished());
    drop(child);
    assert_eq!(host.accounted(), baseline - LedgerRef::allocation_bytes());
}

#[test]
fn native_concurrent_owned_children_respect_one_shared_slot_bound() {
    let (host, metadata) = allocators();
    let supervisor = Supervisor::new(4, metadata.clone()).unwrap();
    let baseline = host.accounted();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let supervisor = &supervisor;
            let host = &host;
            let metadata = &metadata;
            scope.spawn(move || {
                let ticket = supervisor.reserve().unwrap();
                let mut child = child(host.clone(), metadata.clone(), true);
                child.fail_cleanup(100, 100);
                ticket.retain(child);
            });
        }
    });
    assert_eq!(supervisor.occupied(), 4);
    assert!(supervisor.reserve().is_err());
    supervisor.fail_cleanup(0, 0);
    drain(&supervisor);
    assert_eq!(host.accounted(), baseline);
}
