use std::{
    fs::{self, File},
    io,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    abi::{self, Slot},
    backend,
    ir::Snapshot,
    model, JitError,
};
use crate::{
    opcode::{Operation, RCIndex},
    types::{ConstantIndex16, ConstantIndex8, Opt254, RegisterIndex, VarCount},
};

const LIMITS: [(libc::c_int, libc::rlim_t); 4] = [
    (libc::RLIMIT_CPU as _, 30),
    (libc::RLIMIT_AS as _, 2 * 1024 * 1024 * 1024),
    (libc::RLIMIT_CORE as _, 0),
    (libc::RLIMIT_FSIZE as _, 16 * 1024 * 1024),
];

mod heap;

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, limit: usize) -> usize {
        self.next() as usize % limit
    }

    fn register(&mut self) -> RegisterIndex {
        RegisterIndex(self.index(8) as u8)
    }

    fn operand(&mut self) -> RCIndex {
        if self.index(2) == 0 {
            RCIndex::Register(self.register())
        } else {
            RCIndex::Constant(ConstantIndex8(self.index(4) as u8))
        }
    }

    fn slot(&mut self) -> Slot {
        let bits = self.next();
        match self.index(6) {
            0 => Slot {
                tag: abi::NIL,
                bits: 0,
            },
            1 => Slot {
                tag: abi::BOOLEAN,
                bits: bits & 1,
            },
            2 => Slot {
                tag: abi::INTEGER,
                bits,
            },
            3 => Slot {
                tag: abi::NUMBER,
                bits,
            },
            4 => Slot {
                tag: abi::REFERENCE,
                bits: 0,
            },
            _ => Slot {
                tag: abi::NUMBER,
                bits: [
                    0.0f64,
                    -0.0,
                    0.5,
                    -1.5,
                    f64::NAN,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                ][self.index(7)]
                .to_bits(),
            },
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        let length = 8 + self.index(17);
        let allocator =
            super::resources::BudgetAllocator(super::resources::Ledger::new(2 * 1024 * 1024));
        let mut operations =
            allocator_api2::vec::Vec::with_capacity_in(length + 1, allocator.clone());
        for pc in 0..length {
            let dest = self.register();
            let source = self.register();
            let left = self.operand();
            let right = self.operand();
            let skip_if = self.index(2) != 0;
            let target = self.index(length + 1);
            let offset = (target as isize - pc as isize - 1) as i16;
            let op = match self.index(16) {
                0 => Operation::Move { dest, source },
                1 => Operation::LoadConstant {
                    dest,
                    constant: ConstantIndex16(self.index(4) as u16),
                },
                2 => Operation::LoadNil {
                    dest,
                    count: (1 + self.index(8 - usize::from(dest.0))) as u8,
                },
                3 => Operation::LoadBool {
                    dest,
                    value: skip_if,
                    skip_next: pc + 1 < length && self.index(2) != 0,
                },
                4 => Operation::Not { dest, source },
                5 => Operation::Add { dest, left, right },
                6 => Operation::Sub { dest, left, right },
                7 => Operation::Mul { dest, left, right },
                8 => Operation::Div { dest, left, right },
                9 if pc + 1 < length => Operation::Eq {
                    skip_if,
                    left,
                    right,
                },
                10 if pc + 1 < length => Operation::Less {
                    skip_if,
                    left,
                    right,
                },
                11 if pc + 1 < length => Operation::LessEq {
                    skip_if,
                    left,
                    right,
                },
                12 if pc + 1 < length => Operation::Test {
                    value: source,
                    is_true: skip_if,
                },
                13 => Operation::NumericForPrep {
                    base: RegisterIndex(self.index(5) as u8),
                    jump: offset,
                },
                14 => Operation::NumericForLoop {
                    base: RegisterIndex(self.index(5) as u8),
                    jump: offset,
                },
                _ => Operation::Jump {
                    offset,
                    close_upvalues: Opt254::none(),
                },
            };
            operations.push(op);
        }
        operations.push(Operation::Return {
            start: RegisterIndex(0),
            count: VarCount::constant(0),
        });
        let mut constants = allocator_api2::vec::Vec::with_capacity_in(4, allocator);
        constants.extend((0..4).map(|_| self.slot()));
        Snapshot {
            operations,
            constants,
            registers: 8,
            upvalues: 0,
            prototypes: 0,
        }
    }
}

fn invalidate(snapshot: &mut Snapshot, mutation: usize) {
    match mutation {
        0 => {
            snapshot.operations[0] = Operation::Move {
                dest: RegisterIndex(255),
                source: RegisterIndex(0),
            }
        }
        1 => {
            snapshot.operations[0] = Operation::LoadConstant {
                dest: RegisterIndex(0),
                constant: ConstantIndex16(255),
            }
        }
        2 => {
            snapshot.operations[0] = Operation::Jump {
                offset: i16::MAX,
                close_upvalues: Opt254::none(),
            }
        }
        3 => {
            *snapshot.operations.last_mut().unwrap() = Operation::LoadBool {
                dest: RegisterIndex(0),
                value: true,
                skip_next: true,
            }
        }
        4 => {
            *snapshot.operations.last_mut().unwrap() = Operation::LoadNil {
                dest: RegisterIndex(0),
                count: 1,
            }
        }
        5 => snapshot.operations.clear(),
        6 => snapshot.registers = 257,
        7 => {
            snapshot.constants[0] = Slot {
                tag: u64::MAX,
                bits: 0,
            }
        }
        8 => {
            snapshot.constants[0] = Slot {
                tag: abi::REFERENCE,
                bits: 1,
            }
        }
        9 => {
            snapshot.constants[0] = Slot {
                tag: abi::BOOLEAN,
                bits: 2,
            }
        }
        10 => {
            snapshot.constants[0] = Slot {
                tag: abi::NIL,
                bits: 1,
            }
        }
        _ => unreachable!(),
    }
}

fn reject(snapshot: &Snapshot) {
    assert!(matches!(snapshot.verify(), Err(JitError::Compilation(_))));
    let memory = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        backend::compile(snapshot, memory.clone(), 8 * 1024 * 1024),
        Err(JitError::Compilation(_))
    ));
    assert_eq!(memory.load(Ordering::Relaxed), 0);
}

#[test]
fn admission_rejects_all_malformed_mutations_without_mapping_memory() {
    for mutation in 0..11 {
        let mut snapshot = Random(mutation as u64).snapshot();
        snapshot.verify().unwrap();
        invalidate(&mut snapshot, mutation);
        reject(&snapshot);
    }
    let mut snapshot = Random(0).snapshot();
    snapshot.registers = usize::MAX;
    reject(&snapshot);
}

fn scalar_case(random: &mut Random, snapshot: &Snapshot, seed: u64, case: usize) -> (usize, u64) {
    snapshot.verify().unwrap();
    let memory = Arc::new(AtomicUsize::new(0));
    let code = backend::compile(snapshot, memory.clone(), 8 * 1024 * 1024).unwrap();
    let metadata = code.entries.allocator().0.clone();
    let mut invocations = 0;
    let mut instructions = 0;
    for variation in 0..3 {
        let input: Vec<_> = (0..snapshot.registers)
            .map(|_| {
                if variation == 0 {
                    Slot {
                        tag: abi::INTEGER,
                        bits: random.next(),
                    }
                } else if variation == 1 {
                    Slot {
                        tag: abi::NUMBER,
                        bits: random.slot().bits,
                    }
                } else {
                    random.slot()
                }
            })
            .collect();
        for pc in 0..=snapshot.operations.len() + 1 {
            for budget in [0, 1, 2, 3, 63, 64, u32::MAX] {
                let mut expected = input.clone();
                let mut actual = input.clone();
                let reference = model::run(snapshot, &mut expected, pc, budget);
                let exit = code.invoke(&mut actual, pc, budget);
                assert_eq!((exit.pc, exit.instructions, exit.reason), (reference.pc, reference.instructions, reference.reason),
                    "seed={seed} case={case} variation={variation} pc={pc} budget={budget} ops={:?}", snapshot.operations);
                for (register, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
                    assert_eq!(
                        actual.tag, expected.tag,
                        "seed={seed} case={case} pc={pc} register={register}"
                    );
                    if actual.tag == abi::NUMBER && f64::from_bits(expected.bits).is_nan() {
                        assert!(f64::from_bits(actual.bits).is_nan());
                    } else {
                        assert_eq!(actual.bits, expected.bits, "seed={seed} case={case} variation={variation} pc={pc} budget={budget} register={register} input={input:?} ops={:?}", snapshot.operations);
                    }
                }
                invocations += 1;
                instructions += u64::from(exit.instructions);
            }
        }
    }
    drop(code);
    assert_eq!(memory.load(Ordering::Relaxed), 0);
    assert_eq!(
        metadata.current(),
        0,
        "seed={seed} case={case}: backend metadata remained charged"
    );
    (invocations, instructions)
}

fn number(value: &str) -> u64 {
    value
        .strip_prefix("0x")
        .map_or_else(|| value.parse(), |digits| u64::from_str_radix(digits, 16))
        .expect("invalid fuzz seed")
}

#[test]
#[ignore]
fn worker() {
    for (resource, expected) in LIMITS {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::getrlimit(resource as _, &mut limit) }, 0);
        assert_eq!((limit.rlim_cur, limit.rlim_max), (expected, expected));
    }
    match std::env::var("LUNA_JIT_FUZZ_TEST_FAILURE").as_deref() {
        Ok("panic") => panic!("injected worker failure"),
        Ok("signal") => {
            unsafe {
                libc::raise(libc::SIGTERM);
            }
            panic!("signal did not terminate worker");
        }
        Ok("timeout") => thread::sleep(Duration::from_secs(5)),
        _ => {}
    }
    let seed = number(&std::env::var("LUNA_JIT_FUZZ_SEED").expect("worker seed missing"));
    let cases: usize = std::env::var("LUNA_JIT_FUZZ_CASES")
        .unwrap_or_else(|_| "24".into())
        .parse()
        .unwrap();
    assert!((1..=10000).contains(&cases));
    let target = std::env::var("LUNA_JIT_FUZZ_TARGET").unwrap_or_else(|_| "all".into());
    assert!(matches!(
        target.as_str(),
        "all" | "admission" | "scalar" | "heap"
    ));
    let mut random = Random(seed);
    let mut invocations = 0;
    let mut instructions = 0;
    let mut heap_counts = heap::Counts::default();
    for case in 0..cases {
        eprintln!("seed={seed} target={target} case={case}/{cases}");
        if target == "heap" {
            heap_counts.add(heap::run(&mut random, seed, case));
            continue;
        }
        let mut snapshot = random.snapshot();
        let snapshot_ledger = snapshot.operations.allocator().0.clone();
        if target != "admission" {
            let (entries, completed) = scalar_case(&mut random, &snapshot, seed, case);
            invocations += entries;
            instructions += completed;
        }
        if target != "scalar" {
            invalidate(&mut snapshot, case % 11);
            reject(&snapshot);
        }
        drop(snapshot);
        assert_eq!(
            snapshot_ledger.current(),
            0,
            "seed={seed} case={case}: snapshot storage remained charged"
        );
    }
    if matches!(target.as_str(), "all" | "scalar") {
        assert!(instructions > 0);
    }
    if target == "heap" {
        let h = heap_counts;
        eprintln!("heap completed seed={seed} cases={} slices={} yields={} callbacks={} retirements={} host_reads={} userdata_observations={} native_instructions={} table_reads={} table_writes={} allocations={} upvalue_reads={} upvalue_writes={} declines={}", h.cases, h.slices, h.yields, h.callbacks, h.retirements, h.host_reads, h.userdata_observations, h.instructions, h.reads, h.writes, h.allocations, h.upvalue_reads, h.upvalue_writes, h.declines);
    } else {
        eprintln!("completed seed={seed} cases={cases} target={target} kernel_invocations={invocations} native_instructions={instructions}");
    }
}

fn directory() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        PathBuf::from("target/jit-evidence/fuzz").join(format!("{stamp}-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn supervise(
    directory: &Path,
    seed: u64,
    cases: usize,
    target: &str,
    failure: Option<&str>,
    timeout: Duration,
) -> io::Result<()> {
    let log = directory.join(format!("seed-{seed}-{}.log", failure.unwrap_or(target)));
    let file = File::create(&log)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "jit::fuzz::worker",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LUNA_JIT_FUZZ_SEED", seed.to_string())
        .env("LUNA_JIT_FUZZ_CASES", cases.to_string())
        .env("LUNA_JIT_FUZZ_TARGET", target)
        .env_remove("LUNA_JIT_FUZZ_TEST_FAILURE")
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file));
    if let Some(failure) = failure {
        command.env("LUNA_JIT_FUZZ_TEST_FAILURE", failure);
    }
    unsafe {
        command.pre_exec(|| {
            for (resource, value) in LIMITS {
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource as _, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                eprintln!(
                    "worker seed={seed} target={target} status={status} elapsed={:?} log={}",
                    started.elapsed(),
                    log.display()
                );
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "worker failed: {status}; log={}",
                        log.display()
                    )))
                };
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return match result {
                    Err(error) => Err(error),
                    _ => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("worker timed out; log={}", log.display()),
                    )),
                };
            }
        }
    }
}

#[test]
#[ignore]
fn supervisor_rejects_panics_signals_and_timeouts() {
    let directory = directory();
    for failure in ["panic", "signal", "timeout"] {
        let timeout = if failure == "timeout" {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(10)
        };
        let error = supervise(&directory, 0, 1, "all", Some(failure), timeout).unwrap_err();
        assert_eq!(
            error.kind(),
            if failure == "timeout" {
                io::ErrorKind::TimedOut
            } else {
                io::ErrorKind::Other
            }
        );
    }
    supervise(&directory, 0, 1, "all", None, Duration::from_secs(30)).unwrap();
}

#[test]
#[ignore]
fn supervisor() {
    let directory = directory();
    let seeds = std::env::var("LUNA_JIT_FUZZ_SEEDS")
        .unwrap_or_else(|_| "0,1,0xdeadbeef,0xffffffffffffffff".into());
    let seeds: Vec<_> = seeds.split(',').map(number).collect();
    assert!((1..=64).contains(&seeds.len()));
    let cases: usize = std::env::var("LUNA_JIT_FUZZ_CASES")
        .unwrap_or_else(|_| "24".into())
        .parse()
        .unwrap();
    assert!((1..=10000).contains(&cases));
    let target = std::env::var("LUNA_JIT_FUZZ_TARGET").unwrap_or_else(|_| "all".into());
    assert!(matches!(
        target.as_str(),
        "all" | "admission" | "scalar" | "heap"
    ));
    fs::write(directory.join("campaign.txt"), format!("seeds={seeds:?}\ncases_per_seed={cases}\ntarget={target}\nwall_seconds_per_worker=60\ncpu_seconds_per_worker=30\naddress_space_bytes=2147483648\n" )).unwrap();
    let platform = format!(
        "{}-unknown-linux-{}",
        std::env::consts::ARCH,
        if cfg!(target_env = "musl") {
            "musl"
        } else {
            "gnu"
        }
    );
    fs::write(directory.join("runtime.txt"), format!("luna={}\narchitecture={}\nos={}\nrust_target={platform}\ndebug_assertions={}\nworker_binary={}\nreproduce=nix develop -c make jit-fuzz TARGET={platform} FUZZ_TARGET={target} FUZZ_CASES={cases} FUZZ_SEEDS={}\n", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH, std::env::consts::OS, cfg!(debug_assertions), std::env::current_exe().unwrap().display(), seeds.iter().map(u64::to_string).collect::<Vec<_>>().join(","))).unwrap();
    eprintln!("campaign artifacts={}", directory.display());
    for seed in seeds {
        supervise(
            &directory,
            seed,
            cases,
            &target,
            None,
            Duration::from_secs(60),
        )
        .unwrap();
    }
}
