use super::*;
use crate::{
    opcode::Operation, thread::activation::with_test_thread, Fuel, JitConfig, JitMode, Lua, Value,
};

fn fixture(test: impl for<'gc> FnOnce(Context<'gc>, Closure<'gc>, Region<'gc>, usize)) {
    fixture_source(
        b"local n=0 local function f(v) n=n+v end for i=1,20 do f(i) end return n",
        test,
    );
}

fn fixture_source(
    source: &[u8],
    test: impl for<'gc> FnOnce(Context<'gc>, Closure<'gc>, Region<'gc>, usize),
) {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, source).unwrap();
        let prototype = closure.prototype();
        let start = prototype
            .opcodes
            .iter()
            .position(|op| matches!(op.decode(), Operation::Closure { .. }))
            .unwrap()
            + 1;
        let pc = prototype
            .opcodes
            .iter()
            .position(|op| matches!(op.decode(), Operation::Call { .. }))
            .unwrap();
        let key = {
            let registry = ctx.jit_registry().borrow();
            super::super::pairs::Key {
                caller: registry.identity(ctx, prototype).unwrap(),
                callee: registry.identity(ctx, prototype.prototypes[0]).unwrap(),
                pc,
            }
        };
        let allocator = ctx.jit().0.borrow().snapshots.clone();
        let caller =
            || super::super::ir::Snapshot::new_in(&prototype, 4096, allocator.clone()).unwrap();
        ctx.jit().compile(key.caller, caller()).unwrap();
        ctx.jit().test_call_pairs(true);
        ctx.jit().observe_pair(key);
        ctx.jit()
            .compile_pair(
                key,
                caller(),
                super::super::ir::Snapshot::new_in(&prototype.prototypes[0], 4096, allocator)
                    .unwrap(),
            )
            .unwrap();
        let pair = PreparedPair {
            program: ctx.jit().pair_lease(key).unwrap(),
        };
        let region = Region::new(ctx, closure, pair).unwrap();
        test(ctx, closure, region, start);
    });
}

#[test]
fn rejected_region_entry_preserves_frames_values_fuel_and_work() {
    fixture(|ctx, closure, region, start| {
        let foreign = Closure::load(ctx, None, &b"return 7"[..]).unwrap();
        for case in 0..9 {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let mut removed = None;
                let (limit, budget) = match case {
                    0 => (0, 64),
                    1 => (8, 0),
                    2 => (8, 65),
                    3 => {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                        (8, 64)
                    }
                    4 => {
                        let hook = crate::Callback::from_fn(&ctx, |_, _, _| {
                            panic!("declined hook executed")
                        });
                        ctx.set_debug_hook(hook.into(), false, 1);
                        (8, 64)
                    }
                    5 => {
                        host.test_variable_stack();
                        (8, 64)
                    }
                    6 => {
                        host.test_replace_closure(foreign);
                        (8, 64)
                    }
                    7 => {
                        removed = ctx.jit().0.borrow_mut().code.remove(&region.source);
                        (8, 64)
                    }
                    8 => {
                        ctx.jit()
                            .0
                            .borrow_mut()
                            .pairs
                            .as_mut()
                            .unwrap()
                            .evict(region.pair.program.key());
                        (8, 64)
                    }
                    _ => unreachable!(),
                };
                let before_stats = stats(ctx);
                let before = trace(ctx, host, before_stats);
                assert!(
                    region.run(ctx, host, limit, budget).is_none(),
                    "case {case}"
                );
                assert_eq!(trace(ctx, host, before_stats), before, "case {case}");
                if let Some(code) = removed {
                    ctx.jit().0.borrow_mut().code.insert(region.source, code);
                }
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                ctx.set_debug_hook(Value::Nil, false, 0);
            });
        }
    });
}

#[test]
fn region_preserves_canonical_error_and_stops_before_callback_execution() {
    for (source, error) in [
        (
            &b"local n=0 local function f(v) n=n+v end f(3) local p=99 local x=1 return x.key,p,n"
                [..],
            true,
        ),
        (
            &b"local n=0 local function f(v) n=n+v end f(3) cb() return n"[..],
            false,
        ),
    ] {
        fixture_source(source, |ctx, closure, region, start| {
            let callback =
                crate::Callback::from_fn(&ctx, |_, _, _| panic!("callback executed inside region"));
            ctx.globals().set(ctx, "cb", callback).unwrap();
            let (native, result, slices) =
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, start as u32, 4).result.unwrap();
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let before = stats(ctx);
                    let outcome = region.run(ctx, host, 64, 64).unwrap();
                    assert_eq!(outcome.pairs, 1);
                    assert!(outcome.fragments >= 2);
                    assert_eq!(outcome.result.is_err(), error);
                    if !error {
                        assert!(!host.lua_ready());
                    }
                    (
                        trace(ctx, host, before),
                        format!("{:?}", outcome.result),
                        outcome.slices,
                    )
                });
            let (canonical, expected_result) =
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, start as u32, 4).result.unwrap();
                    let before = stats(ctx);
                    let outcome = host.run(ctx, slices, 64, 4);
                    (trace(ctx, host, before), format!("{:?}", outcome.result))
                });
            assert_eq!(result, expected_result);
            let (ordinary, ordinary_result) =
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, start as u32, 4).result.unwrap();
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let before = stats(ctx);
                    let outcome = host.run(ctx, slices, 64, 4);
                    (trace(ctx, host, before), format!("{:?}", outcome.result))
                });
            assert_eq!(result, ordinary_result);
            assert_eq!(native, ordinary);
            assert_eq!(native.5, canonical.5 + if error { 2 } else { 0 });
            assert_eq!(
                (&native.0, &native.1, native.2, native.3, native.4),
                (
                    &canonical.0,
                    &canonical.1,
                    canonical.2,
                    canonical.3,
                    canonical.4
                ),
            );
        });
    }
}

fn trace(
    ctx: Context<'_>,
    host: &ActivationHost<'_, '_>,
    before: (u64, u64),
) -> (Vec<String>, Vec<(u64, u64)>, usize, i32, u64, u64) {
    let (frames, values, open, fuel) = host.test_trace();
    let stats = ctx.jit().0.borrow().stats;
    (
        frames,
        values
            .into_iter()
            .map(abi::Slot::from_value)
            .map(|s| (s.tag, s.bits))
            .collect(),
        open,
        fuel,
        stats.total_dispatches - before.0,
        stats.native_instructions + stats.interpreted_instructions - before.1,
    )
}

fn stats(ctx: Context<'_>) -> (u64, u64) {
    let stats = ctx.jit().0.borrow().stats;
    (
        stats.total_dispatches,
        stats.native_instructions + stats.interpreted_instructions,
    )
}

#[test]
fn generated_region_matches_canonical_bounded_frames_fuel_and_work() {
    fixture(|ctx, closure, region, start| {
        for limit in [1, 2, 3, 4, 8, 64] {
            for budget in [1, 2, 3, 4, 8, 64] {
                for fuel in [-1, 0, 1, 8, 20, 10000] {
                    let (native, slices) =
                        with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                            host.run(ctx, 1, start as u32, 4).result.unwrap();
                            host.test_fuel(Fuel::with(fuel));
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                            let before = stats(ctx);
                            let outcome = region.run(ctx, host, limit, budget).unwrap();
                            outcome.result.unwrap();
                            assert!(outcome.slices > 0 && outcome.slices <= limit);
                            assert!(outcome.fragments > 0 && outcome.fragments <= limit);
                            if (limit, budget, fuel) == (64, 64, 10000) {
                                assert_eq!(outcome.pairs, 20);
                                assert!(outcome.fragments > 20);
                                assert!(matches!(
                                    host.test_trace().1.as_slice(),
                                    [Value::Integer(210)]
                                ));
                            }
                            (trace(ctx, host, before), outcome.slices)
                        });
                    let canonical =
                        with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                            host.run(ctx, 1, start as u32, 4).result.unwrap();
                            host.test_fuel(Fuel::with(fuel));
                            let before = stats(ctx);
                            host.run(ctx, slices, budget, 4).result.unwrap();
                            trace(ctx, host, before)
                        });
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    assert_eq!(
                        native, canonical,
                        "limit={limit} budget={budget} fuel={fuel}"
                    );
                }
            }
        }
    });
}
