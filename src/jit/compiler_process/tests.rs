use super::*;
use crate::jit::{
    backend, compiler_wire as wire,
    ir::Snapshot,
    resources::{Ledger, MappingCounter},
    work,
};
use std::os::unix::process::ExitStatusExt;

const CHILD: &str = "LUNA_COMPILER_PROCESS_FIXTURE";

struct Deployment {
    image: crate::jit::compiler_image::Image,
}

fn deployment() -> Result<Option<&'static Deployment>, crate::jit::compiler_request::Error> {
    static IMAGE: std::sync::OnceLock<Deployment> = std::sync::OnceLock::new();
    if let Some(image) = IMAGE.get() {
        return Ok(Some(image));
    }
    let Some(path) = std::env::var_os("LUNA_JIT_WORKER_IMAGE") else {
        #[cfg(feature = "jit-worker-embedded")]
        if let Some((bytes, identity, _)) = crate::jit::__worker_embedded_image() {
            let ledger = Ledger::try_host(513 << 20)
                .map_err(|_| crate::jit::compiler_request::Error::Allocation)?;
            let image = crate::jit::compiler_image::Image::new(bytes, identity, 512 << 20, ledger)
                .map_err(|_| crate::jit::compiler_request::Error::Identity)?;
            let _ = IMAGE.set(Deployment { image });
            return Ok(IMAGE.get());
        }
        return Ok(None);
    };
    let mut file =
        std::fs::File::open(path).map_err(|_| crate::jit::compiler_request::Error::Io)?;
    let identity = crate::jit::compiler_request::image_identity(&mut file, 512 << 20)?;
    use std::io::{Seek, SeekFrom};
    let length = usize::try_from(
        file.metadata()
            .map_err(|_| crate::jit::compiler_request::Error::Io)?
            .len(),
    )
    .map_err(|_| crate::jit::compiler_request::Error::Limit)?;
    if length == 0 || length > 512 << 20 {
        return Err(crate::jit::compiler_request::Error::Limit);
    }
    let ledger = Ledger::try_host(1025 << 20)
        .map_err(|_| crate::jit::compiler_request::Error::Allocation)?;
    let mut bytes = Vec::new_in(BudgetAllocator(ledger.clone()));
    bytes
        .try_reserve_exact(length)
        .map_err(|_| crate::jit::compiler_request::Error::Allocation)?;
    bytes.resize(length, 0);
    file.seek(SeekFrom::Start(0))
        .map_err(|_| crate::jit::compiler_request::Error::Io)?;
    file.read_exact(&mut bytes)
        .map_err(|_| crate::jit::compiler_request::Error::Io)?;
    let mut tail = [0];
    if file
        .read(&mut tail)
        .map_err(|_| crate::jit::compiler_request::Error::Io)?
        != 0
    {
        return Err(crate::jit::compiler_request::Error::Identity);
    }
    let image = crate::jit::compiler_image::Image::new(&bytes, &identity, 512 << 20, ledger)
        .map_err(|_| crate::jit::compiler_request::Error::Identity)?;
    let _ = IMAGE.set(Deployment { image });
    Ok(IMAGE.get())
}

fn compiler_identity() -> Result<[u8; 32], crate::jit::compiler_request::Error> {
    deployment()?.map_or_else(crate::jit::compiler_request::worker_identity, |image| {
        Ok(*image.image.identity())
    })
}

fn artifact_rejection_code() -> i32 {
    if deployment().unwrap().is_some() {
        67
    } else {
        101
    }
}

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

fn command(mode: &str) -> Launch<'static> {
    COMMANDS.set(COMMANDS.get().saturating_add(1));
    if mode == "artifact" {
        if let Some(image) = deployment().unwrap() {
            return Launch::Image(&image.image);
        }
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "jit::compiler_process::tests::child",
        "--test-threads=1",
        "--quiet",
    ]);
    command.env(CHILD, mode);
    Launch::Fixture(command)
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
fn sealed_launch_startup_faults_are_typed_and_reaped() {
    let bytes = b"not an executable";
    let ledger = Ledger::try_host(1 << 20).unwrap();
    let baseline = ledger.accounted();
    let identity = crate::jit::compiler_request::image_identity(&bytes[..], 1 << 20).unwrap();
    let image = crate::jit::compiler_image::Image::new(bytes, &identity, 1 << 20, ledger.clone())
        .ok()
        .unwrap();
    let output = allocator();
    let failure = run(Launch::Image(&image), &[], limits(), output.clone())
        .err()
        .unwrap();
    assert_eq!(
        failure.cause,
        Cause::Startup(crate::jit::compiler_launch::Fault {
            stage: crate::jit::compiler_launch::Stage::Exec,
            errno: libc::ENOEXEC,
        })
    );
    assert_eq!(failure.status.unwrap().code(), Some(127));
    reaped(failure.pid.unwrap());
    assert_eq!(output.0.current(), 0);
    drop(image);
    assert_eq!(ledger.accounted(), baseline);
}

#[test]
fn sealed_launch_startup_shares_the_transport_deadline() {
    let bytes = b"not executed";
    let identity = crate::jit::compiler_request::image_identity(&bytes[..], 1 << 20).unwrap();
    let image = crate::jit::compiler_image::Image::new(
        bytes,
        &identity,
        1 << 20,
        Ledger::try_host(1 << 20).unwrap(),
    )
    .ok()
    .unwrap();
    let output = allocator();
    let failure = run(
        Launch::Stalled(&image),
        &[],
        Limits {
            wall: Duration::from_millis(20),
            ..limits()
        },
        output.clone(),
    )
    .err()
    .unwrap();
    assert_eq!(failure.cause, Cause::Deadline);
    assert_eq!(failure.status.unwrap().signal(), Some(libc::SIGKILL));
    reaped(failure.pid.unwrap());
    assert_eq!(output.0.current(), 0);
}

#[test]
fn owned_deadline_retains_failed_cleanup_and_image_until_supervisor_recovery() {
    let host = Ledger::try_host(1 << 20).unwrap();
    let metadata = BudgetAllocator(Ledger::try_child(1 << 20, host.clone()).unwrap());
    let supervisor = crate::jit::compiler_jobs::Supervisor::new(1, metadata.clone()).unwrap();
    let baseline = host.accounted();
    let bytes = b"not executed";
    let identity = crate::jit::compiler_request::image_identity(&bytes[..], 1 << 20).unwrap();
    let image = crate::jit::compiler_image::Image::shared(
        bytes,
        &identity,
        1 << 20,
        host.clone(),
        metadata,
    )
    .ok()
    .unwrap();
    let failure = run(
        Launch::OwnedStalled(
            crate::jit::compiler_launch::Admission::new(image, 64 << 10).unwrap(),
            supervisor.reserve().unwrap(),
            100,
            100,
        ),
        &[],
        Limits {
            wall: Duration::from_millis(20),
            address_space: 64 << 10,
            ..limits()
        },
        allocator(),
    )
    .err()
    .unwrap();
    assert_eq!(failure.cause, Cause::Deadline);
    assert_eq!(failure.status, None);
    assert_eq!(supervisor.occupied(), 1);
    assert!(host.accounted() > baseline);
    assert!(supervisor.reserve().is_err());
    supervisor.fail_cleanup(0, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while supervisor.poll() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    reaped(failure.pid.unwrap());
    assert_eq!(host.accounted(), baseline);
}

#[cfg(feature = "jit-worker-embedded")]
#[test]
fn embedded_pending_job_survives_cache_clear_off_polling_and_lua_teardown() {
    let mut lua = crate::Lua::empty();
    let original = lua.jit_stats().accounted_jit_bytes;
    let supervisor = lua.enter(|ctx| ctx.jit().compiler_jobs()).unwrap();
    let (host, metadata) = lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        (manager.host.clone(), manager.metadata.clone())
    });
    let spawn = || {
        let ticket = supervisor.reserve().unwrap();
        let bytes = b"not executed";
        let identity = crate::jit::compiler_request::image_identity(&bytes[..], 1 << 20).unwrap();
        let image = crate::jit::compiler_image::Image::shared(
            bytes,
            &identity,
            1 << 20,
            host.clone(),
            metadata.clone(),
        )
        .ok()
        .unwrap();
        let mut child = crate::jit::compiler_launch::Child::spawn_owned(
            crate::jit::compiler_launch::Admission::new(image, 512 << 20).unwrap(),
            crate::jit::compiler_launch::Limits {
                address_space: 512 << 20,
                cpu_seconds: 3,
                stall: true,
            },
        )
        .unwrap();
        child.fail_cleanup(usize::MAX, usize::MAX);
        ticket.retain(child);
    };
    spawn();
    let charged = lua.jit_stats().accounted_jit_bytes;
    lua.clear_jit_cache();
    lua.set_jit_config(crate::JitConfig::default()).unwrap();
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert_eq!(lua.jit_stats().accounted_jit_bytes, charged);
    assert_eq!(supervisor.occupied(), 1);
    supervisor.fail_cleanup(0, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while supervisor.occupied() != 0 {
        lua.service_jit().unwrap();
        assert!(Instant::now() < deadline);
    }
    spawn();
    drop(lua);
    assert_eq!(supervisor.occupied(), 1);
    assert!(host.current() > 0);
    supervisor.fail_cleanup(0, 0);
    while supervisor.poll() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let completion = supervisor.completion();
    drop(supervisor);
    completion.join();
    assert_eq!(host.current(), 0);
    assert!(original > 0);
}

#[cfg(feature = "jit-worker-embedded")]
#[test]
fn embedded_last_lua_drop_disconnects_without_waiting_for_permanent_job_cleanup() {
    let mut lua = crate::Lua::empty();
    let supervisor = lua.enter(|ctx| ctx.jit().compiler_jobs()).unwrap();
    let (host, metadata) = lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        (manager.host.clone(), manager.metadata.clone())
    });
    let ticket = supervisor.reserve().unwrap();
    let (bytes, identity, _) = crate::jit::__worker_embedded_image().unwrap();
    let image = crate::jit::compiler_image::Image::shared(
        bytes,
        identity,
        512 << 20,
        host.clone(),
        metadata.clone(),
    )
    .ok()
    .unwrap();
    let admission = crate::jit::compiler_launch::Admission::new(image, 512 << 20).unwrap();
    let mut child = crate::jit::compiler_launch::Child::spawn_owned(
        admission,
        crate::jit::compiler_launch::Limits {
            address_space: 512 << 20,
            cpu_seconds: 3,
            stall: true,
        },
    )
    .unwrap();
    let faults = child.fail_cleanup(usize::MAX, usize::MAX);
    ticket.retain(child);
    drop(supervisor);
    let (returned, observed) = std::sync::mpsc::sync_channel(1);
    let (release, resume) = std::sync::mpsc::sync_channel(1);
    let rescue = std::thread::spawn(move || {
        let disconnected = observed.recv_timeout(Duration::from_secs(2)).is_ok();
        if disconnected {
            let _ = resume.recv_timeout(Duration::from_secs(5));
        }
        faults.reset(0, 0);
        disconnected
    });
    drop(lua);
    let pending = host.current();
    let _ = returned.send(());
    let _ = release.send(());
    let disconnected = rescue.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.current() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(disconnected, "Lua destruction waited for denied cleanup");
    assert!(pending >= 512 << 20);
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
        "empty-reply" => {}
        "bad-reply" => io::stderr().write_all(b"not-a-compiler-reply").unwrap(),
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
            use crate::jit::{compiler_request as request, compiler_stream as stream};
            stream::serve(
                &mut io::stdin().lock(),
                &mut io::stderr().lock(),
                stream::Service {
                    worker: request::worker_identity().unwrap(),
                    abi: request::abi_identity(),
                    request_limit: request::MAX_BYTES,
                    input: allocator(),
                    limits: crate::jit::compiler_service::Limits {
                        source: wire_limits(),
                        artifact: artifact_limits(),
                    },
                    allocators: crate::jit::compiler_service::Allocators {
                        source: allocator(),
                        metadata: allocator(),
                        objects: allocator(),
                        output: allocator(),
                    },
                },
            )
            .unwrap();
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
    request_packet_inner(snapshot, route, resume, requested_work, None, None)
}

pub(in crate::jit) fn request_packet_in(
    snapshot: &Snapshot,
    route: backend::artifact::Route,
    resume: bool,
    requested_work: work::Limits,
    host: crate::jit::resources::LedgerRef,
    metadata: BudgetAllocator,
    supervisor: Option<crate::jit::compiler_jobs::Supervisor>,
) -> Result<Packet, crate::JitError> {
    request_packet_inner(
        snapshot,
        route,
        resume,
        requested_work,
        Some((host, metadata)),
        supervisor,
    )
}

fn request_packet_inner(
    snapshot: &Snapshot,
    route: backend::artifact::Route,
    resume: bool,
    requested_work: work::Limits,
    host: Option<(crate::jit::resources::LedgerRef, BudgetAllocator)>,
    supervisor: Option<crate::jit::compiler_jobs::Supervisor>,
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
    let publication = host
        .as_ref()
        .map_or_else(|| workspace.clone(), |(_, metadata)| metadata.clone());
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
    let descriptor: Option<(&'static [u8], &'static [u8; 32], &'static str)> = {
        #[cfg(feature = "jit-worker-embedded")]
        {
            host.as_ref()
                .and_then(|_| crate::jit::__worker_embedded_image())
        }
        #[cfg(not(feature = "jit-worker-embedded"))]
        {
            None
        }
    };
    let identity = if let Some((_, identity, _)) = descriptor {
        *identity
    } else {
        compiler_identity().map_err(|_| crate::JitError::Unavailable("compiler image identity"))?
    };
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
    let ticket = if descriptor.is_some() {
        supervisor
            .as_ref()
            .ok_or(crate::JitError::Unavailable("compiler supervisor scope"))?
            .ensure_host(&host.as_ref().unwrap().0)?;
        Some(
            supervisor
                .as_ref()
                .ok_or(crate::JitError::Unavailable("compiler supervisor scope"))?
                .reserve()?,
        )
    } else {
        None
    };
    let image = match (host, descriptor) {
        (Some((host, metadata)), Some((bytes, identity, _))) => Some(
            crate::jit::compiler_image::Image::shared(bytes, identity, 512 << 20, host, metadata)
                .map_err(|error| match error {
                crate::jit::compiler_image::Error::Allocation
                | crate::jit::compiler_image::Error::Limit => {
                    crate::JitError::ResourceLimit("compiler image")
                }
                _ => crate::JitError::Unavailable("compiler image"),
            })?,
        ),
        _ => None,
    };
    let command = if let Some(image) = image {
        let admission = crate::jit::compiler_launch::Admission::new(image, limits().address_space)?;
        COMMANDS.set(COMMANDS.get().saturating_add(1));
        Launch::Owned(admission, ticket.unwrap())
    } else {
        command("artifact")
    };
    let receipt = run(
        command,
        &frame,
        Limits {
            request: protocol::MAX_BYTES,
            response: crate::jit::compiler_reply::MAX_BYTES,
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
        bytes: crate::jit::compiler_reply::Artifact::new(receipt.bytes, request, &build)
            .map_err(|failure| failure.into_public(publication))?,
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

#[cfg(feature = "jit-worker-embedded")]
#[test]
fn embedded_image_admission_uses_lua_host_and_metadata_before_spawn_and_recovers() {
    let mut lua = crate::Lua::empty();
    let supervisor = lua.enter(|ctx| ctx.jit().compiler_jobs()).unwrap();
    let (host, metadata, snapshot) = lua.enter(|ctx| {
        let source = crate::Closure::load(ctx, None, &b"local x=1 return x+2"[..]).unwrap();
        let manager = ctx.jit().0.borrow();
        let snapshot =
            Snapshot::new_in(&source.prototype(), 4096, manager.snapshots.clone()).unwrap();
        (manager.host.clone(), manager.metadata.clone(), snapshot)
    });
    let baseline = host.accounted();
    let metadata_before = metadata.0.current();
    let commands = COMMANDS.get();
    let request = || {
        request_packet_in(
            &snapshot,
            backend::artifact::Route::Ordinary,
            false,
            work::Limits::from(&crate::JitConfig::default()),
            host.clone(),
            metadata.clone(),
            Some(supervisor.clone()),
        )
    };
    lua.set_memory_limit(Some(lua.accounted_memory() + (128 << 10)));
    assert!(matches!(
        request(),
        Err(crate::JitError::ResourceLimit("compiler image"))
    ));
    assert_eq!(COMMANDS.get(), commands);
    assert_eq!(host.accounted(), baseline);
    let (bytes, _, _) = crate::jit::__worker_embedded_image().unwrap();
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let allowance = bytes.len().div_ceil(page) * page
        + crate::jit::compiler_image::Owner::allocation_bytes()
        + lua.jit_config().max_snapshot_bytes
        + lua.jit_config().max_metadata_bytes;
    lua.set_memory_limit(Some(lua.accounted_memory() + allowance));
    metadata.0.fail_after(0);
    assert!(matches!(
        request(),
        Err(crate::JitError::ResourceLimit("compiler image"))
    ));
    assert_eq!(COMMANDS.get(), commands);
    assert_eq!(host.accounted(), baseline);
    assert_eq!(metadata.0.current(), metadata_before);
    metadata.0.fail_after(usize::MAX);
    assert!(matches!(
        request(),
        Err(crate::JitError::ResourceLimit("compiler workspace"))
    ));
    assert_eq!(COMMANDS.get(), commands);
    assert_eq!(host.accounted(), baseline);
    assert_eq!(metadata.0.current(), metadata_before);
    lua.set_memory_limit(Some(baseline + allowance + limits().address_space as usize));
    let packet = request().unwrap();
    assert_eq!(COMMANDS.get(), commands + 1);
    assert_eq!(metadata.0.current(), metadata_before);
    drop(packet);
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn worker_resource_refusals_reach_the_parent_without_becoming_crashes() {
    let snapshot = crate::Lua::empty().enter(|ctx| {
        let closure =
            crate::Closure::load(ctx, None, &b"local n=1 return n+host_increment(2)"[..]).unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
    });
    let workspace = snapshot.operations.allocator().0.clone();
    let baseline = workspace.current();
    for field in 0..3 {
        let mut work = work::Limits::from(&crate::JitConfig::default());
        match field {
            0 => work.instructions = 0,
            1 => work.blocks = 0,
            _ => work.relocations = 0,
        }
        let commands = COMMANDS.get();
        assert!(matches!(
            request_packet(&snapshot, backend::artifact::Route::Ordinary, false, work),
            Err(crate::JitError::ResourceLimit(
                "compiler worker compilation"
            ))
        ));
        assert_eq!(COMMANDS.get(), commands + 1);
        assert_eq!(workspace.current(), baseline);
    }
    drop(snapshot);
    assert_eq!(workspace.current(), 0);
}

#[test]
fn successful_but_malformed_workers_are_not_valid_compiler_refusals() {
    use crate::jit::compiler_reply as reply;
    for mode in ["empty-reply", "bad-reply"] {
        let output = allocator();
        let ledger = output.0.clone();
        let receipt = run(command(mode), &[], limits(), output.clone()).unwrap();
        reaped(receipt.pid);
        let fault = reply::Artifact::new(receipt.bytes, 1, &[0; 32])
            .err()
            .unwrap();
        assert!(matches!(fault, reply::Failure::Protocol(_)));
        assert!(matches!(
            fault.into_public(output.clone()),
            crate::JitError::Unavailable("compiler response protocol")
        ));
        assert_eq!(ledger.current(), 0);
    }
    let source = b"not-a-snapshot";
    let features = backend::artifact::target_features().unwrap();
    let environment = backend::artifact::environment().unwrap();
    let frame = crate::jit::compiler_request::encode(
        &crate::jit::compiler_request::View {
            request: 43,
            worker: &compiler_identity().unwrap(),
            abi: &crate::jit::compiler_request::abi_identity(),
            work: work::Limits::from(&crate::JitConfig::default()),
            route: crate::jit::compiler_request::Route::Ordinary,
            resume: false,
            features: &features,
            environment: &environment,
            source,
        },
        allocator(),
    )
    .unwrap();
    let build = crate::jit::compiler_request::binding(&frame).unwrap();
    let receipt = run(command("artifact"), &frame, limits(), allocator()).unwrap();
    reaped(receipt.pid);
    let fault = reply::Artifact::new(receipt.bytes, 43, &build)
        .err()
        .unwrap();
    assert_eq!(
        fault,
        reply::Failure::Worker(reply::Fault {
            class: reply::Class::Rejected,
            stage: reply::Stage::Snapshot,
        })
    );
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
                worker: &compiler_identity().unwrap(),
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
                response: crate::jit::compiler_reply::MAX_BYTES,
                ..limits()
            },
            allocator(),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        reaped(receipt.pid);
        let artifact = crate::jit::compiler_reply::decode(&receipt.bytes, request, &build)
            .unwrap()
            .into_result()
            .unwrap();
        let metadata = allocator();
        let ledger = metadata.0.clone();
        let total = MappingCounter::new(ledger.clone());
        let installed = backend::artifact::install(
            artifact,
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
            worker: &compiler_identity().unwrap(),
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
        assert_eq!(
            failure.status.unwrap().code(),
            Some(artifact_rejection_code())
        );
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
        if let Launch::Fixture(worker) = &mut worker {
            worker.env(key, "not-a-valid-request-field");
        }
    }
    let receipt = run(
        worker,
        &frame,
        Limits {
            request: protocol::MAX_BYTES,
            response: crate::jit::compiler_reply::MAX_BYTES,
            ..limits()
        },
        allocator(),
    )
    .unwrap();
    reaped(receipt.pid);
    let build = protocol::binding(&frame).unwrap();
    let artifact = crate::jit::compiler_reply::decode(&receipt.bytes, 37, &build)
        .unwrap()
        .into_result()
        .unwrap();
    let total = MappingCounter::new(Ledger::new(4 << 20));
    let installed = backend::artifact::install(
        artifact,
        backend::artifact::Binding {
            request: 37,
            build: &build,
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
