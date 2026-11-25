use super::*;
use crate::jit::resources::Ledger;

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(4 << 20))
}

fn features() -> String {
    #[cfg(target_arch = "x86_64")]
    let names = [
        "has_sse3",
        "has_ssse3",
        "has_cmpxchg16b",
        "has_sse41",
        "has_sse42",
        "has_avx",
        "has_avx2",
        "has_fma",
        "has_avx_vnni",
        "has_avx512bitalg",
        "has_avx512dq",
        "has_avx512vl",
        "has_avx512vbmi",
        "has_avx512f",
        "has_avx512vnni",
        "has_popcnt",
        "has_bmi1",
        "has_bmi2",
        "has_lzcnt",
    ];
    #[cfg(target_arch = "aarch64")]
    let names = [
        "has_lse",
        "has_pauth",
        "has_fp16",
        "has_dotprod",
        "has_i8mm",
        "sign_return_address_all",
        "sign_return_address",
        "sign_return_address_with_bkey",
        "use_bti",
        "use_csdb",
    ];
    names.map(|name| format!("{name}=false")).join(";")
}

fn fixture(run: impl FnOnce(View<'_>)) {
    let features = features();
    let environment = artifact::with_target(&features, || artifact::environment_in(allocator()))
        .unwrap()
        .unwrap();
    run(View {
        request: 17,
        worker: &[0x12; 32],
        abi: &[0x34; 32],
        work: work::Limits {
            instructions: 1024,
            blocks: 64,
            relocations: 32,
        },
        route: Route::Ordinary,
        resume: false,
        features: &features,
        environment: &environment,
        source: b"owned-source-bytes",
    });
}

fn reseal(bytes: &mut [u8]) {
    let checksum = fingerprint(bytes);
    bytes[152..184].copy_from_slice(&checksum);
}

#[test]
fn all_routes_resume_and_zero_work_roundtrip_without_decoder_ownership() {
    fixture(|mut view| {
        for route in [Route::Ordinary, Route::Arrays, Route::Readonly] {
            for resume in [false, true] {
                view.route = route;
                view.resume = resume;
                let bytes = encode(&view, allocator()).unwrap();
                let decoded = decode(&bytes, view.worker, view.abi).unwrap();
                assert_eq!(decoded.route, route);
                assert_eq!(decoded.resume, resume);
                assert_eq!(decoded.request, view.request);
                assert_eq!(decoded.features, view.features);
                assert_eq!(decoded.environment, view.environment);
                assert_eq!(decoded.source, view.source);
                assert_eq!(decoded.work.instructions, 1024);
                assert!(std::ptr::eq(
                    decoded.source.as_ptr(),
                    bytes[bytes.len() - view.source.len()..].as_ptr()
                ));
                assert_eq!(binding(&bytes).unwrap(), fingerprint(&bytes));
            }
        }
        view.work = work::Limits {
            instructions: 0,
            blocks: 0,
            relocations: 0,
        };
        let bytes = encode(&view, allocator()).unwrap();
        assert_eq!(
            decode(&bytes, view.worker, view.abi)
                .unwrap()
                .work
                .instructions,
            0
        );
    });
}

#[test]
fn malformed_headers_lengths_and_reserved_words_are_rejected() {
    fixture(|view| {
        let original = encode(&view, allocator()).unwrap();
        for length in [
            0,
            7,
            8,
            12,
            24,
            96,
            152,
            183,
            184,
            191,
            192,
            original.len() - 1,
        ] {
            assert!(decode(&original[..length], view.worker, view.abi).is_err());
        }
        for (offset, value) in [(0, 1), (8, 2), (12, 1), (16, 0), (24, 0), (184, 1)] {
            let mut bytes = original.clone();
            bytes[offset] = value;
            reseal(&mut bytes);
            assert!(decode(&bytes, view.worker, view.abi).is_err());
        }
        for offset in [128, 136, 144] {
            let mut bytes = original.clone();
            bytes[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert!(decode(&bytes, view.worker, view.abi).is_err());
        }
        let mut extra = original.clone();
        extra.push(0);
        assert!(decode(&extra, view.worker, view.abi).is_err());
        assert_eq!(binding(&[]), Err(Error::Header));
    });
}

#[test]
fn checksum_binds_every_compilation_field_and_expected_identity() {
    fixture(|view| {
        let original = encode(&view, allocator()).unwrap();
        assert!(matches!(
            decode(&original, &[0; 32], view.abi),
            Err(Error::Identity)
        ));
        assert!(matches!(
            decode(&original, view.worker, &[0; 32]),
            Err(Error::Identity)
        ));
        for offset in [24, 96, 104, 112, 120, 124, 152, original.len() - 1] {
            let mut changed = original.clone();
            changed[offset] ^= if offset == 120 { 3 } else { 1 };
            assert!(decode(&changed, view.worker, view.abi).is_err());
        }
        for (offset, value) in [(96, 2048u64), (104, 32), (112, 16)] {
            let mut changed = original.clone();
            changed[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            reseal(&mut changed);
            assert!(decode(&changed, view.worker, view.abi).is_ok());
            assert_ne!(binding(&original).unwrap(), binding(&changed).unwrap());
        }
    });
}

#[test]
fn resealed_noncanonical_targets_routes_bools_and_work_are_rejected() {
    fixture(|view| {
        let original = encode(&view, allocator()).unwrap();
        for (offset, value) in [(120, 0u32), (120, 4), (124, 2)] {
            let mut bytes = original.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            reseal(&mut bytes);
            assert!(decode(&bytes, view.worker, view.abi).is_err());
        }
        for (offset, value) in [(96, (1u64 << 20) + 1), (104, 65537), (112, 65537)] {
            let mut bytes = original.clone();
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            reseal(&mut bytes);
            assert!(matches!(
                decode(&bytes, view.worker, view.abi),
                Err(Error::Limit)
            ));
        }
        for offset in [
            HEADER,
            HEADER + view.features.len(),
            original.len() - view.source.len() - 1,
        ] {
            let mut bytes = original.clone();
            bytes[offset] ^= 1;
            reseal(&mut bytes);
            assert!(matches!(
                decode(&bytes, view.worker, view.abi),
                Err(Error::Identity)
            ));
        }
    });
}

#[test]
fn one_fallible_allocation_reclaims_quota_and_keeps_persistent_denial() {
    fixture(|mut view| {
        for allowance in [0, 1] {
            let output = allocator();
            output.0.fail_after(allowance);
            match encode(&view, output.clone()) {
                Ok(bytes) => {
                    assert_eq!(allowance, 1);
                    assert_eq!(output.0.current(), bytes.capacity());
                    assert!(decode(&bytes, view.worker, view.abi).is_ok());
                    drop(bytes);
                }
                Err(error) => {
                    assert_eq!(allowance, 0);
                    assert_eq!(error, Error::Allocation);
                }
            }
            assert_eq!(output.0.current(), 0);
            assert!(Vec::<u8, _>::new_in(output).try_reserve_exact(1).is_err());
        }
        let denied = BudgetAllocator(Ledger::new(1));
        assert!(matches!(
            encode(&view, denied.clone()),
            Err(Error::Allocation)
        ));
        assert_eq!(denied.0.current(), 0);
        view.request = 0;
        assert!(matches!(encode(&view, allocator()), Err(Error::Header)));
        view.request = 1;
        view.work.instructions = usize::MAX;
        assert!(matches!(encode(&view, allocator()), Err(Error::Limit)));
    });
}

#[test]
fn owned_packet_survives_sender_release_and_thread_handoff() {
    let output = allocator();
    let ledger = output.0.clone();
    let mut packet = None;
    fixture(|view| packet = Some(encode(&view, output).unwrap()));
    let packet = packet.unwrap();
    std::thread::spawn(move || {
        let view = decode(&packet, &[0x12; 32], &[0x34; 32]).unwrap();
        assert_eq!(view.source, b"owned-source-bytes");
    })
    .join()
    .unwrap();
    assert_eq!(ledger.current(), 0);
}

#[test]
fn sha256_vectors_and_fragmented_streams_are_stable() {
    assert_eq!(
        &Sha256::digest(b"abc")[..],
        &[
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ]
    );
    struct Fragment<'a>(&'a [u8]);
    impl std::io::Read for Fragment<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let count = self.0.len().min(1).min(out.len());
            out[..count].copy_from_slice(&self.0[..count]);
            self.0 = &self.0[count..];
            Ok(count)
        }
    }
    let contiguous = image_identity(&b"abc"[..], 3).unwrap();
    assert_eq!(contiguous, image_identity(Fragment(b"abc"), 3).unwrap());
    assert_ne!(contiguous, image_identity(&b"abd"[..], 3).unwrap());
}

#[test]
fn streaming_image_bounds_and_io_failures_are_fixed_faults() {
    assert_eq!(image_identity(&b""[..], 1), Err(Error::Identity));
    assert_eq!(image_identity(&b"abc"[..], 2), Err(Error::Limit));
    struct Interrupted(bool);
    impl std::io::Read for Interrupted {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let kind = if std::mem::replace(&mut self.0, false) {
                std::io::ErrorKind::Interrupted
            } else {
                std::io::ErrorKind::BrokenPipe
            };
            Err(kind.into())
        }
    }
    assert_eq!(image_identity(Interrupted(true), 100), Err(Error::Io));
}

#[cfg(not(miri))]
#[test]
fn every_truncation_and_single_byte_mutation_is_rejected() {
    let source = std::vec![0u8; SOURCE + 1];
    fixture(|view| {
        let original = encode(&view, allocator()).unwrap();
        for length in 0..original.len() {
            assert!(decode(&original[..length], view.worker, view.abi).is_err());
        }
        for offset in 0..original.len() {
            let mut bytes = original.clone();
            bytes[offset] ^= 1;
            assert!(
                decode(&bytes, view.worker, view.abi).is_err(),
                "offset={offset}"
            );
        }
        let mut view = View {
            source: &source[..SOURCE],
            ..view
        };
        let bytes = encode(&view, allocator()).unwrap();
        assert_eq!(
            decode(&bytes, view.worker, view.abi).unwrap().source.len(),
            SOURCE
        );
        view.source = &source;
        assert!(matches!(encode(&view, allocator()), Err(Error::Limit)));
        let oversized = std::vec![0u8; MAX_BYTES + 1];
        assert!(matches!(
            decode(&oversized, view.worker, view.abi),
            Err(Error::Limit)
        ));
    });
}

#[cfg(not(miri))]
#[test]
fn loaded_worker_image_and_abi_fingerprints_are_stable() {
    let first = worker_identity().unwrap();
    assert_ne!(first, [0; 32]);
    assert_eq!(first, worker_identity().unwrap());
    assert_eq!(
        first,
        image_identity(std::fs::File::open("/proc/self/exe").unwrap(), 512 << 20).unwrap()
    );
    let abi = abi_identity();
    assert_ne!(abi, [0; 32]);
    assert_eq!(abi, abi_identity());
    assert_ne!(abi, first);
}
