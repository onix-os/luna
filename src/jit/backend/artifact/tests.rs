use super::*;
use crate::jit::{compiler_wire, resources::Ledger, work};

mod invocation;
pub(in crate::jit) use invocation::{exercise, exercise_prepared};

const BUILD: [u8; 32] = [0x51; 32];

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(32 << 20))
}
fn limits() -> Limits {
    Limits {
        bytes: 2 << 20,
        modules: 16,
        declarations: 256,
        functions: 128,
        relocations: 8192,
    }
}
fn snapshot(text: &[u8]) -> Snapshot {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, text).unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
    })
}
fn source_packet(snapshot: &Snapshot) -> BudgetVec<u8, BudgetAllocator> {
    compiler_wire::encode(
        snapshot,
        compiler_wire::Limits {
            bytes: 1 << 20,
            operations: 65536,
            constants: 65536,
        },
        allocator(),
    )
    .unwrap()
}

pub(in crate::jit) fn selected(snapshot: &Snapshot) -> Result<Code, CompileError> {
    let metadata = allocator();
    let total = MappingCounter::new(metadata.0.clone());
    let work = work::Limits::from(&crate::JitConfig::default());
    if crate::jit::readonly_lookup::Plan::recognize(snapshot).is_some() {
        crate::jit::readonly_lookup::binding::integer_resume::with_enabled(true, || {
            readonly_lookup::compile_pair(snapshot, total, 4 << 20, metadata, work, Failure::None)
        })
        .0
    } else {
        array::compile_pair(snapshot, total, 4 << 20, metadata, work, Failure::None)
    }
}

fn packet(
    text: &[u8],
) -> (
    BudgetVec<u8, BudgetAllocator>,
    BudgetVec<u8, BudgetAllocator>,
    BudgetVec<u8, BudgetAllocator>,
) {
    let source = snapshot(text);
    let encoded_source = source_packet(&source);
    let (code, bundle) = capture(allocator(), || selected(&source)).unwrap();
    drop(code.unwrap());
    drop(source);
    let environment = environment().unwrap();
    let packet = encode(
        &bundle,
        Binding {
            request: 1,
            build: &BUILD,
            source: &encoded_source,
            environment: &environment,
        },
        limits(),
        allocator(),
    )
    .unwrap();
    (packet, encoded_source, environment)
}

#[test]
fn current_production_routes_roundtrip_without_sender_code_or_arena() {
    mod workloads {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/jit_support/workloads.rs"
        ));
    }
    let mut routes = 0;
    for workload in workloads::WORKLOADS {
        let (packet, source, environment) = packet(workload.source);
        let binding = Binding {
            request: 1,
            build: &BUILD,
            source: &source,
            environment: &environment,
        };
        let output = allocator();
        let ledger = output.0.clone();
        let decoded = decode(&packet, binding, limits(), output).unwrap();
        assert_eq!(
            &*encode(&decoded, binding, limits(), allocator()).unwrap(),
            &*packet
        );
        for module in &decoded.modules {
            routes |= 1 << (module.kind as u8 - 1);
        }
        drop(decoded);
        assert_eq!(ledger.current(), 0);
    }
    assert_eq!(routes, 15);
}

#[test]
fn metadata_and_identity_rejection_precede_owned_allocations() {
    let (packet, source, environment) = packet(b"local t={} for i=1,20 do t[i]=i end return t");
    let binding = Binding {
        request: 1,
        build: &BUILD,
        source: &source,
        environment: &environment,
    };
    for length in 0..packet.len() {
        let output = allocator();
        output.0.fail_after(0);
        let error = decode(&packet[..length], binding, limits(), output.clone())
            .err()
            .unwrap();
        assert_ne!(error, Error::Allocation);
        assert_eq!(output.0.current(), 0);
    }
    for offset in [0, 8, 12, 16, 24, 56, 64, 68, 72, 80] {
        let mut changed = packet.to_vec();
        changed[offset] ^= 255;
        let output = allocator();
        output.0.fail_after(0);
        assert_ne!(
            decode(&changed, binding, limits(), output.clone()).err(),
            Some(Error::Allocation)
        );
        assert!(decode(&changed, binding, limits(), allocator()).is_err());
        assert_eq!(output.0.current(), 0);
    }
    for wrong in [
        Binding {
            request: 0,
            ..binding
        },
        Binding {
            request: 2,
            ..binding
        },
        Binding {
            build: &[0x52; 32],
            ..binding
        },
        Binding {
            source: &[],
            ..binding
        },
        Binding {
            environment: &[],
            ..binding
        },
    ] {
        let output = allocator();
        output.0.fail_after(0);
        assert_eq!(
            decode(&packet, wrong, limits(), output.clone()).err(),
            Some(Error::Identity)
        );
        assert_eq!(output.0.current(), 0);
    }
}

#[test]
fn installed_ordinary_code_matches_original_native_prefixes() {
    let text = b"local sum=0 for i=1,20 do sum=sum+i end return sum";
    let source = snapshot(text);
    let control = selected(&source).unwrap();
    let (packet, bytes, environment) = packet(text);
    let metadata = allocator();
    let ledger = metadata.0.clone();
    let total = MappingCounter::new(ledger.clone());
    let installed = install(
        &packet,
        Binding {
            request: 1,
            build: &BUILD,
            source: &bytes,
            environment: &environment,
        },
        limits(),
        total.clone(),
        4 << 20,
        metadata,
        Failure::None,
    )
    .unwrap();
    assert_eq!(installed.kind(0), Kind::Ordinary);
    let entry: Entry = unsafe { std::mem::transmute(installed.entry(0, 0)) };
    let mut executions = 0;
    for pc in 0..source.operations.len() {
        for budget in [0, 1, 2, 7, 64, u32::MAX] {
            let initial = vec![
                Slot {
                    tag: abi::INTEGER,
                    bits: 3
                };
                source.registers
            ];
            let mut expected = initial.clone();
            let mut actual = initial;
            let a = control.invoke(&mut expected, pc, budget);
            let b = unsafe {
                abi::invoke(entry, actual.as_mut_ptr(), pc, budget, std::ptr::null_mut())
            };
            assert_eq!(
                (a.pc, a.instructions, a.reason),
                (b.pc, b.instructions, b.reason)
            );
            for (a, b) in actual.iter().zip(&expected) {
                assert_eq!((a.tag, a.bits), (b.tag, b.bits));
            }
            executions += b.instructions;
        }
    }
    assert!(executions > 0);
    drop(installed);
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(total.requested(), 0);
    assert_eq!(ledger.current(), 0);
}

#[test]
fn malformed_declarations_relocations_and_caps_fail_before_allocation() {
    let (packet, source, environment) = packet(b"local t={} for i=1,20 do t[i]=i end return t");
    let binding = Binding {
        request: 1,
        build: &BUILD,
        source: &source,
        environment: &environment,
    };
    let decoded = decode(&packet, binding, limits(), allocator()).unwrap();
    let module = 88 + source.len() + environment.len();
    let declarations = module + 16;
    let function = declarations + decoded.modules[0].declarations.len() * 16;
    let reloc = function + 24 + decoded.modules[0].functions[0].bytes.len();
    assert!(!decoded.modules[0].functions[0].relocations.is_empty());
    let mut mutations = Vec::new();
    for (offset, bytes) in [
        (module, vec![255]),
        (module + 4, u32::MAX.to_le_bytes().to_vec()),
        (module + 8, u32::MAX.to_le_bytes().to_vec()),
        (module + 12, vec![1]),
        (declarations, vec![1]),
        (declarations + 1, vec![255]),
        (declarations + 2, vec![3]),
        (declarations + 3, vec![1]),
        (declarations + 4, vec![3]),
        (declarations + 15, vec![1]),
        (declarations + 32, vec![1]),
        (function, u32::MAX.to_le_bytes().to_vec()),
        (function + 4, 3u32.to_le_bytes().to_vec()),
        (function + 8, u64::MAX.to_le_bytes().to_vec()),
        (function + 16, u64::MAX.to_le_bytes().to_vec()),
        (reloc, u32::MAX.to_le_bytes().to_vec()),
        (reloc + 4, vec![255]),
        (reloc + 5, vec![255]),
        (reloc + 6, vec![1]),
        (reloc + 8, u32::MAX.to_le_bytes().to_vec()),
        (reloc + 12, 1u32.to_le_bytes().to_vec()),
        (reloc + 16, i64::MIN.to_le_bytes().to_vec()),
    ] {
        let mut changed = packet.to_vec();
        changed[offset..offset + bytes.len()].copy_from_slice(&bytes);
        mutations.push(changed);
    }
    for changed in mutations {
        let output = allocator();
        output.0.fail_after(0);
        let total = MappingCounter::new(output.0.clone());
        match install(
            &changed,
            binding,
            limits(),
            total.clone(),
            4 << 20,
            output.clone(),
            Failure::None,
        ) {
            Err(InstallError::Packet(error)) => assert_ne!(error, Error::Allocation),
            _ => panic!("malformed artifact reached ownership or installation"),
        }
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(output.0.current(), 0);
        let mut probe = BudgetVec::<u8, _>::new_in(output);
        assert!(probe.try_reserve_exact(1).is_err());
    }
    for small in [
        Limits {
            bytes: packet.len() - 1,
            ..limits()
        },
        Limits {
            modules: 0,
            ..limits()
        },
        Limits {
            declarations: 0,
            ..limits()
        },
        Limits {
            functions: 0,
            ..limits()
        },
        Limits {
            relocations: 0,
            ..limits()
        },
    ] {
        let output = allocator();
        output.0.fail_after(0);
        assert_eq!(
            decode(&packet, binding, small, output.clone()).err(),
            Some(Error::Limit)
        );
        assert_eq!(output.0.current(), 0);
    }
}

#[test]
fn installation_refusals_release_partial_images_and_preserve_external_refusal() {
    let (packet, source, environment) = packet(b"local t={} for i=1,20 do t[i]=i end return t");
    let binding = Binding {
        request: 1,
        build: &BUILD,
        source: &source,
        environment: &environment,
    };
    let output = allocator();
    let total = MappingCounter::new(output.0.clone());
    let complete = install(
        &packet,
        binding,
        limits(),
        total.clone(),
        4 << 20,
        output.clone(),
        Failure::None,
    )
    .unwrap();
    assert!(complete.len() > 1);
    let first_image = complete.mapped_bytes(0);
    assert!(total.load(Ordering::Relaxed) > first_image);
    drop(complete);
    assert_eq!(output.0.current(), 0);
    for (failure, limit) in [
        (Failure::Allocate, 4 << 20),
        (Failure::Protect, 4 << 20),
        (Failure::None, 0),
        (Failure::None, first_image),
    ] {
        let output = allocator();
        let total = MappingCounter::new(output.0.clone());
        match install(
            &packet,
            binding,
            limits(),
            total.clone(),
            limit,
            output.clone(),
            failure,
        ) {
            Err(InstallError::Native(error)) => assert!(matches!(
                error,
                CompileError::ResourceLimit(_) | CompileError::Unavailable(_)
            )),
            _ => panic!("expected typed native refusal"),
        }
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(total.requested(), 0);
        assert_eq!(output.0.current(), 0);
    }
    let mut successes = 0;
    let mut failures = 0;
    for allowance in 0..128 {
        let output = allocator();
        output.0.fail_after(allowance);
        let total = MappingCounter::new(output.0.clone());
        match install(
            &packet,
            binding,
            limits(),
            total.clone(),
            4 << 20,
            output.clone(),
            Failure::None,
        ) {
            Ok(image) => {
                successes += 1;
                drop(image);
            }
            Err(error) => {
                match error {
                    InstallError::Packet(Error::Allocation)
                    | InstallError::Native(CompileError::ResourceLimit(_)) => {}
                    _ => panic!("unexpected allocation refusal: {error:?}"),
                }
                failures += 1;
                let mut probe = BudgetVec::<u8, _>::new_in(output.clone());
                assert!(probe.try_reserve_exact(1).is_err());
            }
        }
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(total.requested(), 0);
        assert_eq!(output.0.current(), 0);
    }
    assert!(failures > 0 && successes > 0);
}

#[test]
fn capture_scopes_restore_nested_and_panicking_work_without_leaks() {
    let source = snapshot(b"local s=0 for i=1,20 do s=s+i end return s");
    let output = allocator();
    let ledger = output.0.clone();
    let ((), outer) = capture(output.clone(), || {
        drop(selected(&source).unwrap());
        let (code, inner) = capture(output.clone(), || selected(&source)).unwrap();
        drop(code.unwrap());
        assert_eq!(inner.modules.len(), 1);
        drop(inner);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = capture(output.clone(), || {
                drop(selected(&source).unwrap());
                panic!("capture cleanup fixture");
            });
        }))
        .is_err());
    })
    .unwrap();
    assert_eq!(outer.modules.len(), 1);
    drop(outer);
    assert!(STATE.with_borrow(Option::is_none));
    assert_eq!(ledger.current(), 0);
}

#[test]
fn complete_parent_target_requests_restore_and_reject_incomplete_features() {
    let features = target_features().unwrap();
    let original = environment().unwrap();
    assert_eq!(
        &*with_target(&features, environment).unwrap().unwrap(),
        &*original
    );
    for invalid in [
        String::new(),
        format!("{features};unknown=true"),
        features.replacen('=', "=invalid", 1),
        "x".repeat(32769),
    ] {
        assert!(with_target(&invalid, || ()).is_err());
    }
    let masked = features
        .split(';')
        .map(|field| {
            if field.ends_with("=true") {
                field.trim_end_matches("true").to_owned() + "false"
            } else {
                field.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(";");
    let changed = with_target(&masked, || {
        let before = environment().unwrap();
        assert_eq!(
            &*with_target(&features, environment).unwrap().unwrap(),
            &*original
        );
        assert_eq!(&*environment().unwrap(), &*before);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_target(&features, || panic!("target restore fixture"));
        }))
        .is_err());
        assert_eq!(&*environment().unwrap(), &*before);
        before
    })
    .unwrap();
    if features.contains("=true") {
        assert_ne!(&*changed, &*original);
    } else {
        assert_eq!(&*changed, &*original);
    }
    assert_eq!(&*environment().unwrap(), &*original);
}
