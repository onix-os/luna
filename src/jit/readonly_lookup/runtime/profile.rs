use super::{
    tests::counters,
    timing::{prepare, Prepared},
};
use crate::Executor;
use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
};

#[path = "../../../../examples/jit_support/profile_control.rs"]
mod profile_control;

#[inline(never)]
fn execute_window(state: &mut Prepared, iterations: usize) {
    for _ in 0..iterations {
        let executor = state
            .lua
            .enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&state.closure).into(), ())));
        assert_eq!(
            std::hint::black_box(state.lua.execute::<i64>(&executor).unwrap()),
            state.expected
        );
    }
}

fn run<W: Write, R: Read>(
    name: &str,
    mode: &str,
    iterations: usize,
    control: &mut profile_control::Control<W, R>,
) -> io::Result<([u64; 25], usize, usize)> {
    assert!((1..=10000).contains(&iterations));
    assert!(["off", "ordinary", "readonly"].contains(&mode));
    let mut state = prepare(name, mode);
    let before = state.lua.jit_stats();
    control.enabled(|| {
        execute_window(&mut state, iterations);
        Ok::<_, io::Error>(())
    })?;
    let after = state.lua.jit_stats();
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert_eq!(after.installed_regions, before.installed_regions);
    assert_eq!(after.compilation_failures, 0);
    assert_eq!(after.code_bytes, before.code_bytes);
    assert_eq!(after.code_requested_bytes, before.code_requested_bytes);
    assert_eq!(
        after.native_instructions > before.native_instructions,
        mode != "off"
    );
    let (a, b) = (counters(after), counters(before));
    let delta = std::array::from_fn(|i| a[i] - b[i]);
    state.lua.clear_jit_cache();
    assert_eq!(state.lua.jit_stats().code_bytes, 0);
    assert_eq!(state.lua.jit_stats().code_requested_bytes, 0);
    Ok((delta, state.code_count, state.child_count))
}

#[test]
fn acknowledged_windows_preserve_frozen_counts_and_release_code() {
    for case in ["rust_callbacks", "closure_upvalue"] {
        let mut samples = Vec::new();
        for mode in ["off", "ordinary", "readonly"] {
            let mut control = profile_control::Control::new(Vec::new(), &b"ack\nack\n"[..]);
            samples.push(run(case, mode, 2, &mut control).unwrap());
        }
        assert_eq!(samples[0].0[0], samples[1].0[0]);
        for i in 0..25 {
            let removed = if case == "rust_callbacks" && [16, 17].contains(&i) {
                10000
            } else {
                0
            };
            assert_eq!(
                samples[1].0[i],
                samples[2].0[i] + removed,
                "{case} counter={i}"
            );
        }
    }
}

#[test]
fn invalid_acknowledgement_does_not_enter_the_profile_window() {
    let mut control = profile_control::Control::new(Vec::new(), &b"bad\n"[..]);
    assert_eq!(
        run("rust_callbacks", "readonly", 1, &mut control)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
#[ignore]
fn profile_warmed_runtime() {
    let case = std::env::var("LUNA_READONLY_CASE").unwrap();
    let mode = std::env::var("LUNA_READONLY_MODE").unwrap();
    let iterations = std::env::var("LUNA_READONLY_ITERATIONS")
        .unwrap()
        .parse()
        .unwrap();
    let commands = OpenOptions::new()
        .write(true)
        .open(std::env::var_os("LUNA_READONLY_CONTROL").unwrap())
        .unwrap();
    let acknowledgements = OpenOptions::new()
        .read(true)
        .open(std::env::var_os("LUNA_READONLY_ACK").unwrap())
        .unwrap();
    let mut control = profile_control::Control::new(commands, acknowledgements);
    let (counters, code_count, child_count) = run(&case, &mode, iterations, &mut control).unwrap();
    println!("\nreadonly_profile_protocol=1 mode={mode} case={case} warmups=2 drained=true iterations={iterations} verified=1 compilation_delta=0 code_count={code_count} child_count={child_count} counters={counters:?}");
    println!("scope=warmed_execution_lifecycle elapsed_acceptance=false");
}
