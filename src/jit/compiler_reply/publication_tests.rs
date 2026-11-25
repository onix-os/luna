use super::*;
use crate::jit::resources::Ledger;

fn stages() -> [Stage; 4] {
    [
        Stage::Snapshot,
        Stage::Target,
        Stage::Compiler,
        Stage::Artifact,
    ]
}

#[test]
fn every_class_and_stage_preserves_public_category_and_fixed_label() {
    for class in [
        Class::Resource,
        Class::Unavailable,
        Class::Rejected,
        Class::Configuration,
        Class::Compilation,
    ] {
        for stage in stages() {
            let ledger = Ledger::new(32);
            if class != Class::Compilation {
                ledger.fail_after(0);
            }
            let fault = Fault { class, stage };
            assert!(fault.label().len() <= 32);
            let error = Failure::Worker(fault).into_owned(BudgetAllocator(ledger.clone()));
            assert_eq!(
                Fault::classify(&error, stage).class,
                if class == Class::Rejected {
                    Class::Unavailable
                } else {
                    class
                }
            );
            let label = match &error {
                CompileError::Compilation(text) => text.as_str(),
                CompileError::ResourceLimit(text)
                | CompileError::Unavailable(text)
                | CompileError::InvalidConfiguration(text) => *text,
                CompileError::Indexed { .. } => panic!("indexed worker label"),
            };
            assert_eq!(label, fault.label());
            assert_eq!(
                ledger.current(),
                if class == Class::Compilation {
                    fault.label().len()
                } else {
                    0
                }
            );
            let published = error.into_public(BudgetAllocator(ledger.clone()));
            assert_eq!(ledger.current(), 0);
            drop(published);
            assert_eq!(
                ledger.peak(),
                if class == Class::Compilation {
                    fault.label().len()
                } else {
                    0
                }
            );
        }
    }
}

#[test]
fn publication_denial_is_a_fixed_resource_error_and_recovers_explicitly() {
    for stage in stages() {
        let fault = Fault {
            class: Class::Compilation,
            stage,
        };
        for underlying in [false, true] {
            let ledger = Ledger::new(if underlying {
                32
            } else {
                fault.label().len() - 1
            });
            if underlying {
                ledger.fail_after(0);
            }
            for _ in 0..2 {
                assert!(matches!(
                    Failure::Worker(fault).into_owned(BudgetAllocator(ledger.clone())),
                    CompileError::ResourceLimit("compiler error publication")
                ));
                assert_eq!(ledger.current(), 0);
            }
            ledger.set_limit(32);
            ledger.fail_after(usize::MAX);
            assert!(
                matches!(Failure::Worker(fault).into_owned(BudgetAllocator(ledger.clone())),
                CompileError::Compilation(text) if text == fault.label())
            );
            assert_eq!(ledger.current(), 0);
        }
    }
}

#[test]
fn protocol_refusals_never_allocate_or_become_worker_compilation_errors() {
    let ledger = Ledger::new(0);
    ledger.fail_after(0);
    for fault in [
        Error::Header,
        Error::Limit,
        Error::Identity,
        Error::Checksum,
        Error::Outcome,
    ] {
        assert!(matches!(
            Failure::Protocol(fault).into_owned(BudgetAllocator(ledger.clone())),
            CompileError::Unavailable("compiler response protocol")
        ));
    }
    assert_eq!(ledger.peak(), 0);
    assert_eq!(ledger.current(), 0);
}

#[test]
fn authenticated_failure_storage_is_reclaimed_before_publication_and_corruption_stays_protocol() {
    for corrupt in [false, true] {
        let wire = Ledger::new(HEADER);
        let publication = Ledger::new(32);
        let fault = Fault {
            class: Class::Compilation,
            stage: Stage::Compiler,
        };
        let mut bytes = Vec::new_in(BudgetAllocator(wire.clone()));
        bytes.try_reserve_exact(HEADER).unwrap();
        bytes.extend_from_slice(&header(Outcome::Failure(fault), 42, &[7; 32]).unwrap());
        if corrupt {
            bytes[72] ^= 1;
        }
        let failure = Artifact::new(bytes, 42, &[7; 32]).err().unwrap();
        assert_eq!(wire.current(), 0);
        publication.fail_after(0);
        let error = failure.into_owned(BudgetAllocator(publication.clone()));
        if corrupt {
            assert!(matches!(
                error,
                CompileError::Unavailable("compiler response protocol")
            ));
        } else {
            assert!(matches!(
                error,
                CompileError::ResourceLimit("compiler error publication")
            ));
        }
        assert_eq!(publication.current(), 0);
    }
}
