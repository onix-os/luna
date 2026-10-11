use std::cell::{Cell, RefCell};

use crate::{Callback, CallbackReturn, Context, Execution, JitMode, JitStats};

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COUNTS: Cell<[u64; 2]> = const { Cell::new([0; 2]) };
    static OBSERVATIONS: RefCell<Option<Vec<Observation>>> = const { RefCell::new(None) };
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Observation {
    kind: &'static str,
    stats: JitStats,
    frame: Option<(usize, usize)>,
    fuel: Option<(i32, bool)>,
}

pub(super) fn enabled(ctx: crate::Context<'_>) -> bool {
    ENABLED.get() && ctx.jit().active()
}

pub(super) fn record(kind: usize) {
    let mut counts = COUNTS.get();
    counts[kind - 1] += 1;
    COUNTS.set(counts);
}

pub(super) fn capture<R>(enabled: bool, f: impl FnOnce() -> R) -> (R, [u64; 2], Vec<Observation>) {
    struct Reset(bool, [u64; 2], Option<Vec<Observation>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            ENABLED.set(self.0);
            COUNTS.set(self.1);
            OBSERVATIONS.with_borrow_mut(|observations| *observations = self.2.take());
        }
    }
    let _reset = Reset(
        ENABLED.replace(enabled),
        COUNTS.replace([0; 2]),
        OBSERVATIONS.replace(Some(Vec::new())),
    );
    let result = f();
    (result, COUNTS.get(), OBSERVATIONS.take().unwrap())
}

pub(super) fn observe_slice(stats: JitStats) {
    OBSERVATIONS.with_borrow_mut(|observations| {
        if let Some(observations) = observations {
            observations.push(Observation {
                kind: "slice",
                stats,
                frame: None,
                fuel: None,
            });
        }
    });
}

pub(super) fn observe_prepared(stats: JitStats) {
    OBSERVATIONS.with_borrow_mut(|observations| {
        if let Some(observations) = observations {
            observations.push(Observation {
                kind: "prepared",
                stats,
                frame: None,
                fuel: None,
            });
        }
    });
}

pub(super) fn check_equivalent(mut actual: Vec<Observation>, mut expected: Vec<Observation>) {
    fn normalize(observations: &mut [Observation]) {
        let initial = observations.first().unwrap();
        assert_eq!(initial.kind, "prepared");
        let peaks = (
            initial.stats.snapshot_peak_bytes,
            initial.stats.metadata_peak_bytes,
            initial.stats.accounted_jit_peak_bytes,
        );
        for observation in observations {
            if observation.kind != "callback" {
                let stats = &mut observation.stats;
                assert_eq!(
                    (
                        stats.snapshot_peak_bytes,
                        stats.metadata_peak_bytes,
                        stats.accounted_jit_peak_bytes
                    ),
                    peaks
                );
                stats.snapshot_peak_bytes = 0;
                stats.metadata_peak_bytes = 0;
                stats.accounted_jit_peak_bytes = 0;
            }
        }
    }
    normalize(&mut actual);
    normalize(&mut expected);
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(actual, expected, "observation {index}");
    }
}

fn observe_callback(ctx: Context<'_>, execution: &mut Execution<'_, '_>) {
    let stats = ctx.jit().0.borrow_mut().stats;
    let frame = execution
        .upper_lua_frame()
        .map(|frame| (frame.base, frame.pc));
    let fuel = execution.fuel();
    OBSERVATIONS.with_borrow_mut(|observations| {
        observations.as_mut().unwrap().push(Observation {
            kind: "callback",
            stats,
            frame,
            fuel: Some((fuel.remaining(), fuel.is_interrupted())),
        });
    });
}

pub(super) fn install(ctx: Context<'_>) {
    if !OBSERVATIONS.with_borrow(Option::is_some) {
        return;
    }
    ctx.set_global(
        "handoff_panic",
        Callback::from_fn(&ctx, |ctx, mut execution, _| {
            observe_callback(ctx, &mut execution);
            std::panic::panic_any(731u64)
        }),
    );
    ctx.set_global(
        "handoff_value",
        Callback::from_fn(&ctx, |ctx, mut execution, mut stack| {
            observe_callback(ctx, &mut execution);
            let value = stack.consume::<i64>(ctx)?;
            stack.replace(ctx, value + 1);
            Ok(CallbackReturn::Return)
        }),
    );
    ctx.set_global(
        "handoff_mode",
        Callback::from_fn(&ctx, |ctx, mut execution, mut stack| {
            observe_callback(ctx, &mut execution);
            let enabled = stack.consume::<bool>(ctx)?;
            let mut manager = ctx.jit().0.borrow_mut();
            let mut config = manager.config.clone();
            config.mode = if enabled { JitMode::Auto } else { JitMode::Off };
            manager.configure(config);
            Ok(CallbackReturn::Return)
        }),
    );
    ctx.set_global(
        "handoff_clear",
        Callback::from_fn(&ctx, |ctx, mut execution, _| {
            observe_callback(ctx, &mut execution);
            ctx.jit().0.borrow_mut().clear();
            Ok(CallbackReturn::Return)
        }),
    );
}

#[test]
fn callback_handoff_capture_restores_after_panic_and_nested_scopes() {
    assert!(!ENABLED.get());
    let failed = std::panic::catch_unwind(|| capture(true, || panic!("handoff panic")));
    assert_eq!(
        failed.unwrap_err().downcast_ref::<&str>(),
        Some(&"handoff panic")
    );
    assert!(!ENABLED.get());
    assert_eq!(COUNTS.get(), [0; 2]);
    assert!(OBSERVATIONS.with_borrow(Option::is_none));
    let (_, counts, _) = capture(true, || {
        record(1);
        let (_, nested, _) = capture(false, || {
            assert!(!ENABLED.get());
            record(2);
        });
        assert_eq!(nested, [0, 1]);
        assert!(ENABLED.get());
        assert_eq!(COUNTS.get(), [1, 0]);
    });
    assert_eq!(counts, [1, 0]);
}
