use super::*;
use crate::jit::segments::Request;

#[test]
fn typed_faults_are_fixed_copy_values_without_diagnostic_owners() {
    assert!(!std::mem::needs_drop::<Fault>());
    assert!(!std::mem::needs_drop::<System>());
    assert!(std::mem::size_of::<Fault>() <= 32);
    bounded(|| {
        let fault = Fault::capture(Operation::Map, io::Error::from_raw_os_error(libc::ENOMEM));
        let copied = fault;
        assert_eq!(copied, fault);
        assert!(matches!(
            copied,
            Fault::System(
                Operation::Map,
                System {
                    code: Some(libc::ENOMEM),
                    ..
                }
            )
        ));
    });
}

#[test]
fn checked_requests_preserve_rounding_and_exact_legacy_faults() {
    for page in [4096, 16384, 65536] {
        for align in [1, 8, 64, 4096, 8192, 131072] {
            for size in [0, 1, 63, 4095, 4096, 4097, 65537] {
                let checked = bounded(|| Request::checked(size, align, page)).unwrap();
                assert_eq!(
                    checked.bytes,
                    Request::new(size, align, page).unwrap().bytes
                );
                assert_eq!(checked.bytes % page, 0);
            }
        }
    }
    for (size, align, page, fault) in [
        (1, 0, 4096, Fault::InvalidAlignment),
        (1, 3, 4096, Fault::InvalidAlignment),
        (1, 8, 0, Fault::InvalidAlignment),
        (1, 8, 4095, Fault::InvalidAlignment),
        (usize::MAX, 1, 4096, Fault::SizeOverflow),
        (isize::MAX as usize, 8192, 4096, Fault::SizeOverflow),
    ] {
        assert_eq!(
            bounded(|| Request::checked(size, align, page)).err(),
            Some(fault)
        );
        let legacy = Request::new(size, align, page).err().unwrap();
        let expected = fault.into_io();
        assert_eq!(
            (legacy.kind(), legacy.to_string()),
            (expected.kind(), expected.to_string())
        );
    }
}

#[test]
fn captured_os_codes_preserve_operation_and_adapter_diagnostics() {
    for operation in [
        Operation::Map,
        Operation::Cache,
        Operation::Protect,
        Operation::Bti,
        Operation::Pipeline,
    ] {
        for code in [libc::EACCES, libc::ENOMEM, libc::EINVAL, libc::EIO] {
            let original = io::Error::from_raw_os_error(code);
            let kind = original.kind();
            let message = original.to_string();
            let fault = bounded(|| Fault::capture(operation, original));
            assert_eq!(
                fault,
                Fault::System(
                    operation,
                    System {
                        code: Some(code),
                        kind
                    }
                )
            );
            let legacy = fault.into_io();
            assert_eq!(legacy.to_string(), message);
            if operation == Operation::Cache {
                assert_eq!(
                    (legacy.kind(), legacy.raw_os_error()),
                    (io::ErrorKind::Other, None)
                );
            } else {
                assert_eq!((legacy.kind(), legacy.raw_os_error()), (kind, Some(code)));
            }
            let module = fault.into_module().to_string();
            assert!(module.contains(&message));
            if operation == Operation::Pipeline {
                assert!(module.contains("native pipeline flush:"));
            }
        }
    }
}

#[test]
fn bounded_scopes_reject_both_adapters_and_restore_nested_panics() {
    assert_adapters_allowed();
    bounded(|| {
        bounded(|| assert!(BOUNDED.get()));
        assert!(BOUNDED.get());
        assert!(std::panic::catch_unwind(|| bounded(|| Fault::ImageLimit.into_io())).is_err());
        assert!(
            std::panic::catch_unwind(|| bounded(|| Fault::ProtectionDenied.into_module())).is_err()
        );
        assert!(BOUNDED.get());
    });
    assert_adapters_allowed();
    assert!(std::panic::catch_unwind(|| bounded(assert_adapters_allowed)).is_err());
    assert_adapters_allowed();
}

#[test]
fn coherence_shims_preserve_success_and_failure_phase() {
    assert_eq!(
        coherence(Operation::Pipeline, Ok::<_, io::Error>(17)),
        Ok(17)
    );
    let failed: Result<(), Fault> = bounded(|| {
        coherence(
            Operation::Pipeline,
            Err::<(), _>(io::Error::from_raw_os_error(libc::EACCES)),
        )
    });
    #[cfg(not(miri))]
    assert!(matches!(
        failed,
        Err(Fault::System(
            Operation::Pipeline,
            System {
                code: Some(libc::EACCES),
                ..
            }
        ))
    ));
    #[cfg(miri)]
    assert!(matches!(
        failed,
        Err(Fault::System(
            Operation::Pipeline,
            System {
                code: None,
                kind: io::ErrorKind::Other
            }
        ))
    ));
}

#[test]
fn static_fault_adapter_kinds_and_messages_remain_exact() {
    for (fault, kind, message) in [
        (
            Fault::AlignmentOverflow,
            io::ErrorKind::Other,
            "native alignment overflow",
        ),
        (
            Fault::InvalidAlignment,
            io::ErrorKind::InvalidInput,
            "invalid native alignment or page size",
        ),
        (
            Fault::SizeOverflow,
            io::ErrorKind::Other,
            "native allocation size overflow",
        ),
        (
            Fault::AlignmentOutsideMapping,
            io::ErrorKind::Other,
            "native alignment outside mapping",
        ),
        (
            Fault::SegmentUnavailable,
            io::ErrorKind::Other,
            "native segment unavailable",
        ),
        (
            Fault::ImageLimit,
            io::ErrorKind::Other,
            "native image exceeds code cache limit",
        ),
        (
            Fault::MetadataLimit,
            io::ErrorKind::Other,
            "native allocation record quota exhausted",
        ),
        (
            Fault::MappingLimit,
            io::ErrorKind::Other,
            "native memory quota exhausted",
        ),
        (
            Fault::HostLimit,
            io::ErrorKind::Other,
            "host memory quota exhausted",
        ),
        (
            Fault::AllocationDenied,
            io::ErrorKind::PermissionDenied,
            "injected native allocation denial",
        ),
        (
            Fault::ProtectionDenied,
            io::ErrorKind::Other,
            "injected native protection denial",
        ),
        (
            Fault::PartialProtectionDenied,
            io::ErrorKind::Other,
            "injected partial protection denial",
        ),
    ] {
        let legacy = fault.into_io();
        assert_eq!((legacy.kind(), legacy.to_string()), (kind, message.into()));
        assert!(fault.into_module().to_string().contains(message));
    }
}
