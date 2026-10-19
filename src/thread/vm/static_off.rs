use std::cell::Cell;

thread_local! {
    static STATE: Cell<(bool, usize, usize)> = const { Cell::new((false, 0, 0)) };
}

pub(in crate::thread) fn reference_enabled() -> bool {
    STATE.with(|state| state.get().0)
}

pub(in crate::thread) fn record(specialized: bool) {
    STATE.with(|state| {
        let (reference, ordinary, selected) = state.get();
        state.set((
            reference,
            ordinary + usize::from(!specialized),
            selected + usize::from(specialized),
        ));
    });
}

pub(in crate::thread) fn scoped<R>(
    reference: bool,
    run: impl FnOnce() -> R,
) -> (R, (usize, usize)) {
    struct Restore((bool, usize, usize));
    impl Drop for Restore {
        fn drop(&mut self) {
            STATE.with(|state| state.set(self.0));
        }
    }
    let previous = STATE.with(|state| state.replace((reference, 0, 0)));
    let restore = Restore(previous);
    let result = run();
    let counts = STATE.with(|state| (state.get().1, state.get().2));
    drop(restore);
    (result, counts)
}

#[test]
fn selector_scope_restores_nested_and_panicking_observations() {
    let (_, counts) = scoped(false, || {
        assert!(!reference_enabled());
        record(true);
        let (_, nested) = scoped(true, || {
            assert!(reference_enabled());
            record(false);
        });
        assert_eq!(nested, (1, 0));
        let panic = std::panic::catch_unwind(|| {
            scoped(true, || {
                record(false);
                std::panic::panic_any(41_u64);
            });
        })
        .unwrap_err();
        assert_eq!(*panic.downcast::<u64>().unwrap(), 41);
        assert!(!reference_enabled());
        record(true);
    });
    assert_eq!(counts, (0, 2));
    assert!(!reference_enabled());
}
