use super::*;
use crate::jit::resources::Ledger;

const REQUEST: u64 = 19;
const BINDING: [u8; 32] = [0x75; 32];

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(4 << 20))
}

fn packet(outcome: Outcome<'_>, output: BudgetAllocator) -> Vec<u8, BudgetAllocator> {
    let head = header(outcome, REQUEST, &BINDING).unwrap();
    let payload = outcome.into_result().unwrap_or(&[]);
    let mut bytes = Vec::new_in(output);
    bytes.try_reserve_exact(HEADER + payload.len()).unwrap();
    bytes.extend_from_slice(&head);
    bytes.extend_from_slice(payload);
    bytes
}

fn reseal(bytes: &mut [u8]) {
    let digest = checksum(bytes, &bytes[HEADER..]);
    bytes[72..104].copy_from_slice(&digest);
}

#[test]
fn success_views_and_owned_artifacts_reuse_the_received_allocation() {
    let output = allocator();
    let bytes = packet(Outcome::Artifact(b"native-code"), output.clone());
    let capacity = bytes.capacity();
    let address = bytes[HEADER..].as_ptr();
    let view = decode(&bytes, REQUEST, &BINDING)
        .unwrap()
        .into_result()
        .unwrap();
    assert_eq!(view, b"native-code");
    assert_eq!(view.as_ptr(), address);
    output.0.fail_after(0);
    let artifact =
        Artifact::new(bytes, REQUEST, &BINDING).unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(&*artifact, b"native-code");
    assert_eq!(artifact.as_ptr(), address);
    assert_eq!(output.0.current(), capacity);
    drop(artifact);
    assert_eq!(output.0.current(), 0);
    assert!(Vec::<u8, _>::new_in(output).try_reserve_exact(1).is_err());
}

#[test]
fn every_fault_class_and_stage_is_a_fixed_header_only_reply() {
    for class in [
        Class::Unavailable,
        Class::Resource,
        Class::Compilation,
        Class::Configuration,
        Class::Rejected,
    ] {
        for stage in [
            Stage::Snapshot,
            Stage::Target,
            Stage::Compiler,
            Stage::Artifact,
        ] {
            let fault = Fault { class, stage };
            let bytes = header(Outcome::Failure(fault), REQUEST, &BINDING).unwrap();
            assert_eq!(bytes.len(), HEADER);
            assert_eq!(
                decode(&bytes, REQUEST, &BINDING),
                Ok(Outcome::Failure(fault))
            );
            assert!(!fault.label().is_empty());
        }
    }
}

#[test]
fn malformed_headers_lengths_identity_and_checksums_are_rejected() {
    let original = packet(Outcome::Artifact(b"compiled"), allocator());
    for length in [
        0,
        7,
        8,
        12,
        24,
        64,
        72,
        103,
        104,
        111,
        112,
        original.len() - 1,
    ] {
        assert!(decode(&original[..length], REQUEST, &BINDING).is_err());
    }
    assert_eq!(decode(&original, 0, &BINDING), Err(Error::Identity));
    assert_eq!(
        decode(&original, REQUEST + 1, &BINDING),
        Err(Error::Identity)
    );
    assert_eq!(decode(&original, REQUEST, &[0; 32]), Err(Error::Identity));
    for offset in [0, 8, 16, 24, 32, 64, 72, 108, original.len() - 1] {
        let mut bytes = original.clone();
        bytes[offset] ^= 1;
        assert!(
            decode(&bytes, REQUEST, &BINDING).is_err(),
            "offset={offset}"
        );
    }
    for offset in [16, 64] {
        let mut bytes = original.clone();
        bytes[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        reseal(&mut bytes);
        assert!(decode(&bytes, REQUEST, &BINDING).is_err());
    }
    let mut extra = original.clone();
    extra.push(0);
    assert!(decode(&extra, REQUEST, &BINDING).is_err());
}

#[test]
fn resealed_unknown_or_inconsistent_outcomes_cannot_publish_payloads() {
    let original = packet(Outcome::Artifact(b"code"), allocator());
    for (offset, value) in [(12, 1u32), (12, 6), (12, u32::MAX), (104, 1), (108, 1)] {
        let mut bytes = original.clone();
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        reseal(&mut bytes);
        assert!(decode(&bytes, REQUEST, &BINDING).is_err());
    }
    let original = header(
        Outcome::Failure(Fault {
            class: Class::Resource,
            stage: Stage::Compiler,
        }),
        REQUEST,
        &BINDING,
    )
    .unwrap();
    for (offset, value) in [(12, 0u32), (104, 0), (104, 5), (104, u32::MAX)] {
        let mut bytes = original;
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        reseal(&mut bytes);
        assert!(decode(&bytes, REQUEST, &BINDING).is_err());
    }
    assert_eq!(
        header(Outcome::Artifact(&[]), REQUEST, &BINDING),
        Err(Error::Outcome)
    );
    assert_eq!(
        header(Outcome::Artifact(b"x"), 0, &BINDING),
        Err(Error::Identity)
    );
}

#[test]
fn owned_failure_and_corruption_reclaim_storage_without_resetting_denial() {
    for corrupt in [false, true] {
        let output = allocator();
        let fault = Fault {
            class: Class::Resource,
            stage: Stage::Compiler,
        };
        let mut bytes = packet(Outcome::Failure(fault), output.clone());
        if corrupt {
            bytes[72] ^= 1;
        }
        output.0.fail_after(0);
        assert_eq!(
            Artifact::new(bytes, REQUEST, &BINDING).err().unwrap(),
            if corrupt {
                Failure::Protocol(Error::Checksum)
            } else {
                Failure::Worker(fault)
            }
        );
        assert_eq!(output.0.current(), 0);
        assert!(Vec::<u8, _>::new_in(output).try_reserve_exact(1).is_err());
    }
}

#[test]
fn worker_error_classification_preserves_public_categories_without_text_transport() {
    for (error, class) in [
        (CompileError::Unavailable("unsupported"), Class::Unavailable),
        (CompileError::ResourceLimit("budget"), Class::Resource),
        (
            CompileError::InvalidConfiguration("config"),
            Class::Configuration,
        ),
        (
            CompileError::Compilation("compiler detail".into()),
            Class::Compilation,
        ),
    ] {
        let fault = Fault::classify(&error, Stage::Compiler);
        assert_eq!(fault.class, class);
        assert_eq!(fault.stage, Stage::Compiler);
        let bytes = header(Outcome::Failure(fault), REQUEST, &BINDING).unwrap();
        assert_eq!(
            decode(&bytes, REQUEST, &BINDING).unwrap().into_result(),
            Err(fault)
        );
    }
}

#[test]
fn received_artifact_outlives_sender_buffers_and_thread_handoff() {
    let output = allocator();
    let ledger = output.0.clone();
    let bytes = {
        let source = std::vec![1, 2, 3, 4];
        packet(Outcome::Artifact(&source), output)
    };
    let artifact =
        Artifact::new(bytes, REQUEST, &BINDING).unwrap_or_else(|error| panic!("{error:?}"));
    std::thread::spawn(move || assert_eq!(&*artifact, &[1, 2, 3, 4]))
        .join()
        .unwrap();
    assert_eq!(ledger.current(), 0);
}

#[cfg(not(miri))]
#[test]
fn exhaustive_mutation_truncation_and_exact_payload_caps_are_enforced() {
    let original = packet(Outcome::Artifact(b"code"), allocator());
    for length in 0..original.len() {
        assert!(decode(&original[..length], REQUEST, &BINDING).is_err());
    }
    for offset in 0..original.len() {
        let mut bytes = original.clone();
        bytes[offset] ^= 1;
        assert!(decode(&bytes, REQUEST, &BINDING).is_err());
    }
    let payload = std::vec![0; PAYLOAD+1];
    let bytes = packet(Outcome::Artifact(&payload[..PAYLOAD]), allocator());
    assert_eq!(bytes.len(), MAX_BYTES);
    assert_eq!(
        decode(&bytes, REQUEST, &BINDING)
            .unwrap()
            .into_result()
            .unwrap()
            .len(),
        PAYLOAD
    );
    assert_eq!(
        header(Outcome::Artifact(&payload), REQUEST, &BINDING),
        Err(Error::Limit)
    );
    let oversized = std::vec![0; MAX_BYTES+1];
    assert_eq!(decode(&oversized, REQUEST, &BINDING), Err(Error::Limit));
}
