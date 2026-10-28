use super::*;
use crate::jit::{
    backend, compiler_wire as wire,
    ir::Snapshot,
    resources::{Ledger, MappingCounter},
    work,
};
use std::os::unix::process::ExitStatusExt;

const CHILD: &str = "LUNA_COMPILER_PROCESS_FIXTURE";

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
            let code = if crate::jit::readonly_lookup::Plan::recognize(&snapshot).is_some() {
                crate::jit::readonly_lookup::binding::integer_resume::with_enabled(true, || {
                    backend::readonly_lookup::compile_pair(
                        &snapshot,
                        total.clone(),
                        4 << 20,
                        metadata,
                        work::Limits::from(&crate::JitConfig::default()),
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
                    work::Limits::from(&crate::JitConfig::default()),
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
