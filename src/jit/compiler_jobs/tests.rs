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
    maximum * std::mem::size_of::<Slot>() + AtomicShared::<Shared>::allocation_bytes()
}

fn finish(supervisor: Supervisor) {
    let completion = supervisor.completion();
    drop(supervisor);
    completion.join();
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
        crate::jit::compiler_launch::Admission::new(image(host, metadata), 64 << 10).unwrap(),
        Limits {
            address_space: 64 << 10,
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
    assert!(supervisor.0.wake.get().is_none());
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
        Err(CompileError::ResourceLimit("compiler job capacity"))
    ));
    supervisor.configure(1).unwrap();
    drop(one);
    assert!(supervisor.reserve().is_err());
    drop(two);
    metadata.0.fail_after(0);
    assert!(matches!(
        supervisor.configure(8),
        Err(CompileError::ResourceLimit("compiler jobs metadata"))
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
    finish(supervisor);
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
            Err(CompileError::ResourceLimit("compiler jobs metadata"))
        ));
        assert_eq!(host.accounted(), baseline);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(fds(), before);
        host.set_limit(16 << 20);
        metadata.0.set_limit(1 << 20);
        metadata.0.fail_after(usize::MAX);
        finish(Supervisor::new(2, metadata.clone()).unwrap());
        assert_eq!(host.accounted(), baseline);
    }
    let (_, metadata) = allocators();
    assert!(matches!(
        Supervisor::new(0, metadata.clone()),
        Err(CompileError::InvalidConfiguration(_))
    ));
    assert!(matches!(
        Supervisor::new(usize::MAX, metadata),
        Err(CompileError::ResourceLimit(_))
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
    finish(supervisor);
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
    child.fail_cleanup(usize::MAX, usize::MAX);
    let charged = host.accounted();
    assert_eq!(
        charged,
        baseline
            + unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize
            + crate::jit::compiler_image::Owner::allocation_bytes()
            + (64 << 10)
    );
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
    finish(supervisor);
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
    child.fail_cleanup(0, usize::MAX);
    ticket.retain(child);
    assert_eq!(supervisor.poll(), 1);
    assert!(host.accounted() > baseline);
    supervisor.fail_cleanup(0, 0);
    drain(&supervisor);
    assert_eq!(host.accounted(), baseline);
    finish(supervisor);
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
    finish(supervisor);
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
                child.fail_cleanup(usize::MAX, usize::MAX);
                ticket.retain(child);
            });
        }
    });
    assert_eq!(supervisor.occupied(), 4);
    assert!(supervisor.reserve().is_err());
    supervisor.fail_cleanup(0, 0);
    drain(&supervisor);
    assert_eq!(host.accounted(), baseline);
    finish(supervisor);
}

#[test]
fn budget_completion_start_refusal_rolls_back_then_recovers() {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            FAIL_START.set(self.0);
        }
    }
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    let admitted = host.accounted();
    let restore = Restore(FAIL_START.replace(true));
    for _ in 0..2 {
        assert!(matches!(
            supervisor.reserve(),
            Err(CompileError::ResourceLimit("compiler completion service"))
        ));
        assert_eq!(host.accounted(), admitted);
        assert_eq!(supervisor.occupied(), 0);
        assert!(supervisor.0.wake.get().is_none());
    }
    drop(restore);
    drop(supervisor.reserve().unwrap());
    finish(supervisor);
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn budget_tickets_are_clients_but_completion_receipts_are_not() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    let completion = supervisor.completion();
    let ticket = supervisor.reserve().unwrap();
    drop(supervisor);
    assert_eq!(completion.0.clients.load(Ordering::Acquire), 1);
    assert_eq!(completion.0.queue.lock().unwrap().occupied(), 1);
    drop(ticket);
    completion.join();
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn native_last_client_drop_returns_with_live_charges_under_permanent_cleanup_refusal() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    let ticket = supervisor.reserve().unwrap();
    let mut child = child(host.clone(), metadata.clone(), true);
    let faults = child.fail_cleanup(usize::MAX, usize::MAX);
    ticket.retain(child);
    let charged = host.accounted();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let dropping = std::thread::spawn(move || {
        drop(supervisor);
        send.send(()).unwrap();
    });
    let returned = receive.recv_timeout(Duration::from_secs(2)).is_ok();
    let retained = host.accounted();
    faults.reset(0, 0);
    dropping.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.accounted() != baseline {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(returned, "caller drop waited for denied cleanup");
    assert_eq!(retained, charged);
}

#[test]
fn native_completion_is_fair_and_progresses_without_foreground_polling() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let supervisor = Supervisor::new(2, metadata.clone()).unwrap();
    let one = supervisor.reserve().unwrap();
    let mut blocked = child(host.clone(), metadata.clone(), true);
    let faults = blocked.fail_cleanup(usize::MAX, usize::MAX);
    one.retain(blocked);
    let retained = host.accounted();
    let two = supervisor.reserve().unwrap();
    two.retain(child(host.clone(), metadata.clone(), false));
    let deadline = Instant::now() + Duration::from_secs(5);
    while (host.accounted() != retained || supervisor.occupied() != 1) && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    let progressed = host.accounted() == retained && supervisor.occupied() == 1;
    faults.reset(0, 0);
    finish(supervisor);
    assert!(
        progressed,
        "refused job starved a second job or required a foreground poll"
    );
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn native_blocked_cleanup_keeps_capacity_without_holding_the_queue_or_caller() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let supervisor = Supervisor::new(1, metadata.clone()).unwrap();
    let ticket = supervisor.reserve().unwrap();
    let mut job = child(host.clone(), metadata.clone(), true);
    let faults = job.fail_cleanup(0, 0);
    faults.pause();
    ticket.retain(job);
    let charged = host.accounted();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !faults.entered() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let entered = faults.entered();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let dropping = std::thread::spawn(move || {
        let occupied = supervisor.poll();
        let refused = matches!(
            supervisor.reserve(),
            Err(CompileError::ResourceLimit("compiler job capacity"))
        );
        drop(supervisor);
        let _ = send.send((occupied, refused));
    });
    let returned = receive.recv_timeout(Duration::from_secs(2)).ok();
    let retained = host.accounted();
    faults.resume();
    dropping.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.accounted() != baseline {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(entered, "service did not enter cleanup");
    assert_eq!(returned, Some((1, true)));
    assert_eq!(retained, charged);
}
