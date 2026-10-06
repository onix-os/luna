use super::*;
use std::{hint::black_box, time::Instant};

#[allow(dead_code)]
#[path = "../../../../examples/jit_support/workloads.rs"]
mod workloads;

fn setting(name: &str, default: usize, maximum: usize) -> usize {
    let value = std::env::var(name).map_or(default, |value| value.parse().unwrap());
    assert!((1..=maximum).contains(&value));
    value
}

#[test]
#[ignore = "paired release diagnostic, not a performance acceptance gate"]
fn paired_region_cost() {
    let samples = setting("LUNA_REGION_SAMPLES", 21, 101);
    let iterations = setting("LUNA_REGION_ITERATIONS", 8, 100);
    let workload = workloads::WORKLOADS
        .iter()
        .find(|case| case.name == "closure_upvalue")
        .unwrap();
    fixture_source(workload.source, |ctx, closure, region, start| {
        let mut times: [Vec<u128>; 3] = std::array::from_fn(|_| Vec::new());
        for round in 0..samples + 3 {
            let mut expected = None;
            for offset in 0..3 {
                let mode = (round + offset) % 3;
                let mut elapsed = 0;
                let mut counters = [0_u64; 8];
                for _ in 0..iterations {
                    let state = with_test_thread(ctx, closure, &mut Fuel::with(1000000), |host| {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                        host.run(ctx, 1, start as u32, 4).result.unwrap();
                        ctx.jit().0.borrow_mut().config.mode = if mode == 0 {
                            JitMode::Off
                        } else {
                            JitMode::Auto
                        };
                        let before = ctx.jit().0.borrow().stats;
                        let timer = Instant::now();
                        let mut pairs = 0;
                        let mut fragments = 0;
                        let mut fallbacks = 0;
                        while host.lua_ready() {
                            if mode == 2 {
                                if let Some(outcome) = region.run(ctx, host, 64, 64) {
                                    outcome.result.unwrap();
                                    pairs += outcome.pairs;
                                    fragments += outcome.fragments;
                                } else {
                                    fallbacks += 1;
                                    host.run(ctx, 64, 64, 4).result.unwrap();
                                }
                            } else {
                                host.run(ctx, 64, 64, 4).result.unwrap();
                            }
                            assert!(host.fuel().should_continue());
                        }
                        elapsed += timer.elapsed().as_nanos();
                        let after = ctx.jit().0.borrow().stats;
                        let state = trace(
                            ctx,
                            host,
                            (
                                before.total_dispatches,
                                before.native_instructions + before.interpreted_instructions,
                            ),
                        );
                        assert_eq!(state.1, vec![(abi::INTEGER, workload.expected as u64)]);
                        if mode == 2 {
                            assert_eq!(pairs, 10000);
                            assert_eq!(fragments, 10001);
                            assert_eq!(fallbacks, 0);
                        }
                        if mode != 0 {
                            assert!(after.native_instructions > before.native_instructions);
                        }
                        for (total, value) in counters.iter_mut().zip([
                            after.native_instructions - before.native_instructions,
                            after.helper_instructions - before.helper_instructions,
                            after.native_entries - before.native_entries,
                            after.code_lookups - before.code_lookups,
                            after.code_leases - before.code_leases,
                            pairs as u64,
                            fragments as u64,
                            fallbacks as u64,
                        ]) {
                            *total += value;
                        }
                        black_box(state)
                    });
                    if let Some(expected) = &expected {
                        assert_eq!(&state, expected);
                    } else {
                        expected = Some(state);
                    }
                }
                if round >= 3 {
                    times[mode].push(elapsed);
                    println!("case={} sample={} order={} mode={} iterations={} ns={} native={} helpers={} entries={} lookups={} leases={} pairs={} fragments={} fallbacks={}",
                        workload.name, round - 3, offset, ["off", "auto", "region"][mode], iterations, elapsed,
                        counters[0], counters[1], counters[2], counters[3], counters[4], counters[5], counters[6], counters[7]);
                }
            }
        }
        let medians = times.map(|mut values| {
            values.sort_unstable();
            values[values.len() / 2]
        });
        println!("diagnostic_only=true off_ns={} auto_ns={} region_ns={} region_over_auto={:.6} region_over_off={:.6}",
            medians[0], medians[1], medians[2], medians[2] as f64 / medians[1] as f64, medians[2] as f64 / medians[0] as f64);
    });
}

fn profile_body<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    region: &Region<'gc>,
    mode: &str,
) -> [usize; 3] {
    let mut counts = [0; 3];
    while host.lua_ready() {
        if mode == "region" {
            if let Some(outcome) = region.run(ctx, host, 64, 64) {
                outcome.result.unwrap();
                counts[0] += outcome.pairs;
                counts[1] += outcome.fragments;
            } else {
                counts[2] += 1;
                host.run(ctx, 64, 64, 4).result.unwrap();
            }
        } else {
            host.run(ctx, 64, 64, 4).result.unwrap();
        }
        assert!(host.fuel().should_continue());
    }
    counts
}

#[inline(never)]
fn profile_region<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    region: &Region<'gc>,
    mode: &str,
) -> [usize; 3] {
    black_box(profile_body(ctx, host, region, mode))
}

#[test]
#[ignore = "isolated instruction profile, not timing acceptance"]
fn region_cost_profile() {
    let mode = std::env::var("LUNA_REGION_PROFILE_MODE").unwrap_or_else(|_| "region".into());
    assert!(["off", "auto", "region"].contains(&mode.as_str()));
    let iterations = setting("LUNA_REGION_ITERATIONS", 3, 100);
    let workload = workloads::WORKLOADS
        .iter()
        .find(|case| case.name == "closure_upvalue")
        .unwrap();
    fixture_source(workload.source, |ctx, closure, region, start| {
        let expected = with_test_thread(ctx, closure, &mut Fuel::with(1000000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            let before = stats(ctx);
            profile_body(ctx, host, &region, "off");
            trace(ctx, host, before)
        });
        assert_eq!(expected.1, vec![(abi::INTEGER, workload.expected as u64)]);
        for iteration in 0..iterations {
            with_test_thread(ctx, closure, &mut Fuel::with(1000000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                ctx.jit().0.borrow_mut().config.mode = if mode == "off" {
                    JitMode::Off
                } else {
                    JitMode::Auto
                };
                let before = ctx.jit().0.borrow().stats;
                let counts = profile_region(ctx, host, &region, &mode);
                let after = ctx.jit().0.borrow().stats;
                assert_eq!(
                    trace(
                        ctx,
                        host,
                        (
                            before.total_dispatches,
                            before.native_instructions + before.interpreted_instructions
                        )
                    ),
                    expected
                );
                if mode == "region" {
                    assert_eq!(counts, [10000, 10001, 0]);
                }
                if mode != "off" {
                    assert_eq!(
                        after.native_instructions - before.native_instructions,
                        60006
                    );
                }
                println!("profile_verified=true mode={mode} iteration={iteration} native={} pairs={} fragments={} fallbacks={}", after.native_instructions - before.native_instructions, counts[0], counts[1], counts[2]);
            });
        }
    });
}
