use super::*;
use crate::jit::{
    resources::{Ledger, MappingCounter},
    work,
};

const WORKER: [u8; 32] = [0x42; 32];
const ABI: [u8; 32] = [0x83; 32];
const ID: u64 = 29;
const SOURCE: &[u8] = b"local sum=0 for i=1,20 do sum=sum+host_increment(i) end return sum";

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(32 << 20))
}

fn limits() -> Limits {
    Limits {
        source: wire::Limits {
            bytes: 1 << 20,
            operations: 65536,
            constants: 65536,
        },
        artifact: artifact::Limits {
            bytes: 2 << 20,
            modules: 16,
            declarations: 256,
            functions: 128,
            relocations: 8192,
        },
    }
}

fn allocators() -> Allocators {
    Allocators {
        source: allocator(),
        metadata: allocator(),
        objects: allocator(),
        output: allocator(),
    }
}

fn budgets(allocators: &Allocators) -> [BudgetAllocator; 4] {
    [
        allocators.source.clone(),
        allocators.metadata.clone(),
        allocators.objects.clone(),
        allocators.output.clone(),
    ]
}

struct Input {
    packet: Vec<u8, BudgetAllocator>,
    source: Vec<u8, BudgetAllocator>,
    environment: Vec<u8, BudgetAllocator>,
    build: [u8; 32],
}

fn input(source: &[u8], route: request::Route) -> Input {
    let source = crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, source).unwrap();
        let snapshot = crate::jit::ir::Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
        wire::encode(&snapshot, limits().source, allocator()).unwrap()
    });
    let environment = artifact::environment_in(allocator()).unwrap();
    let features = artifact::target_features().unwrap();
    let packet = request::encode(
        &request::View {
            request: ID,
            worker: &WORKER,
            abi: &ABI,
            work: work::Limits::from(&crate::JitConfig::default()),
            route,
            resume: true,
            features: &features,
            environment: &environment,
            source: &source,
        },
        allocator(),
    )
    .unwrap();
    let build = request::binding(&packet).unwrap();
    Input {
        packet,
        source,
        environment,
        build,
    }
}

fn outcome(response: &Response, build: &[u8; 32]) -> Result<(), reply::Fault> {
    let mut bytes = response.header().to_vec();
    bytes.extend_from_slice(response.payload());
    reply::decode(&bytes, ID, build)
        .unwrap()
        .into_result()
        .map(|_| ())
}

#[test]
fn owned_service_releases_input_and_executes_all_native_entry_kinds() {
    let mut routes = 0;
    let mut calls = 0;
    for (source, route) in [
        (
            &b"local s=0 for i=1,20 do s=s+i end return s"[..],
            request::Route::Ordinary,
        ),
        (
            &b"local t={} for i=1,20 do t[i]=i end local s=0 for i=1,20 do s=s+t[i] end return s"[..],
            request::Route::Arrays,
        ),
        (SOURCE, request::Route::Readonly),
    ] {
        let input = input(source, route);
        let input_ledger = input.packet.allocator().0.clone();
        let allocators = allocators();
        let ledgers = budgets(&allocators);
        let response = handle(input.packet, &WORKER, &ABI, limits(), allocators).unwrap();
        assert_eq!(input_ledger.current(), 0);
        assert_eq!(outcome(&response, &input.build), Ok(()));
        for ledger in &ledgers[..3] {
            assert_eq!(ledger.0.current(), 0);
        }
        assert!(ledgers[3].0.current() > 0);
        let total = MappingCounter::new(Ledger::new(4 << 20));
        let installed = artifact::install(
            response.payload(),
            artifact::Binding {
                request: ID,
                build: &input.build,
                source: &input.source,
                environment: &input.environment,
            },
            limits().artifact,
            total.clone(),
            4 << 20,
            allocator(),
            crate::jit::backend::Failure::None,
        )
        .unwrap();
        for module in 0..installed.len() {
            routes |= 1 << (installed.kind(module) as u8 - 1);
        }
        let snapshot = wire::decode(&input.source, limits().source, allocator()).unwrap();
        drop(input.source);
        drop(input.environment);
        drop(response);
        for ledger in &ledgers {
            assert_eq!(ledger.0.current(), 0);
        }
        calls += artifact::exercise(&installed, &snapshot).0;
        drop(installed);
        assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(total.requested(), 0);
    }
    assert_eq!(routes, 15);
    assert!(calls > 0);
}

#[test]
fn allocation_prefix_refusals_are_typed_reclaimed_and_persistent_at_every_stage() {
    for stage in 0..4 {
        let mut refused = 0;
        let mut completed = 0;
        let mut clean_success = false;
        for allowance in 0..=128 {
            let input = input(SOURCE, request::Route::Readonly);
            let expected_stage = if stage == 0 {
                let probe = allocator();
                probe.0.fail_after(allowance);
                let decoded = wire::decode(&input.source, limits().source, probe.clone());
                let phase = match decoded {
                    Err(wire::Error::Allocation) => reply::Stage::Snapshot,
                    Ok(snapshot) => {
                        drop(snapshot);
                        reply::Stage::Compiler
                    }
                    Err(error) => panic!("unexpected source probe {error:?}"),
                };
                assert_eq!(probe.0.current(), 0);
                phase
            } else if stage == 3 {
                reply::Stage::Artifact
            } else {
                reply::Stage::Compiler
            };
            let input_ledger = input.packet.allocator().0.clone();
            let allocators = allocators();
            let ledgers = budgets(&allocators);
            ledgers[stage].0.fail_after(allowance);
            let response = handle(input.packet, &WORKER, &ABI, limits(), allocators).unwrap();
            assert_eq!(input_ledger.current(), 0);
            match outcome(&response, &input.build) {
                Ok(()) => {
                    completed += 1;
                    clean_success |= ledgers[stage].0.refusals() == 0;
                }
                Err(fault) => {
                    refused += 1;
                    assert_eq!(fault.class, reply::Class::Resource);
                    assert_eq!(fault.stage, expected_stage);
                    assert!(response.payload().is_empty());
                }
            }
            drop(response);
            for ledger in &ledgers {
                assert_eq!(ledger.0.current(), 0);
            }
            if ledgers[stage].0.refusals() > 0 {
                assert!(Vec::<u8, _>::new_in(ledgers[stage].clone())
                    .try_reserve_exact(1)
                    .is_err());
            }
        }
        assert!(
            refused > 0 && completed > 0 && clean_success,
            "stage={stage}"
        );
    }
}

#[test]
fn byte_quota_refusals_release_all_service_stages() {
    for stage in 0..4 {
        let input = input(SOURCE, request::Route::Readonly);
        let allocators = allocators();
        let ledgers = budgets(&allocators);
        ledgers[stage].0.set_limit(1);
        let response = handle(input.packet, &WORKER, &ABI, limits(), allocators).unwrap();
        let fault = outcome(&response, &input.build).unwrap_err();
        assert_eq!(fault.class, reply::Class::Resource);
        assert_eq!(
            fault.stage,
            match stage {
                0 => reply::Stage::Snapshot,
                3 => reply::Stage::Artifact,
                _ => reply::Stage::Compiler,
            }
        );
        drop(response);
        for ledger in &ledgers {
            assert_eq!(ledger.0.current(), 0);
        }
    }
}

#[test]
fn invalid_requests_and_service_configuration_precede_compiler_allocations() {
    for configuration in [false, true] {
        let mut input = input(SOURCE, request::Route::Readonly);
        let input_ledger = input.packet.allocator().0.clone();
        let mut limits = limits();
        if configuration {
            limits.artifact.bytes += 1;
        } else {
            input.packet[0] ^= 1;
        }
        let allocators = allocators();
        let ledgers = budgets(&allocators);
        for ledger in &ledgers {
            ledger.0.fail_after(0);
        }
        let error = handle(input.packet, &WORKER, &ABI, limits, allocators)
            .err()
            .unwrap();
        assert_eq!(
            error,
            if configuration {
                Error::Configuration
            } else {
                Error::Request(request::Error::Header)
            }
        );
        assert_eq!(input_ledger.current(), 0);
        for ledger in &ledgers {
            assert_eq!(ledger.0.current(), 0);
            assert_eq!(ledger.0.refusals(), 0);
        }
    }
}

#[test]
fn service_compiles_only_owned_input_on_another_thread() {
    let input = input(SOURCE, request::Route::Readonly);
    let input_ledger = input.packet.allocator().0.clone();
    let allocators = allocators();
    let ledgers = budgets(&allocators);
    let response = std::thread::spawn(move || {
        handle(input.packet, &WORKER, &ABI, limits(), allocators).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(input_ledger.current(), 0);
    assert_eq!(outcome(&response, &input.build), Ok(()));
    drop(response);
    for ledger in &ledgers {
        assert_eq!(ledger.0.current(), 0);
    }
}

#[test]
fn source_and_artifact_policy_limits_return_bound_resource_outcomes() {
    for stage in 0..2 {
        let input = input(SOURCE, request::Route::Readonly);
        let mut limits = limits();
        if stage == 0 {
            limits.source.operations = 0;
        } else {
            limits.artifact.bytes = 0;
        }
        let allocators = allocators();
        let ledgers = budgets(&allocators);
        let response = handle(input.packet, &WORKER, &ABI, limits, allocators).unwrap();
        assert_eq!(
            outcome(&response, &input.build),
            Err(reply::Fault {
                class: reply::Class::Resource,
                stage: if stage == 0 {
                    reply::Stage::Snapshot
                } else {
                    reply::Stage::Artifact
                },
            })
        );
        drop(response);
        for ledger in &ledgers {
            assert_eq!(ledger.0.current(), 0);
        }
    }
}
