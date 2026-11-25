use super::*;
use crate::jit::resources::Ledger;

const BUILD: [u8; 32] = [81; 32];
fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(1 << 20))
}
fn limits() -> Limits {
    Limits {
        bytes: 1 << 20,
        modules: 4,
        declarations: 16,
        functions: 8,
        relocations: 64,
    }
}
fn binding() -> Binding<'static> {
    Binding {
        request: 1,
        build: &BUILD,
        source: b"owned-source-fixture",
        environment: b"owned-environment-fixture",
    }
}
fn bundle() -> Bundle {
    let local = Declaration([0, 5, 0, 0, 2, 2, 1, 2, 2, 0, 0, 0, 0, 0, 0, 0]);
    let imported = Declaration([1, 6, 1, 0, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0]);
    let mut functions = BudgetVec::new_in(allocator());
    functions.try_reserve_exact(1).unwrap();
    functions.push(Function {
        id: 0,
        alignment: 16,
        bytes: copy(&[0; 16], allocator()).unwrap(),
        relocations: copy(
            &[Relocation {
                offset: 0,
                kind: 1,
                target_kind: 0,
                index: 1,
                target_offset: 0,
                addend: 0,
            }],
            allocator(),
        )
        .unwrap(),
    });
    let mut modules = BudgetVec::new_in(allocator());
    modules.try_reserve_exact(1).unwrap();
    modules.push(ModuleImage {
        kind: Kind::Ordinary,
        declarations: copy(&[local, imported], allocator()).unwrap(),
        functions,
    });
    Bundle {
        modules,
        environment: copy(binding().environment, allocator()).unwrap(),
    }
}
fn packet() -> BudgetVec<u8, BudgetAllocator> {
    encode(&bundle(), binding(), limits(), allocator()).unwrap()
}

#[test]
fn decoded_artifact_owns_canonical_buffers_after_sender_release() {
    let encoded = packet();
    let output = allocator();
    let ledger = output.0.clone();
    let decoded = decode(&encoded, binding(), limits(), output).unwrap();
    drop(encoded);
    assert!(ledger.current() > 0);
    let reencoded = encode(&decoded, binding(), limits(), allocator()).unwrap();
    assert_eq!(&*reencoded, &*packet());
    drop(decoded);
    assert_eq!(ledger.current(), 0);
}

#[test]
fn every_truncation_and_byte_mutation_is_total_and_canonical() {
    let packet = packet();
    let output = allocator();
    let ledger = output.0.clone();
    for length in 0..packet.len() {
        assert!(decode(&packet[..length], binding(), limits(), output.clone()).is_err());
        assert_eq!(ledger.current(), 0);
    }
    for index in 0..packet.len() {
        for byte in [0, 1, 127, 255] {
            let mut changed = packet.to_vec();
            changed[index] = byte;
            if let Ok(decoded) = decode(&changed, binding(), limits(), output.clone()) {
                assert_eq!(
                    &*encode(&decoded, binding(), limits(), allocator()).unwrap(),
                    &*changed
                );
            }
            assert_eq!(ledger.current(), 0);
        }
    }
}

#[test]
fn corrupt_records_and_identity_refuse_before_ownership() {
    let packet = packet();
    let module = HEADER + binding().source.len() + binding().environment.len();
    let function = module + 16 + 32;
    let reloc = function + 24 + 16;
    for offset in [
        0,
        8,
        12,
        16,
        24,
        56,
        64,
        68,
        72,
        80,
        module,
        module + 4,
        module + 8,
        module + 12,
        module + 16 + 3,
        function,
        function + 4,
        function + 8,
        function + 16,
        reloc,
        reloc + 4,
        reloc + 5,
        reloc + 6,
        reloc + 8,
        reloc + 12,
        reloc + 16,
    ] {
        let mut changed = packet.to_vec();
        changed[offset] = 255;
        let output = allocator();
        output.0.fail_after(0);
        let error = decode(&changed, binding(), limits(), output.clone())
            .err()
            .unwrap();
        assert_ne!(error, Error::Allocation);
        assert_eq!(output.0.current(), 0);
    }
}

#[test]
fn exact_caps_and_each_allocation_refusal_reclaim_owned_storage() {
    let packet = packet();
    let exact = Limits {
        bytes: packet.len(),
        ..limits()
    };
    assert!(decode(&packet, binding(), exact, allocator()).is_ok());
    assert_eq!(
        decode(
            &packet,
            binding(),
            Limits {
                bytes: packet.len() - 1,
                ..limits()
            },
            allocator()
        )
        .err(),
        Some(Error::Limit)
    );
    let mut failed = 0;
    let mut passed = 0;
    for allowance in 0..16 {
        let output = allocator();
        output.0.fail_after(allowance);
        match decode(&packet, binding(), limits(), output.clone()) {
            Ok(decoded) => {
                drop(decoded);
                passed += 1;
            }
            Err(error) => {
                assert_eq!(error, Error::Allocation);
                failed += 1;
                let mut probe = BudgetVec::<u8, _>::new_in(output.clone());
                assert!(probe.try_reserve_exact(1).is_err());
            }
        }
        assert_eq!(output.0.current(), 0);
    }
    assert!(failed > 0 && passed > 0);
}

#[test]
fn relocation_spans_and_module_combinations_cannot_overflow_linker_ranges() {
    let limit = if cfg!(target_arch = "aarch64") {
        (1 << 27) - 4
    } else {
        i32::MAX as usize
    };
    assert!(branch_span(limit - 7, 0));
    assert!(!branch_span(limit - 6, 0));
    assert!(!branch_span(usize::MAX, 0));
    assert!(!branch_span(1, usize::MAX));
    let packet = packet();
    let start = HEADER + binding().source.len() + binding().environment.len();
    let mut doubled = packet.to_vec();
    doubled.extend_from_slice(&packet[start..]);
    let length = doubled.len() as u64;
    doubled[56..64].copy_from_slice(&length.to_le_bytes());
    doubled[80..88].copy_from_slice(&2u64.to_le_bytes());
    let output = allocator();
    output.0.fail_after(0);
    assert_eq!(
        decode(&doubled, binding(), limits(), output.clone()).err(),
        Some(Error::Module)
    );
    assert_eq!(output.0.current(), 0);
}
