use std::cell::Cell;

use crate::Fuel;

thread_local! {
    static STATE: Cell<(bool, usize, usize)> = const { Cell::new((false, 0, 0)) };
}

pub(super) fn apply(fuel: &mut Fuel, amount: i32, legacy_charged: bool) {
    assert!(amount == 0 || amount == super::Executor::FUEL_PER_STEP);
    assert_eq!(
        amount,
        if legacy_charged {
            0
        } else {
            super::Executor::FUEL_PER_STEP
        }
    );
    let reference = STATE.with(|state| {
        let (reference, zero, normal) = state.get();
        state.set((
            reference,
            zero + usize::from(amount == 0),
            normal + usize::from(amount != 0),
        ));
        reference
    });
    if reference {
        if !legacy_charged {
            fuel.consume(super::Executor::FUEL_PER_STEP);
        }
    } else {
        fuel.consume(amount);
    }
}

fn scoped<R>(reference: bool, run: impl FnOnce() -> R) -> (R, (usize, usize)) {
    struct Restore((bool, usize, usize));
    impl Drop for Restore {
        fn drop(&mut self) {
            STATE.with(|state| state.set(self.0));
        }
    }
    let restore = Restore(STATE.with(|state| state.replace((reference, 0, 0))));
    let result = run();
    let counts = STATE.with(|state| (state.get().1, state.get().2));
    drop(restore);
    (result, counts)
}

#[test]
fn zero_and_normal_charges_match_at_signed_edges_and_interruption() {
    let mut values = vec![
        i32::MIN,
        i32::MIN + 1,
        i32::MIN + 4,
        -5,
        -1,
        0,
        1,
        3,
        4,
        5,
        i32::MAX - 4,
        i32::MAX,
    ];
    let mut seed = 0x7a51_39e1_u32;
    for _ in 0..4096 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        values.push(seed as i32);
    }
    for remaining in values {
        for interrupted in [false, true] {
            for amount in [0, super::Executor::FUEL_PER_STEP] {
                let run = |reference| {
                    scoped(reference, || {
                        let mut fuel = Fuel::with(remaining);
                        if interrupted {
                            fuel.interrupt();
                        }
                        apply(&mut fuel, amount, amount == 0);
                        (fuel.remaining(), fuel.is_interrupted())
                    })
                };
                let (expected, original_route) = run(true);
                let (actual, selected_route) = run(false);
                assert_eq!(actual, expected);
                assert_eq!(actual, (remaining.saturating_sub(amount), interrupted));
                assert_eq!(original_route, selected_route);
                assert_eq!(
                    selected_route,
                    (usize::from(amount == 0), usize::from(amount != 0))
                );
            }
        }
    }
}

#[test]
fn scopes_restore_nested_and_panicking_selection() {
    let (_, outer) = scoped(false, || {
        apply(&mut Fuel::empty(), 0, true);
        let (_, inner) = scoped(true, || apply(&mut Fuel::empty(), 4, false));
        assert_eq!(inner, (0, 1));
        let error = std::panic::catch_unwind(|| scoped(true, || std::panic::panic_any(29_u64)));
        assert_eq!(*error.unwrap_err().downcast::<u64>().unwrap(), 29);
        apply(&mut Fuel::empty(), 4, false);
    });
    assert_eq!(outer, (1, 1));
}

#[test]
#[cfg(not(miri))]
fn amount_and_boolean_paths_preserve_public_executor_boundaries() {
    use super::activation_tests::run_mode_scoped;
    let scripts = [
        "local n=0 for i=1,12 do n=n+i end return n",
        "local n=0 local function f(x) n=n+x end for i=1,12 do f(1) end return n",
        "local function f(...) return ... end local function g(...) return f(...) end return g(17)",
        "local t=setmetatable({n=2},{__add=function(a,b) return a.n+b.n end}) return t+t",
        "local ok=pcall(function() return 1+{} end) return ok and 0 or 1",
        "local n=0 do local x <close> = setmetatable({}, {__close=function() n=n+1 end}) end return n",
        "local n=0 for i=1,4 do n=boundary(n) end return n",
        "local ticks=0 debug.sethook(function() ticks=ticks+1 end,'',1) local n=0 for i=1,4 do n=n+i end debug.sethook() return n",
    ];
    let mut totals = (0, 0);
    for (case, source) in scripts.into_iter().enumerate() {
        for native in [false, true] {
            for host_limit in [0, 64] {
                for budget in [0, 1, 17, 4096] {
                    let run =
                        || run_mode_scoped(source, budget, native, false, None, false, host_limit);
                    let (expected, original_route) = scoped(true, run);
                    let (actual, selected_route) = scoped(false, run);
                    assert_eq!(
                        expected, actual,
                        "case={case} native={native} host={host_limit} budget={budget}"
                    );
                    assert_eq!(original_route, selected_route);
                    totals.0 += selected_route.0;
                    totals.1 += selected_route.1;
                }
            }
        }
    }
    assert!(totals.0 > 0, "activation host never charged internally");
    assert!(totals.1 > 0, "ordinary outer step was never charged");
}

#[test]
fn interpreted_host_and_ordinary_routes_match_without_native_code() {
    use super::activation_tests::run_mode_scoped;
    let source = "local n=0 local function f(x) n=n+x end for i=1,3 do f(1) end return n";
    let mut total = (0, 0);
    for budget in [0, 17] {
        for limit in [0, 4] {
            let run = || run_mode_scoped(source, budget, false, false, None, false, limit);
            let (expected, old) = scoped(true, run);
            let (actual, new) = scoped(false, run);
            assert_eq!(expected, actual);
            assert_eq!(old, new);
            total.0 += new.0;
            total.1 += new.1;
        }
    }
    assert!(total.0 > 0 && total.1 > 0);
}

#[test]
fn mismatched_accounting_representations_are_rejected() {
    for (amount, charged) in [(0, false), (4, true)] {
        let mut fuel = Fuel::with(19);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply(&mut fuel, amount, charged);
        }));
        assert!(result.is_err());
        assert_eq!(fuel.remaining(), 19);
    }
}
