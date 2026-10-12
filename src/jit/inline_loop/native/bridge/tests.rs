use super::*;
use crate::{jit::work, thread::activation::with_test_thread, Closure, Fuel, JitConfig, Lua};

fn fixture(
    source: &[u8],
    upper: bool,
    test: impl for<'gc> FnOnce(Context<'gc>, Closure<'gc>, Binding<'gc>, &Plan<'_>),
) {
    Lua::empty().enter(|ctx| {
        let root = Closure::load(ctx, None, source).unwrap();
        let caller = if upper {
            root.prototype().prototypes[0]
        } else {
            root.prototype()
        };
        let callee = caller.prototypes[0];
        let caller_source = Snapshot::new(&caller, 4096, 1 << 20).unwrap();
        let callee_source = Snapshot::new(&callee, 4096, 1 << 20).unwrap();
        let pc = caller_source
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Call { .. }))
            .unwrap();
        let plan = Plan::new(
            &caller_source,
            &callee_source,
            pc,
            work::Limits::from(&JitConfig::default()),
        )
        .unwrap();
        let binding = Binding::new(&plan, caller, callee).unwrap();
        test(ctx, root, binding, &plan);
    });
}

fn position<'gc>(ctx: Context<'gc>, host: &mut ActivationHost<'gc, '_>, binding: &Binding<'gc>) {
    for _ in 0..64 {
        if host.with_registers(|caller, registers| {
            Gc::ptr_eq(caller.prototype(), binding.caller)
                && *registers.pc == binding.descriptor.call
        }) {
            return;
        }
        host.run(ctx, 1, 1, 0).result.unwrap();
    }
    panic!("call site was not reached");
}

fn identical<'gc>(
    actual: &[LuaValue<'gc>],
    expected: &[LuaValue<'gc>],
    actual_roots: &[LuaValue<'gc>],
    expected_roots: &[LuaValue<'gc>],
) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        match (*actual, *expected) {
            (LuaValue::Nil, LuaValue::Nil) => {}
            (LuaValue::Integer(a), LuaValue::Integer(b)) => assert_eq!(a, b),
            (LuaValue::Number(a), LuaValue::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (LuaValue::Boolean(a), LuaValue::Boolean(b)) => assert_eq!(a, b),
            (LuaValue::Table(a), LuaValue::Table(b)) => {
                let actual = actual_roots
                    .iter()
                    .position(|root| matches!(root, LuaValue::Table(root) if *root == a));
                let expected = expected_roots
                    .iter()
                    .position(|root| matches!(root, LuaValue::Table(root) if *root == b));
                assert!(actual.is_some() && expected.is_some());
                assert_eq!(actual, expected);
            }
            (LuaValue::Function(a), LuaValue::Function(b)) => {
                let actual = actual_roots
                    .iter()
                    .position(|root| matches!(root, LuaValue::Function(root) if *root == a));
                let expected = expected_roots
                    .iter()
                    .position(|root| matches!(root, LuaValue::Function(root) if *root == b));
                assert!(actual.is_some() && expected.is_some());
                assert_eq!(actual, expected);
            }
            _ => panic!("unexpected values {actual:?} {expected:?}"),
        }
    }
}

#[test]
fn scoped_batches_preserve_physical_calls_pending_prefixes_and_fuel() {
    let mut completed = 0;
    let mut batched = 0;
    let mut pending = 0;
    for (source, upper) in [
        (&b"local x=7 local function f(v) x=x+v end for i=1,12 do f(i) end return x"[..], false),
        (&b"local x=7 local function f(v) x=x-v end for i=31,-10,-3 do f(i) end return x"[..], false),
        (&b"local x=3 local function f() x=x*2 end for i=1,17 do f() end return x"[..], false),
        (&b"local x=7 local function outer() local function f(v) x=x+v end for i=1,12 do f(i) end return x end local y=outer() return y"[..], true),
        (&b"local x=9223372036854775807 local function f(v) x=x+v end for i=1,4 do f(i) end return x"[..], false),
        (&b"local x=7 local t={} local function f(v,w) x=x+v end for i=1,12 do f(i,t) end return x"[..], false),
    ] {
        let before = completed;
        fixture(source, upper, |ctx, root, binding, plan| {
            super::super::tests::with_entry(plan, |entry| {
                for budget in [0, 1, 3, 4, 8, 32, 64, 65] {
                    for limit in [0, 1, 2, 3, 4, 8, 16, 64, usize::MAX] {
                        for prefix in [0, 1, 3, 31, 63, 64] {
                            for fuel in [-1, 0, 8, 9, 10, 11, 20, 64, 1000, i32::MAX] {
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                                let result = with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
                                    position(ctx, host, &binding);
                                    host.test_fuel(Fuel::with(fuel));
                                    let before = host.test_trace();
                                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                                    let result = binding.run(ctx, host, limit, budget, prefix, |state| unsafe { entry(state) });
                                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                                    let after = host.test_trace();
                                    if result.is_none() {
                                        assert_eq!(format!("{before:?}"), format!("{after:?}"));
                                    }
                                    result.map(|progress| (progress, after, before.1))
                                });
                                let Some((progress, actual, actual_roots)) = result else { continue };
                                assert!(progress.slices as usize <= limit);
                                assert_eq!(progress.slices, progress.calls * 2);
                                let (expected, expected_roots) = with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
                                    position(ctx, host, &binding);
                                    let roots = host.test_trace().1;
                                    host.test_fuel(Fuel::with(fuel));
                                    host.charge_instructions(prefix);
                                    let outcome = host.run(ctx, progress.slices as usize, budget, 4);
                                    outcome.result.unwrap();
                                    assert_eq!(outcome.activations, progress.slices as usize);
                                    if progress.pending != 0 {
                                        host.run(ctx, 1, progress.pending, 0).result.unwrap();
                                    }
                                    (host.test_trace(), roots)
                                });
                                assert_eq!(actual.0, expected.0);
                                identical(&actual.1, &expected.1, &actual_roots, &expected_roots);
                                assert_eq!(actual.2, expected.2);
                                assert_eq!(actual.3 - progress.pending as i32, expected.3);
                                completed += 1;
                                batched += usize::from(progress.calls > 1);
                                pending += usize::from(progress.pending > 0);
                            }
                        }
                    }
                }
            });
        });
        assert!(completed > before, "upper={upper} source={source:?}");
    }
    assert!(
        completed > 1000 && batched > 100 && pending > 10,
        "completed={completed} batched={batched} pending={pending}"
    );
}

#[test]
fn invalid_bindings_and_precommit_failures_leave_the_host_unchanged() {
    fixture(
        &b"local x=7 local function f(v) x=x+v end for i=1,12 do f(i) end return x"[..],
        false,
        |ctx, root, binding, plan| {
            super::super::tests::with_entry(plan, |entry| {
                for fault in 0..14 {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
                        position(ctx, host, &binding);
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                        let before = host.test_trace();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            binding.run(ctx, host, 16, 64, 0, |state| {
                                if fault == 0 {
                                    panic!("before native entry");
                                }
                                unsafe { entry(state) };
                                match fault {
                                    1 => panic!("before publication"),
                                    2 => state.calls = u32::MAX,
                                    3 => state.instructions = u32::MAX,
                                    4 => state.pc = u64::MAX,
                                    5 => state.fuel += 1,
                                    6 => state.budget += 1,
                                    7 => state.reason = INVALID,
                                    8 => state.interrupted = 1,
                                    9 => state.root ^= 1,
                                    10 => state.slots[0].tag = u64::MAX,
                                    11 => {
                                        state.slots[0] = abi::Slot {
                                            tag: abi::REFERENCE,
                                            bits: u64::MAX,
                                        }
                                    }
                                    12 => state.upper.tag = abi::NUMBER,
                                    13 => state.capture ^= 1,
                                    _ => unreachable!(),
                                }
                            })
                        }));
                        if fault < 2 {
                            assert!(result.is_err());
                        } else {
                            assert!(result.unwrap().is_none(), "fault={fault}");
                        }
                        assert_eq!(
                            format!("{:?}", host.test_trace()),
                            format!("{before:?}"),
                            "fault={fault}"
                        );
                    });
                }
            });
            let foreign = Closure::load(ctx, None, &b"return 9"[..]).unwrap();
            assert!(Binding::new(plan, foreign.prototype(), binding.callee,).is_none());
            assert!(Binding::new(plan, binding.caller, foreign.prototype(),).is_none());
        },
    );
}

#[test]
fn runtime_refusals_and_post_publication_unwind_keep_canonical_state() {
    fixture(
        &b"local x=7 local function f(v) x=x+v end for i=1,12 do f(i) end return x"[..],
        false,
        |ctx, root, binding, plan| {
            super::super::tests::with_entry(plan, |entry| {
                for fault in 0..8 {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
                        position(ctx, host, &binding);
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                        match fault {
                            0 => ctx.jit().0.borrow_mut().config.mode = JitMode::Off,
                            1 => host.test_variable_stack(),
                            2 => {
                                let mut fuel = Fuel::with(10000);
                                fuel.interrupt();
                                host.test_fuel(fuel);
                            }
                            3 => host.with_registers(|_, r| *r.pc += 1),
                            4 => host.with_registers(|_, r| {
                                r.stack_frame[usize::from(binding.descriptor.function.0)] =
                                    LuaValue::Nil
                            }),
                            5 => {
                                host.with_registers(|_, r| r.stack_frame[0] = LuaValue::Number(7.5))
                            }
                            6 => host.with_registers(|_, r| {
                                r.stack_frame[usize::from(binding.descriptor.function.0) + 1] =
                                    LuaValue::Number(2.5)
                            }),
                            7 => host.test_replace_closure(
                                Closure::load(ctx, None, &b"return 9"[..]).unwrap(),
                            ),
                            _ => unreachable!(),
                        }
                        let before = host.test_trace();
                        let mut entered = 0;
                        assert!(binding
                            .run(ctx, host, 16, 64, 0, |state| {
                                entered += 1;
                                unsafe { entry(state) };
                            })
                            .is_none());
                        assert_eq!(entered, usize::from(fault == 6));
                        assert_eq!(format!("{:?}", host.test_trace()), format!("{before:?}"));
                    });
                }
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
                    position(ctx, host, &binding);
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let mut committed = None;
                    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let progress = binding
                            .run(ctx, host, 16, 64, 0, |state| unsafe { entry(state) })
                            .unwrap();
                        assert!(progress.calls > 1);
                        committed = Some(host.test_trace());
                        panic!("after publication");
                    }));
                    assert!(panic.is_err());
                    assert_eq!(
                        format!("{:?}", host.test_trace()),
                        format!("{:?}", committed.unwrap())
                    );
                    assert!(ctx.jit().0.try_borrow_mut().is_ok());
                });
            });
        },
    );
}

#[test]
fn runtime_descriptor_does_not_retain_source_workspace() {
    fixture(
        &b"local x=7 local function f(v) x=x+v end for i=1,12 do f(i) end return x"[..],
        false,
        |ctx, root, original, plan| {
            super::super::tests::with_entry(plan, |entry| {
                let (binding, caller_ledger, callee_ledger) = {
                    let caller = Snapshot::new(&original.caller, 4096, 1 << 20).unwrap();
                    let callee = Snapshot::new(&original.callee, 4096, 1 << 20).unwrap();
                    let caller_ledger = caller.operations.allocator().0.clone();
                    let callee_ledger = callee.operations.allocator().0.clone();
                    let independent = Plan::new(
                        &caller,
                        &callee,
                        original.descriptor.call,
                        work::Limits::from(&JitConfig::default()),
                    )
                    .unwrap();
                    let binding =
                        Binding::new(&independent, original.caller, original.callee).unwrap();
                    assert_eq!(binding.descriptor, original.descriptor);
                    (binding, caller_ledger, callee_ledger)
                };
                assert_eq!(caller_ledger.current(), 0);
                assert_eq!(callee_ledger.current(), 0);
                with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
                    position(ctx, host, &binding);
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let progress = binding
                        .run(ctx, host, 16, 64, 0, |state| unsafe { entry(state) })
                        .unwrap();
                    assert!(progress.calls > 1);
                });
                assert_eq!(caller_ledger.current(), 0);
                assert_eq!(callee_ledger.current(), 0);
            });
        },
    );
}
