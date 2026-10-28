use super::*;
use crate::jit::{
    backend, compiler_wire as wire,
    ir::Snapshot,
    resources::{Ledger, MappingCounter},
    work,
};
use std::os::unix::process::ExitStatusExt;

const CHILD: &str = "LUNA_COMPILER_PROCESS_FIXTURE";

thread_local! {
    static COMMANDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn limits() -> Limits {
    Limits {
        request: 1 << 20,
        response: 1 << 20,
        address_space: 512 << 20,
        cpu_seconds: 3,
        wall: Duration::from_secs(30),
    }
}

fn wire_limits() -> wire::Limits {
    wire::Limits {
        bytes: 1 << 20,
        operations: 65536,
        constants: 65536,
    }
}

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(32 << 20))
}

fn command(mode: &str) -> Command {
    COMMANDS.set(COMMANDS.get().saturating_add(1));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "jit::compiler_process::tests::child",
        "--test-threads=1",
        "--quiet",
    ]);
    command.env(CHILD, mode);
    command
}

fn reaped(pid: u32) {
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn child() {
    let Ok(mode) = std::env::var(CHILD) else {
        return;
    };
    match mode.as_str() {
        "cpu" => loop {
            std::hint::black_box(123u64.wrapping_mul(456));
        },
        "wait" => std::thread::sleep(Duration::from_secs(120)),
        "flood" => {
            let mut output = io::stderr().lock();
            loop {
                output.write_all(&[123; 4096]).unwrap();
            }
        }
        "exit" => std::process::exit(23),
        "duplex" => {
            let mut output = io::stderr().lock();
            output.write_all(&[99; 65536]).unwrap();
            let mut request = std::vec::Vec::new();
            io::stdin().read_to_end(&mut request).unwrap();
            output
                .write_all(&(request.len() as u64).to_le_bytes())
                .unwrap();
        }
        "memory" => {
            let mapping = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    1 << 30,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                    -1,
                    0,
                )
            };
            assert_eq!(mapping, libc::MAP_FAILED);
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOMEM)
            );
            io::stderr().write_all(b"address-space-refused").unwrap();
        }
        "artifact" => {
            use crate::jit::compiler_request as request;
            let mut packet = Vec::new_in(allocator());
            packet.try_reserve_exact(request::MAX_BYTES).unwrap();
            let mut input = io::stdin().lock().take(request::MAX_BYTES as u64 + 1);
            let mut scratch = [0; 4096];
            loop {
                let count = input.read(&mut scratch).unwrap();
                if count == 0 {
                    break;
                }
                assert!(packet.len() + count <= request::MAX_BYTES);
                packet.extend_from_slice(&scratch[..count]);
            }
            let identity = request::worker_identity().unwrap();
            let abi = request::abi_identity();
            let view = request::decode(&packet, &identity, &abi).unwrap();
            let snapshot = wire::decode(view.source, wire_limits(), allocator()).unwrap();
            let bundle = backend::artifact::with_target(view.features, || {
                backend::artifact::emission::emit(
                    &snapshot,
                    match view.route {
                        request::Route::Ordinary => backend::artifact::Route::Ordinary,
                        request::Route::Arrays => backend::artifact::Route::Arrays,
                        request::Route::Readonly => backend::artifact::Route::Readonly,
                    },
                    view.resume,
                    view.work,
                    allocator(),
                    allocator(),
                )
            })
            .unwrap()
            .unwrap();
            assert_eq!(bundle.target_environment(), view.environment);
            let build = request::binding(&packet).unwrap();
            let response = backend::artifact::encode(
                &bundle,
                backend::artifact::Binding {
                    request: view.request,
                    build: &build,
                    source: view.source,
                    environment: view.environment,
                },
                artifact_limits(),
                allocator(),
            )
            .unwrap();
            io::stderr().write_all(&response).unwrap();
        }
        "compile" => {
            let mut packet = Vec::new_in(allocator());
            packet.try_reserve_exact(1 << 20).unwrap();
            let mut input = io::stdin().lock().take((1 << 20) + 1);
            let mut scratch = [0; 4096];
            loop {
                let count = input.read(&mut scratch).unwrap();
                if count == 0 {
                    break;
                }
                assert!(packet.len() + count <= 1 << 20);
                packet.extend_from_slice(&scratch[..count]);
            }
            let snapshot = wire::decode(&packet, wire_limits(), allocator()).unwrap();
            drop(packet);
            let ledger = Ledger::new(4 << 20);
            let total = MappingCounter::new(ledger.clone());
            let metadata = allocator();
            let requested_work = work::Limits::from(&crate::JitConfig::default());
            let code = if crate::jit::readonly_lookup::Plan::recognize(&snapshot).is_some() {
                crate::jit::readonly_lookup::binding::integer_resume::with_enabled(true, || {
                    backend::readonly_lookup::compile_pair(
                        &snapshot,
                        total.clone(),
                        4 << 20,
                        metadata,
                        requested_work,
                        backend::Failure::None,
                    )
                })
                .0
            } else {
                backend::array::compile_pair(
                    &snapshot,
                    total.clone(),
                    4 << 20,
                    metadata,
                    requested_work,
                    backend::Failure::None,
                )
            }
            .unwrap();
            let mapped = total.load(std::sync::atomic::Ordering::Relaxed);
            assert!(mapped > 0);
            let mut routes = 1u64;
            if code.array_kernels.is_some() {
                routes |= 2;
            }
            if let Some(readonly) = &code.readonly_lookup {
                routes |= 4;
                if readonly.integer_resume.is_some() {
                    routes |= 8;
                }
            }
            drop(code);
            assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            assert_eq!(ledger.current(), 0);
            let result = wire::encode(&snapshot, wire_limits(), allocator()).unwrap();
            let mut output = io::stderr().lock();
            output.write_all(b"LUCOMP01").unwrap();
            output.write_all(&(mapped as u64).to_le_bytes()).unwrap();
            output.write_all(&routes.to_le_bytes()).unwrap();
            output.write_all(&result).unwrap();
        }
        _ => panic!("unknown compiler fixture"),
    }
}

#[test]
fn real_owned_snapshots_compile_after_cross_process_arena_release() {
    mod workloads {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/jit_support/workloads.rs"
        ));
    }
    let packets = {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            workloads::WORKLOADS
                .iter()
                .map(|workload| {
                    (
                        workload.name,
                        workload.source,
                        match workload.name {
                            "array_table" => 3,
                            "rust_callbacks" => 13,
                            _ => 1,
                        },
                    )
                })
                .chain([
                    ("predicate", workloads::PREDICATE_SOURCE, 1),
                    ("cold", workloads::COLD_SOURCE, 1),
                    ("prepared", &b"return target"[..], 1),
                ])
                .map(|(name, source, routes)| {
                    let closure = crate::Closure::load(ctx, None, source).unwrap();
                    let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
                    (
                        name,
                        wire::encode(&snapshot, wire_limits(), allocator()).unwrap(),
                        routes,
                    )
                })
                .collect::<std::vec::Vec<_>>()
        })
    };
    assert_eq!(packets.len(), 10);
    let mut all_routes = 0;
    for (name, packet, expected_routes) in packets {
        let output = allocator();
        let ledger = output.0.clone();
        let receipt = run(command("compile"), &packet, limits(), output).unwrap();
        reaped(receipt.pid);
        assert_eq!(&receipt.bytes[..8], b"LUCOMP01");
        assert!(u64::from_le_bytes(receipt.bytes[8..16].try_into().unwrap()) > 0);
        let routes = u64::from_le_bytes(receipt.bytes[16..24].try_into().unwrap());
        assert_eq!(routes, expected_routes, "{name}");
        all_routes |= routes;
        assert_eq!(&receipt.bytes[24..], &*packet);
        let snapshot = wire::decode(&receipt.bytes[24..], wire_limits(), allocator()).unwrap();
        snapshot.verify().unwrap();
        drop(receipt);
        assert_eq!(ledger.current(), 0);
    }
    assert_eq!(all_routes, 15);
}

#[test]
fn deadlines_cover_stalled_input_and_silent_workers_and_reap_them() {
    for request in [&[][..], &vec![123; 1 << 20][..]] {
        let output = allocator();
        let ledger = output.0.clone();
        let failure = run(
            command("wait"),
            request,
            Limits {
                wall: Duration::from_millis(200),
                ..limits()
            },
            output,
        )
        .err()
        .unwrap();
        assert_eq!(failure.cause, Cause::Deadline);
        assert!(failure.status.unwrap().signal().is_some());
        reaped(failure.pid.unwrap());
        assert_eq!(ledger.current(), 0);
    }
}

#[test]
fn response_floods_and_abnormal_exits_reclaim_all_owned_storage() {
    for (mode, expected) in [("flood", Cause::ResponseLimit), ("exit", Cause::Exit)] {
        let output = allocator();
        let ledger = output.0.clone();
        let failure = run(
            command(mode),
            &[],
            Limits {
                response: 32,
                ..limits()
            },
            output,
        )
        .err()
        .unwrap();
        assert_eq!(failure.cause, expected);
        reaped(failure.pid.unwrap());
        assert_eq!(ledger.current(), 0);
        if mode == "exit" {
            assert_eq!(failure.status.unwrap().code(), Some(23));
        }
    }
}

#[test]
fn kernel_enforces_the_worker_address_space_limit() {
    let receipt = run(command("memory"), &[], limits(), allocator()).unwrap();
    assert_eq!(&*receipt.bytes, b"address-space-refused");
    reaped(receipt.pid);
}

#[test]
fn kernel_enforces_the_worker_cpu_limit() {
    let failure = run(
        command("cpu"),
        &[],
        Limits {
            cpu_seconds: 1,
            wall: Duration::from_secs(120),
            ..limits()
        },
        allocator(),
    )
    .err()
    .unwrap();
    assert_eq!(failure.cause, Cause::Exit);
    assert_eq!(failure.status.unwrap().signal(), Some(libc::SIGKILL));
    reaped(failure.pid.unwrap());
}

#[test]
fn request_and_storage_refusals_precede_process_creation() {
    for allowance in 0..4 {
        let snapshot = crate::Lua::empty().enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, &b"local x=1 return x+1"[..]).unwrap();
            Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
        });
        let workspace = snapshot.operations.allocator().clone();
        let baseline = workspace.0.current();
        let commands = COMMANDS.get();
        workspace.0.fail_after(allowance);
        assert!(matches!(
            request_packet(
                &snapshot,
                backend::artifact::Route::Ordinary,
                false,
                work::Limits::from(&crate::JitConfig::default()),
            ),
            Err(crate::JitError::ResourceLimit(_))
        ));
        assert_eq!(COMMANDS.get(), commands);
        assert_eq!(workspace.0.current(), baseline);
        assert!(Vec::<u8, _>::new_in(workspace.clone())
            .try_reserve_exact(1)
            .is_err());
        drop(snapshot);
        assert_eq!(workspace.0.current(), 0);
    }
    let missing = || Command::new("/luna-compiler-fixture-does-not-exist");
    let output = allocator();
    output.0.fail_after(0);
    let failure = run(missing(), &[], limits(), output.clone()).err().unwrap();
    assert_eq!(failure.cause, Cause::Allocation);
    assert_eq!(failure.pid, None);
    assert_eq!(output.0.current(), 0);
    let failure = run(
        missing(),
        &[1],
        Limits {
            request: 0,
            ..limits()
        },
        allocator(),
    )
    .err()
    .unwrap();
    assert_eq!(failure.cause, Cause::RequestLimit);
    assert_eq!(failure.pid, None);
    for bad in [
        Limits {
            address_space: 0,
            ..limits()
        },
        Limits {
            address_space: libc::RLIM_INFINITY,
            ..limits()
        },
        Limits {
            cpu_seconds: 0,
            ..limits()
        },
        Limits {
            cpu_seconds: libc::RLIM_INFINITY,
            ..limits()
        },
        Limits {
            wall: Duration::ZERO,
            ..limits()
        },
        Limits {
            wall: Duration::MAX,
            ..limits()
        },
    ] {
        let failure = run(missing(), &[], bad, allocator()).err().unwrap();
        assert_eq!(failure.cause, Cause::Configuration);
        assert_eq!(failure.pid, None);
    }
}

#[test]
fn bidirectional_backpressure_preserves_exact_response_limits() {
    let request = vec![123; 1 << 20];
    let output = allocator();
    let ledger = output.0.clone();
    let receipt = run(
        command("duplex"),
        &request,
        Limits {
            response: 65544,
            ..limits()
        },
        output,
    )
    .unwrap();
    assert_eq!(receipt.bytes.len(), 65544);
    assert!(receipt.bytes[..65536].iter().all(|&byte| byte == 99));
    assert_eq!(
        u64::from_le_bytes(receipt.bytes[65536..].try_into().unwrap()),
        request.len() as u64
    );
    reaped(receipt.pid);
    drop(receipt);
    assert_eq!(ledger.current(), 0);
    let failure = run(
        command("duplex"),
        &request,
        Limits {
            response: 65543,
            ..limits()
        },
        allocator(),
    )
    .err()
    .unwrap();
    assert_eq!(failure.cause, Cause::ResponseLimit);
    reaped(failure.pid.unwrap());
}

#[test]
fn rejected_wire_requests_do_not_damage_the_parent_or_next_worker() {
    for request in [&[][..], &b"not-a-snapshot"[..]] {
        let output = allocator();
        let ledger = output.0.clone();
        let failure = run(command("compile"), request, limits(), output)
            .err()
            .unwrap();
        assert_eq!(failure.cause, Cause::Exit);
        assert_eq!(failure.status.unwrap().code(), Some(101));
        reaped(failure.pid.unwrap());
        assert_eq!(ledger.current(), 0);
    }
    let receipt = run(command("memory"), &[], limits(), allocator()).unwrap();
    reaped(receipt.pid);
    assert_eq!(&*receipt.bytes, b"address-space-refused");
}

fn artifact_limits() -> backend::artifact::Limits {
    backend::artifact::Limits {
        bytes: 2 << 20,
        modules: 16,
        declarations: 256,
        functions: 128,
        relocations: 8192,
    }
}

pub(in crate::jit) fn request_packet(
    snapshot: &Snapshot,
    route: backend::artifact::Route,
    resume: bool,
    requested_work: work::Limits,
) -> Result<Packet, crate::JitError> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let request = NEXT
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |id| id.checked_add(1),
        )
        .map_err(|_| crate::JitError::ResourceLimit("compiler request identities"))?;
    let workspace = snapshot.operations.allocator().clone();
    let source = wire::encode(snapshot, wire_limits(), workspace.clone())
        .map_err(|_| crate::JitError::ResourceLimit("compiler request snapshot"))?;
    let environment =
        backend::artifact::environment_in(workspace.clone()).map_err(|error| match error {
            backend::artifact::Error::Allocation | backend::artifact::Error::Limit => {
                crate::JitError::ResourceLimit("compiler target environment")
            }
            _ => crate::JitError::Unavailable("compiler target environment"),
        })?;
    let features =
        backend::artifact::target_features_in(workspace.clone()).map_err(|error| match error {
            backend::artifact::Error::Allocation | backend::artifact::Error::Limit => {
                crate::JitError::ResourceLimit("compiler target features")
            }
            _ => crate::JitError::Unavailable("compiler target features"),
        })?;
    use crate::jit::compiler_request as protocol;
    let identity = protocol::worker_identity()
        .map_err(|_| crate::JitError::Unavailable("compiler image identity"))?;
    let abi = protocol::abi_identity();
    let frame = protocol::encode(
        &protocol::View {
            request,
            worker: &identity,
            abi: &abi,
            work: requested_work,
            route: match route {
                backend::artifact::Route::Ordinary => protocol::Route::Ordinary,
                backend::artifact::Route::Arrays => protocol::Route::Arrays,
                backend::artifact::Route::Readonly => protocol::Route::Readonly,
            },
            resume,
            features: &features,
            environment: &environment,
            source: &source,
        },
        workspace.clone(),
    )
    .map_err(|error| match error {
        protocol::Error::Allocation | protocol::Error::Limit => {
            crate::JitError::ResourceLimit("compiler request packet")
        }
        _ => crate::JitError::Unavailable("compiler request packet"),
    })?;
    let build = protocol::binding(&frame)
        .map_err(|_| crate::JitError::Unavailable("compiler request binding"))?;
    let receipt = run(
        command("artifact"),
        &frame,
        Limits {
            request: protocol::MAX_BYTES,
            response: 2 << 20,
            ..limits()
        },
        workspace,
    )
    .map_err(|failure| match failure.cause {
        Cause::Allocation | Cause::RequestLimit | Cause::ResponseLimit => {
            crate::JitError::ResourceLimit("compiler transport")
        }
        _ => crate::JitError::Unavailable("compiler worker failed"),
    })?;
    reaped(receipt.pid);
    Ok(Packet {
        source,
        bytes: receipt.bytes,
        environment,
        request,
        build,
    })
}

#[test]
fn allocation_refusal_while_receiving_kills_and_reaps_the_worker() {
    let output = allocator();
    let ledger = output.0.clone();
    output.0.fail_after(1);
    let failure = run(
        command("flood"),
        &[],
        Limits {
            response: 16384,
            ..limits()
        },
        output.clone(),
    )
    .err()
    .unwrap();
    assert_eq!(failure.cause, Cause::Allocation);
    reaped(failure.pid.unwrap());
    assert_eq!(ledger.current(), 0);
    let mut probe = Vec::<u8, _>::new_in(output);
    assert!(probe.try_reserve_exact(1).is_err());
}

#[test]
fn child_generated_artifacts_run_all_production_entries_in_the_parent() {
    mod workloads {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/jit_support/workloads.rs"
        ));
    }
    let requests = {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            workloads::WORKLOADS
                .iter()
                .map(|workload| (workload.name, workload.source))
                .chain([
                    ("predicate", workloads::PREDICATE_SOURCE),
                    ("cold", workloads::COLD_SOURCE),
                    ("prepared", &b"return target"[..]),
                ])
                .map(|(name, source)| {
                    let closure = crate::Closure::load(ctx, None, source).unwrap();
                    let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
                    (
                        name,
                        wire::encode(&snapshot, wire_limits(), allocator()).unwrap(),
                    )
                })
                .collect::<std::vec::Vec<_>>()
        })
    };
    let environment = backend::artifact::environment().unwrap();
    let target_features = backend::artifact::target_features().unwrap();
    let mut routes = 0;
    let mut calls = 0;
    let mut instructions = 0;
    let mut progressing_routes = 0;
    for (index, (name, source)) in requests.into_iter().enumerate() {
        let request = index as u64 + 1;
        use crate::jit::compiler_request as protocol;
        let snapshot = wire::decode(&source, wire_limits(), allocator()).unwrap();
        let route = if crate::jit::readonly_lookup::Plan::recognize(&snapshot).is_some() {
            protocol::Route::Readonly
        } else {
            protocol::Route::Arrays
        };
        drop(snapshot);
        let frame = protocol::encode(
            &protocol::View {
                request,
                worker: &protocol::worker_identity().unwrap(),
                abi: &protocol::abi_identity(),
                work: work::Limits::from(&crate::JitConfig::default()),
                route,
                resume: true,
                features: &target_features,
                environment: &environment,
                source: &source,
            },
            allocator(),
        )
        .unwrap();
        let build = protocol::binding(&frame).unwrap();
        let receipt = run(
            command("artifact"),
            &frame,
            Limits {
                request: protocol::MAX_BYTES,
                ..limits()
            },
            allocator(),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        reaped(receipt.pid);
        let metadata = allocator();
        let ledger = metadata.0.clone();
        let total = MappingCounter::new(ledger.clone());
        let installed = backend::artifact::install(
            &receipt.bytes,
            backend::artifact::Binding {
                request,
                build: &build,
                source: &source,
                environment: &environment,
            },
            artifact_limits(),
            total.clone(),
            4 << 20,
            metadata,
            backend::Failure::None,
        )
        .unwrap();
        let snapshot = wire::decode(&source, wire_limits(), allocator()).unwrap();
        drop(source);
        drop(receipt);
        for module in 0..installed.len() {
            routes |= 1 << (installed.kind(module) as u8 - 1);
        }
        let (count, native, progressing) = backend::artifact::exercise(&installed, &snapshot);
        calls += count;
        instructions += native;
        progressing_routes |= progressing;
        if name == "prepared" {
            let (bound_calls, bound_instructions) =
                backend::artifact::exercise_prepared(&installed, &snapshot);
            calls += bound_calls;
            instructions += bound_instructions;
            eprintln!(
                "artifact-parent-helper calls={bound_calls} instructions={bound_instructions}"
            );
        }
        eprintln!("artifact-case name={name} calls={count} instructions={native} progressing={progressing}");
        drop(snapshot);
        drop(installed);
        assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(total.requested(), 0);
        assert_eq!(ledger.current(), 0);
    }
    assert_eq!(routes, 15);
    assert_eq!(progressing_routes, 15);
    assert!(calls > 0 && instructions > 0);
    eprintln!("artifact-parent calls={calls} instructions={instructions} routes={routes}");
}

#[test]
fn owned_requests_reject_cross_process_mismatches_and_ignore_old_env_channels() {
    use crate::jit::compiler_request as protocol;
    let snapshot = crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, &b"local n=1 return n+2"[..]).unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
    });
    let source = wire::encode(&snapshot, wire_limits(), allocator()).unwrap();
    drop(snapshot);
    let environment = backend::artifact::environment().unwrap();
    let features = backend::artifact::target_features().unwrap();
    let frame = protocol::encode(
        &protocol::View {
            request: 37,
            worker: &protocol::worker_identity().unwrap(),
            abi: &protocol::abi_identity(),
            work: work::Limits::from(&crate::JitConfig::default()),
            route: protocol::Route::Ordinary,
            resume: false,
            features: &features,
            environment: &environment,
            source: &source,
        },
        allocator(),
    )
    .unwrap();
    for offset in [32, 64, 96, 120, 124, 152, frame.len() - 1] {
        let output = allocator();
        let ledger = output.0.clone();
        let mut changed = frame.clone();
        changed[offset] ^= 1;
        let failure = run(
            command("artifact"),
            &changed,
            Limits {
                request: protocol::MAX_BYTES,
                ..limits()
            },
            output,
        )
        .err()
        .unwrap();
        assert_eq!(failure.cause, Cause::Exit, "offset={offset}");
        assert_eq!(failure.status.unwrap().code(), Some(101));
        reaped(failure.pid.unwrap());
        assert_eq!(ledger.current(), 0);
    }
    let mut worker = command("artifact");
    for key in [
        "LUNA_COMPILER_REQUEST_ID",
        "LUNA_COMPILER_TARGET_FEATURES",
        "LUNA_COMPILER_ROUTE",
        "LUNA_COMPILER_RESUME",
        "LUNA_COMPILER_WORK",
    ] {
        worker.env(key, "not-a-valid-request-field");
    }
    let receipt = run(
        worker,
        &frame,
        Limits {
            request: protocol::MAX_BYTES,
            ..limits()
        },
        allocator(),
    )
    .unwrap();
    reaped(receipt.pid);
    let total = MappingCounter::new(Ledger::new(4 << 20));
    let installed = backend::artifact::install(
        &receipt.bytes,
        backend::artifact::Binding {
            request: 37,
            build: &protocol::binding(&frame).unwrap(),
            source: &source,
            environment: &environment,
        },
        artifact_limits(),
        total.clone(),
        4 << 20,
        allocator(),
        backend::Failure::None,
    )
    .unwrap();
    assert_eq!(installed.len(), 1);
    drop(installed);
    assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
}
