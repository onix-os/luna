use super::*;
use crate::jit::resources::{Ledger, LedgerRef};

fn memory() -> (Memory, BudgetAllocator, LedgerRef) {
    let host = Ledger::new(1 << 20);
    let allocator = BudgetAllocator(Ledger::child(1 << 20, host.clone()));
    let status = MemoryStatus::try_new(allocator.clone()).unwrap();
    (
        Memory {
            allocations: BudgetVec::new_in(allocator.clone()),
            total: MappingCounter::new(allocator.0.clone()),
            status,
            failure: Failure::None,
            limit: 8192,
            page: 4096,
        },
        allocator,
        host,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_os_core_refusals_preserve_legacy_status_and_all_reservations() {
        for core in [false, true] {
            for (case, expected) in [
                (0, Fault::ImageLimit),
                (1, Fault::MetadataLimit),
                (2, Fault::MappingLimit),
                (3, Fault::HostLimit),
                (4, Fault::AllocationDenied),
                (5, Fault::InvalidAlignment),
            ] {
                let (mut memory, allocator, host) = memory();
                let total = memory.total.clone();
                let mut seed = 0;
                match case {
                    0 => memory.limit = 0,
                    1 => allocator.0.fail_after(0),
                    2 => {
                        seed = memory.limit;
                        total.fetch_add(seed, Ordering::Relaxed);
                    }
                    3 => host.set_limit(host.current() + std::mem::size_of::<Segment>()),
                    4 => memory.failure = Failure::Allocate,
                    5 => memory.page = 0,
                    _ => unreachable!(),
                }
                if core {
                    assert_eq!(
                        native_fault::bounded(|| memory.allocate_checked(
                            1,
                            1,
                            JITMemoryKind::Executable
                        ))
                        .err(),
                        Some(expected)
                    );
                } else {
                    assert_eq!(
                        memory
                            .allocate(1, 1, JITMemoryKind::Executable)
                            .err()
                            .unwrap()
                            .kind(),
                        expected.into_io().kind()
                    );
                }
                let error = memory.status.error().unwrap();
                let reason = match case {
                    0 => "native image size",
                    1 => "JIT metadata",
                    4 => "native memory allocation or protection denied",
                    _ => "native mappings",
                };
                if case == 4 {
                    assert!(matches!(error, JitError::Unavailable(message) if message == reason));
                } else {
                    assert!(matches!(error, JitError::ResourceLimit(message) if message == reason));
                }
                assert_eq!(
                    (
                        memory.status.metadata_refused.load(Ordering::Relaxed),
                        memory.status.image_refused.load(Ordering::Relaxed),
                        memory.status.quota_refused.load(Ordering::Relaxed),
                        memory.status.unavailable.load(Ordering::Relaxed)
                    ),
                    (
                        case == 1,
                        case == 0,
                        matches!(case, 0 | 2 | 3 | 5),
                        case == 4
                    ),
                );
                assert_eq!(
                    (total.load(Ordering::Relaxed), total.requested()),
                    (seed, 0)
                );
                if case == 1 {
                    assert!(BudgetVec::<u8, _>::new_in(allocator.clone())
                        .try_reserve_exact(1)
                        .is_err());
                }
                drop(memory);
                assert_eq!((allocator.0.current(), host.current()), (0, 0));
                if seed != 0 {
                    total.fetch_sub(seed, Ordering::Relaxed);
                }
                assert_eq!((total.load(Ordering::Relaxed), total.requested()), (0, 0));
            }
        }
    }

    #[test]
    fn finalize_core_refusals_stay_typed_and_legacy_diagnostics_stay_outside() {
        for failure in [Failure::Protect, Failure::Pipeline] {
            let (mut memory, allocator, host) = memory();
            memory.failure = failure;
            let fault = native_fault::bounded(|| memory.finalize_checked(BranchProtection::None))
                .err()
                .unwrap();
            match failure {
                Failure::Protect => assert_eq!(fault, Fault::ProtectionDenied),
                Failure::Pipeline => {
                    assert!(matches!(fault, Fault::System(NativeOperation::Pipeline, _)))
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                memory.status.error(),
                Some(JitError::Unavailable(_))
            ));
            assert!(memory.finalize(BranchProtection::None).is_err());
            drop(memory);
            assert_eq!((allocator.0.current(), host.current()), (0, 0));
        }
    }
}

#[cfg(not(miri))]
mod native {
    use super::*;

    fn native_memory() -> (Memory, BudgetAllocator, LedgerRef) {
        let (mut memory, allocator, host) = memory();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page > 0 && (page as usize).is_power_of_two());
        memory.page = page as usize;
        memory.limit = 2 * memory.page;
        (memory, allocator, host)
    }

    fn permissions(pointer: *const u8) -> String {
        let address = pointer.expose_provenance();
        std::fs::read_to_string("/proc/self/maps")
            .unwrap()
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                let (start, end) = fields.next()?.split_once('-')?;
                let start = usize::from_str_radix(start, 16).ok()?;
                let end = usize::from_str_radix(end, 16).ok()?;
                (start <= address && address < end).then(|| fields.next().unwrap().to_owned())
            })
            .unwrap()
    }

    #[test]
    fn cache_pipeline_and_partial_protection_refusals_release_real_maps() {
        for failure in [
            Failure::Cache,
            Failure::Pipeline,
            Failure::ProtectAfterFirst,
            Failure::Protect,
        ] {
            let (mut memory, allocator, host) = native_memory();
            let total = memory.total.clone();
            native_fault::bounded(|| {
                memory
                    .allocate_checked(1, 1, JITMemoryKind::Executable)
                    .unwrap();
                memory
                    .allocate_checked(1, 1, JITMemoryKind::Executable)
                    .unwrap();
            });
            memory.failure = failure;
            let fault = native_fault::bounded(|| memory.finalize_checked(BranchProtection::None))
                .err()
                .unwrap();
            match failure {
                Failure::Cache => {
                    assert!(matches!(fault, Fault::System(NativeOperation::Cache, _)))
                }
                Failure::Pipeline => {
                    assert!(matches!(fault, Fault::System(NativeOperation::Pipeline, _)))
                }
                Failure::ProtectAfterFirst => assert_eq!(fault, Fault::PartialProtectionDenied),
                Failure::Protect => assert_eq!(fault, Fault::ProtectionDenied),
                _ => unreachable!(),
            }
            for (index, segment) in memory.allocations.iter().enumerate() {
                let protected = failure == Failure::Pipeline
                    || (failure == Failure::ProtectAfterFirst && index == 0);
                assert!(permissions(segment.base()).starts_with(if protected {
                    "r-x"
                } else {
                    "rw-"
                }));
            }
            assert!(matches!(
                memory.status.error(),
                Some(JitError::Unavailable(_))
            ));
            assert_eq!(
                (total.load(Ordering::Relaxed), total.requested()),
                (2 * memory.page, 2)
            );
            drop(memory);
            assert_eq!((total.load(Ordering::Relaxed), total.requested()), (0, 0));
            assert_eq!((allocator.0.current(), host.current()), (0, 0));
        }
    }

    #[test]
    fn core_and_legacy_success_keep_exact_bytes_permissions_and_idempotence() {
        for core in [false, true] {
            let (mut memory, allocator, host) = native_memory();
            memory.limit = 1 << 20;
            let total = memory.total.clone();
            let mut pointers = Vec::new();
            let mut mapped = 0;
            let mut requested = 0;
            for (size, align, kind, expected) in [
                (1, 8192, JITMemoryKind::Executable, "r-x"),
                (4097, 64, JITMemoryKind::ReadOnly, "r--"),
                (63, 8, JITMemoryKind::Writable, "rw-"),
            ] {
                let pointer = if core {
                    native_fault::bounded(|| memory.allocate_checked(size, align, kind)).unwrap()
                } else {
                    memory.allocate(size, align, kind).unwrap()
                };
                // The allocation is writable and owned by memory until finalization.
                unsafe { pointer.write(0x5a) };
                mapped += Request::checked(size, align, memory.page).unwrap().bytes;
                requested += size;
                pointers.push((pointer, expected));
            }
            if core {
                native_fault::bounded(|| memory.finalize_checked(BranchProtection::None)).unwrap();
            } else {
                memory.finalize(BranchProtection::None).unwrap();
            }
            memory.failure = Failure::Cache;
            native_fault::bounded(|| memory.finalize_checked(BranchProtection::None)).unwrap();
            for (pointer, expected) in pointers {
                assert!(permissions(pointer).starts_with(expected));
                // Finalization preserves readable bytes in the live allocation.
                assert_eq!(unsafe { pointer.read() }, 0x5a);
            }
            assert_eq!(
                (total.load(Ordering::Relaxed), total.requested()),
                (mapped, requested)
            );
            drop(memory);
            assert_eq!((total.load(Ordering::Relaxed), total.requested()), (0, 0));
            assert_eq!((allocator.0.current(), host.current()), (0, 0));
        }
    }
}
