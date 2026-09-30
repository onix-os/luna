#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

#[path = "../examples/jit_support/suspension.rs"]
mod suspension;

fn check_observation(report: &suspension::Report, mode: suspension::Mode, fuel: i32) {
    assert!(report.steps >= 3);
    if fuel <= 64 {
        assert!(report.steps > 3);
    }
    assert!(report.counted_work_max <= fuel as u64 + 63);
    assert!(report.fuel_debit_max <= i64::from(fuel) + 67);
    assert!(report.elapsed_ns >= report.load_ns);
    assert!(report.poll_ns >= report.max_poll_ns);
    assert!(report.gc_peak > 0);
    assert!(report.code_peak >= report.stats.code_bytes);
    assert!(report.queue_peak <= 16);
    assert_eq!(report.native_table_write_segments.len(), 3);
    if mode != suspension::Mode::Off {
        let first = usize::from(mode == suspension::Mode::Auto && fuel == 65536);
        if first == 1 {
            assert_eq!(report.native_segments[0], 0);
            assert_eq!(report.native_table_write_segments[0], 0);
        }
        assert!(report.native_segments[first..]
            .iter()
            .all(|count| *count > 0));
        assert!(report.native_table_write_segments[first..]
            .iter()
            .all(|count| *count > 0));
    }
}

#[test]
fn public_coroutine_slices_resume_with_verified_tiered_heap_work() {
    for mode in [
        suspension::Mode::Off,
        suspension::Mode::Auto,
        suspension::Mode::Prepared,
    ] {
        for fuel in [1, 64, 65536] {
            let report = suspension::run("coroutine_resume", mode, fuel).unwrap();
            assert_eq!(report.boundary_values, [20100, 40200, 60300]);
            assert_eq!(report.suspensions, 2);
            assert_eq!(report.pending_polls, 0);
            assert_eq!(report.ready_polls, 0);
            assert_eq!(report.wakes, 0);
            assert_eq!(report.forced_gc_cycles, 4);
            check_observation(&report, mode, fuel);
        }
    }
}

#[cfg(feature = "async")]
#[test]
fn public_foreign_futures_park_outside_arena_and_resume_native_heap_work() {
    for mode in [
        suspension::Mode::Off,
        suspension::Mode::Auto,
        suspension::Mode::Prepared,
    ] {
        for fuel in [1, 64, 65536] {
            let report = suspension::run("foreign_await", mode, fuel).unwrap();
            assert_eq!(report.boundary_values, [60384]);
            assert_eq!(report.suspensions, 2);
            assert_eq!(report.pending_polls, 6);
            assert_eq!(report.ready_polls, 2);
            assert_eq!(report.wakes, 6);
            assert_eq!(report.forced_gc_cycles, 12);
            check_observation(&report, mode, fuel);
        }
    }
}
