use super::*;
use crate::jit::resources::Ledger;

const BYTES: &[u8] = b"immutable compiler owner";

fn identity() -> [u8; 32] {
    crate::jit::compiler_request::image_identity(BYTES, 1 << 20).unwrap()
}

fn charge() -> usize {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    rounded(BYTES.len(), 1 << 20, page).unwrap() + Owner::allocation_bytes()
}

fn allocators() -> (LedgerRef, BudgetAllocator) {
    let host = Ledger::try_host(1 << 20).unwrap();
    let metadata = BudgetAllocator(Ledger::try_child(1 << 20, host.clone()).unwrap());
    (host, metadata)
}

fn shared(host: LedgerRef, metadata: BudgetAllocator) -> Result<Owner, Error> {
    Image::shared(BYTES, &identity(), 1 << 20, host, metadata)
}

fn fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn exact_payload_and_complete_metadata_owner_charge_until_final_clone() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    host.set_limit(baseline + charge());
    metadata.0.set_limit(Owner::allocation_bytes());
    let owner = shared(host.clone(), metadata.clone()).ok().unwrap();
    let peer = owner.clone();
    let fd = owner.as_raw_fd();
    assert_eq!(host.accounted(), baseline + charge());
    assert_eq!(metadata.0.current(), Owner::allocation_bytes());
    assert_eq!(owner.as_raw_fd(), peer.as_raw_fd());
    drop(owner);
    assert_eq!(host.accounted(), baseline + charge());
    assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    drop(peer);
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    assert_eq!(host.accounted(), baseline);
    assert_eq!(metadata.0.current(), 0);
}

#[test]
fn host_and_metadata_limits_roll_back_descriptors_then_recover() {
    for metadata_denied in [false, true] {
        let (host, metadata) = allocators();
        let baseline = host.accounted();
        let before = fds();
        if metadata_denied {
            metadata.0.set_limit(Owner::allocation_bytes() - 1);
        } else {
            host.set_limit(baseline + charge() - 1);
        }
        assert!(matches!(
            shared(host.clone(), metadata.clone()),
            Err(Error::Allocation)
        ));
        assert_eq!(host.accounted(), baseline);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(fds(), before);
        host.set_limit(baseline + charge());
        metadata.0.set_limit(Owner::allocation_bytes());
        drop(shared(host.clone(), metadata.clone()).ok().unwrap());
        assert_eq!(host.accounted(), baseline);
        assert_eq!(fds(), before);
    }
}

#[test]
fn underlying_owner_denial_closes_temporary_image_and_preserves_fault() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let before = fds();
    metadata.0.fail_after(0);
    for _ in 0..2 {
        assert!(matches!(
            shared(host.clone(), metadata.clone()),
            Err(Error::Allocation)
        ));
        assert_eq!(host.accounted(), baseline);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(fds(), before);
    }
    metadata.0.fail_after(usize::MAX);
    drop(shared(host.clone(), metadata).ok().unwrap());
    assert_eq!(host.current(), 0);
}

#[test]
fn invalid_inputs_and_unrelated_host_roots_never_publish_an_owner() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let before = fds();
    assert!(matches!(
        Image::shared(&[], &[0; 32], 1, host.clone(), metadata.clone()),
        Err(Error::Limit)
    ));
    assert!(matches!(
        Image::shared(BYTES, &[0; 32], 1 << 20, host.clone(), metadata.clone()),
        Err(Error::Identity)
    ));
    assert!(matches!(
        Image::shared(BYTES, &identity(), 0, host.clone(), metadata.clone()),
        Err(Error::Configuration)
    ));
    let other = Ledger::try_host(1 << 20).unwrap();
    assert!(matches!(
        shared(other, metadata.clone()),
        Err(Error::Configuration)
    ));
    assert_eq!(host.accounted(), baseline);
    assert_eq!(fds(), before);
}

#[test]
fn overlapping_requests_cannot_reuse_a_live_shared_image_allowance() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    host.set_limit(baseline + charge());
    let owner = shared(host.clone(), metadata.clone()).ok().unwrap();
    let peer = owner.clone();
    assert!(matches!(
        shared(host.clone(), metadata.clone()),
        Err(Error::Allocation)
    ));
    drop(owner);
    assert!(matches!(
        shared(host.clone(), metadata.clone()),
        Err(Error::Allocation)
    ));
    assert_eq!(host.accounted(), baseline + charge());
    drop(peer);
    drop(shared(host.clone(), metadata).ok().unwrap());
    assert_eq!(host.current(), 0);
}

#[test]
fn thread_handoff_keeps_payload_and_metadata_until_the_last_owner() {
    let (host, metadata) = allocators();
    let baseline = host.accounted();
    let owner = shared(host.clone(), metadata.clone()).ok().unwrap();
    let peer = owner.clone();
    drop(owner);
    std::thread::scope(|scope| {
        let host = &host;
        scope
            .spawn(move || {
                assert_eq!(host.accounted(), baseline + charge());
                assert_eq!(peer.identity(), &identity());
                drop(peer);
                assert_eq!(host.accounted(), baseline);
            })
            .join()
            .unwrap();
    });
    assert_eq!(metadata.0.current(), 0);
}

#[test]
fn lua_memory_limit_and_cache_clear_observe_live_transaction_ownership() {
    let mut lua = crate::Lua::empty();
    let (host, metadata) = lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        (manager.host.clone(), manager.metadata.clone())
    });
    let before = lua.accounted_memory();
    let bootstrap = lua.jit_stats().bootstrap_bytes;
    lua.set_memory_limit(Some(before + charge() - 1));
    assert!(matches!(
        shared(host.clone(), metadata.clone()),
        Err(Error::Allocation)
    ));
    assert_eq!(lua.accounted_memory(), before);
    lua.set_memory_limit(Some(before + charge()));
    let owner = shared(host.clone(), metadata).ok().unwrap();
    assert_eq!(lua.accounted_memory(), before + charge());
    assert_eq!(lua.jit_stats().bootstrap_bytes, bootstrap);
    assert_eq!(lua.jit_stats().metadata_bytes, Owner::allocation_bytes());
    assert_eq!(lua.jit_stats().code_bytes, 0);
    lua.clear_jit_cache();
    assert_eq!(lua.accounted_memory(), before + charge());
    lua.set_memory_limit(Some(before));
    assert_eq!(owner.identity(), &identity());
    drop(owner);
    assert_eq!(lua.accounted_memory(), before);
}

#[test]
fn image_owner_can_outlive_lua_without_losing_the_host_or_metadata_charge() {
    let mut lua = crate::Lua::empty();
    let (host, metadata) = lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        (manager.host.clone(), manager.metadata.clone())
    });
    let owner = shared(host.clone(), metadata).ok().unwrap();
    drop(lua);
    assert_eq!(host.current(), charge());
    assert_ne!(unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_GETFD) }, -1);
    drop(owner);
    assert_eq!(host.accounted(), LedgerRef::allocation_bytes());
}
