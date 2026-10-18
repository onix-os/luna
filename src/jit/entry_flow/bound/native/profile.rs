use super::*;

fn run(ordinary: &Code, resumed: &Code) -> [u64; 4] {
    let integer = |value: i64| Slot {
        tag: abi::INTEGER,
        bits: value as u64,
    };
    let mut slots = [integer(0); 7];
    slots[0] = integer(1);
    slots[1] = integer(80);
    slots[2] = integer(1);
    let mut pc = 0;
    let mut counts = [0; 4];
    loop {
        let code = if pc == 3 { resumed } else { ordinary };
        let exit = code.invoke(&mut slots, pc, 64);
        counts[0] += 1;
        counts[1] += u64::from(exit.instructions);
        pc = exit.pc as usize;
        match (pc, exit.reason) {
            (2, reason) if reason == Kind::Interpreter as u32 => {
                assert_eq!(slots[5].tag, abi::INTEGER);
                slots[4] = integer((slots[5].bits as i64).wrapping_add(1));
                counts[2] += 1;
                pc = 3;
            }
            (5, reason) if reason == Kind::Interpreter as u32 => {
                assert_eq!((slots[6].tag, slots[6].bits), (abi::INTEGER, 3320));
                counts[3] = slots[6].bits;
                return counts;
            }
            _ => panic!("unexpected native exit {pc} {}", exit.reason),
        }
    }
}

#[inline(never)]
fn profile_execute(ordinary: &Code, resumed: &Code) -> [u64; 4] {
    let mut totals = [0; 4];
    for _ in 0..1000 {
        let report = run(
            std::hint::black_box(ordinary),
            std::hint::black_box(resumed),
        );
        for (total, value) in totals.iter_mut().zip(report) {
            *total += value;
        }
    }
    std::hint::black_box(totals)
}

#[test]
fn repeated_resumption_matches_control_counts_and_results() {
    let snapshot = source();
    let ordinary = compile(&snapshot, Failure::None);
    let resumed = compile(&snapshot, Failure::RequireBoundEntry(3));
    let expected = run(&ordinary, &ordinary);
    assert_eq!(expected[0], 81);
    assert_eq!(expected[2], 80);
    assert_eq!(expected[3], 3320);
    for _ in 0..16 {
        assert_eq!(run(&ordinary, &resumed), expected);
    }
}

#[test]
#[ignore]
fn profile_bound_resumption() {
    let selected = std::env::var("LUNA_BOUND_ENTRY").unwrap();
    assert!(matches!(selected.as_str(), "0" | "1"));
    let snapshot = source();
    let ordinary = compile(&snapshot, Failure::None);
    let bound = compile(&snapshot, Failure::RequireBoundEntry(3));
    let resumed = if selected == "1" { &bound } else { &ordinary };
    let expected = run(&ordinary, &ordinary);
    for _ in 0..2 {
        assert_eq!(run(&ordinary, resumed), expected);
    }
    let totals = profile_execute(&ordinary, resumed);
    assert_eq!(totals, expected.map(|count| count * 1000));
    let directory = std::path::PathBuf::from(std::env::var_os("LUNA_JIT_DIAGNOSTIC_DIR").unwrap());
    std::fs::create_dir_all(&directory).unwrap();
    for (name, code) in [("ordinary", &ordinary), ("bound", &bound)] {
        let metadata = code.dump_finalized(&directory.join(format!("{name}.bin")));
        std::fs::write(directory.join(format!("{name}.metadata")), metadata).unwrap();
    }
    println!("bound={selected} repetitions=1000 totals={totals:?} validated=true scope=native-component-only");
}
