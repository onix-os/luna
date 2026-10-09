use super::*;

mod cache;
mod cost;
mod registration;
mod request;
mod service;
use crate::{
    opcode::Operation, thread::activation::with_test_thread, Fuel, JitConfig, JitMode, Lua, Value,
};

fn fixture(test: impl for<'gc> FnOnce(Context<'gc>, Closure<'gc>, Region, usize)) {
    fixture_source(
        b"local n=0 local function f(v) n=n+v end for i=1,20 do f(i) end return n",
        test,
    );
}

pub(super) fn fixture_source(
    source: &[u8],
    test: impl for<'gc> FnOnce(Context<'gc>, Closure<'gc>, Region, usize),
) {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, source).unwrap();
        let (region, start) = build_region(ctx, closure);
        test(ctx, closure, region, start);
    });
}

pub(super) fn build_region<'gc>(ctx: Context<'gc>, closure: Closure<'gc>) -> (Region, usize) {
    let (pair, start) = build_dependencies(ctx, closure);
    (Region::new(ctx, closure, pair).unwrap(), start)
}

fn build_dependencies<'gc>(ctx: Context<'gc>, closure: Closure<'gc>) -> (PreparedPair, usize) {
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
    if ctx.jit().0.borrow().pairs.is_none() {
        ctx.jit().test_call_pairs(true);
    }
    ctx.jit().observe_pair(key);
    ctx.jit()
        .compile_pair(
            key,
            caller(),
            super::super::ir::Snapshot::new_in(&prototype.prototypes[0], 4096, allocator).unwrap(),
        )
        .unwrap();
    let pair = PreparedPair {
        program: ctx.jit().pair_lease(key).unwrap(),
    };
    (pair, start)
}

#[test]
fn rooted_caller_moves_keep_closures_after_source_overwrite_without_helpers() {
    let source = b"local n=0 local function f(v) n=n+v end local g=f local h=g f=nil for i=1,20 do h(i) end return n";
    fixture_source(source, |ctx, closure, region, start| {
        assert!(region.caller.rooted_moves());
        let (native, slices) = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
            let before = stats(ctx);
            let helpers = ctx.jit().0.borrow().stats.helper_calls;
            let outcome = region.run(ctx, host, 64, 64).unwrap();
            outcome.result.unwrap();
            assert_eq!(outcome.pairs, 20);
            assert_eq!(ctx.jit().0.borrow().stats.helper_calls, helpers);
            let state = trace(ctx, host, before);
            assert_eq!(state.1, vec![(abi::INTEGER, 210)]);
            (state, outcome.slices)
        });
        let canonical = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            let before = stats(ctx);
            host.run(ctx, slices, 64, 4).result.unwrap();
            trace(ctx, host, before)
        });
        assert_eq!(native, canonical);
    });
}

#[test]
fn admitted_pair_matches_canonical_with_shifted_capture_arguments() {
    for source in [
        &b"local n=7 local function f(v) n=n+v end f(3) return n"[..],
        &b"local n=7 local function f(v,...) n=n+v end f(3,4,5) return n"[..],
        &b"local n=7 local function f() n=n+2 end f() return n"[..],
    ] {
        fixture_source(source, |ctx, closure, region, _| {
            let pc = region.pair.program.key().pc;
            let Operation::Call { func, args, .. } = closure.prototype().opcodes[pc].decode()
            else {
                panic!()
            };
            for argument_capture in [false, true] {
                if argument_capture && args.to_constant() == Some(0) {
                    continue;
                }
                let run = |admission| {
                    with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                        host.run(ctx, 1, pc as u32, 4).result.unwrap();
                        if argument_capture {
                            host.with_registers(|_, mut registers| {
                                let Value::Function(crate::Function::Closure(callee)) =
                                    registers.stack_frame[usize::from(func.0)]
                                else {
                                    panic!()
                                };
                                let upvalue = registers.open_test_upvalue(
                                    &ctx,
                                    crate::types::RegisterIndex(func.0 + 1),
                                );
                                callee.set_upvalue(&ctx, 0, upvalue);
                            });
                        }
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                        let before = stats(ctx);
                        let outcome = if admission {
                            let admitted = super::super::canonical::admission::Admitted::new(
                                &region.pair.program,
                                ctx,
                                closure,
                                host.frame_identity(),
                            )
                            .unwrap();
                            admitted.invoke(host, 64, 0).unwrap()
                        } else {
                            region.pair.program.invoke_result(ctx, host, 64, 0).unwrap()
                        };
                        outcome.result.unwrap();
                        ((outcome.calls, outcome.returns), trace(ctx, host, before))
                    })
                };
                assert_eq!(run(true), run(false));
            }
        });
    }
}

#[test]
fn admitted_pair_tracks_rebound_capture_across_repeated_calls() {
    let source =
        b"local n=7 local m=11 local keep={} local function f(v) n=n+v end f(3) return n,m,keep";
    fixture_source(source, |ctx, closure, region, _| {
        let pc = region.pair.program.key().pc;
        let Operation::Call { func, .. } = closure.prototype().opcodes[pc].decode() else {
            panic!()
        };
        let run = |native| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, pc as u32, 4).result.unwrap();
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let admitted = super::super::canonical::admission::Admitted::new(
                    &region.pair.program,
                    ctx,
                    closure,
                    host.frame_identity(),
                )
                .unwrap();
                let callee = host.with_registers(|_, registers| {
                    let Value::Function(crate::Function::Closure(callee)) =
                        registers.stack_frame[usize::from(func.0)]
                    else {
                        panic!()
                    };
                    callee
                });
                let before = stats(ctx);
                let native_before = ctx.jit().0.borrow().stats.native_instructions;
                for capture in [0, 1, 0] {
                    host.with_registers(|_, mut registers| {
                        *registers.pc = pc;
                        registers.stack_frame[usize::from(func.0)] = callee.into();
                        registers.stack_frame[usize::from(func.0) + 1] = Value::Integer(3);
                        let upvalue =
                            registers.open_test_upvalue(&ctx, crate::types::RegisterIndex(capture));
                        callee.set_upvalue(&ctx, 0, upvalue);
                    });
                    if native {
                        let outcome = admitted.invoke(host, 64, 0).unwrap();
                        outcome.result.unwrap();
                        assert_eq!((outcome.calls, outcome.returns), (1, 1));
                    } else {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                        host.run(ctx, 2, 64, 4).result.unwrap();
                    }
                }
                if native {
                    assert_eq!(
                        ctx.jit().0.borrow().stats.native_instructions - native_before,
                        9
                    );
                }
                let state = trace(ctx, host, before);
                assert_eq!(&state.1[..2], &[(abi::INTEGER, 13), (abi::INTEGER, 14)]);
                state
            })
        };
        assert_eq!(run(true), run(false));
    });
}

#[test]
fn returned_pair_preserves_wide_scalar_and_reference_prefixes() {
    let locals = (0..24)
        .map(|index| format!("local k{index}={} ", 100 + index))
        .collect::<String>();
    let source = format!("local n=0 local keep={{value=7}} {locals} local function f(v) n=n+v end for i=1,20 do f(i) k0=k0+1 end return n,k0,k23,keep.value");
    fixture_source(source.as_bytes(), |ctx, closure, region, start| {
        assert!(region.caller.registers() > 24);
        let (native, slices) = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
            let before = stats(ctx);
            let outcome = region.run(ctx, host, 64, 64).unwrap();
            outcome.result.unwrap();
            assert_eq!(outcome.pairs, 20);
            let state = trace(ctx, host, before);
            assert_eq!(
                state.1,
                [210, 120, 123, 7]
                    .map(|value| (abi::INTEGER, value))
                    .to_vec()
            );
            (state, outcome.slices)
        });
        let canonical = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            let before = stats(ctx);
            host.run(ctx, slices, 64, 4).result.unwrap();
            trace(ctx, host, before)
        });
        assert_eq!(native, canonical);
    });
}

#[test]
fn returned_pair_preserves_open_upper_frame_captures() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let root = Closure::load(ctx, None, &b"local n=0 local function caller() local p=99 local function f(v) n=n+v end for i=1,10 do f(i) p=p+1 end return end caller() return n"[..]).unwrap();
        let mut slices = 0;
        let mut run = |native: bool| with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, 64, 4).result.unwrap();
            let caller = host.with_registers(|closure, registers| { assert_eq!(*registers.pc, 0); closure });
            assert_ne!(caller, root);
            let start = caller.prototype().opcodes.iter().position(|opcode| matches!(opcode.decode(), Operation::Closure { .. })).unwrap() + 1;
            let region = native.then(|| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let (region, found) = build_region(ctx, caller);
                assert_eq!(found, start);
                region
            });
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            let before = stats(ctx);
            if let Some(region) = region {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let outcome = region.run(ctx, host, 64, 64).unwrap();
                outcome.result.unwrap();
                assert_eq!(outcome.pairs, 10);
                slices = outcome.slices;
            } else {
                host.run(ctx, slices, 64, 4).result.unwrap();
            }
            host.with_registers(|closure, registers| {
                assert_eq!(closure, root);
                assert!(matches!(registers.stack_frame[0], Value::Integer(55)));
            });
            trace(ctx, host, before)
        });
        let native = run(true);
        assert_eq!(native, run(false));
    });
}

#[test]
fn region_handles_argument_tail_aliases_and_partial_calls() {
    for (source, returns) in [
        (
            &b"local n=7 local function f(v,w) n=n+v end f(2,3) local p=99 return n,p"[..],
            1,
        ),
        (
            &b"local n=7 local function f(v) n=n+v end f(2) local p=99 return n,p"[..],
            0,
        ),
    ] {
        fixture_source(source, |ctx, closure, region, _| {
            let pc = region.pair.program.key().pc;
            let Operation::Call { func, .. } = closure.prototype().opcodes[pc].decode() else {
                panic!()
            };
            let mut slices = 0;
            let mut run = |native| {
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, (pc - 1) as u32, 4).result.unwrap();
                    let capture = host.with_registers(|_, mut registers| {
                        assert_eq!(*registers.pc, pc - 1);
                        let Value::Function(crate::Function::Closure(callee)) =
                            registers.stack_frame[usize::from(func.0)]
                        else {
                            panic!()
                        };
                        let capture = registers
                            .open_test_upvalue(&ctx, crate::types::RegisterIndex(func.0 + 1));
                        callee.set_upvalue(&ctx, 0, capture);
                        capture
                    });
                    let before = stats(ctx);
                    if native {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                        let outcome = region.run(ctx, host, 64, 64).unwrap();
                        outcome.result.unwrap();
                        assert_eq!(outcome.pairs, returns);
                        assert_eq!(outcome.fragments, 1 + returns);
                        slices = outcome.slices;
                    } else {
                        host.run(ctx, slices, 64, 4).result.unwrap();
                    }
                    if returns == 1 {
                        assert!(matches!(
                            capture.get(),
                            crate::closure::UpValueState::Closed(Value::Integer(5))
                        ));
                    } else {
                        host.with_registers(|_, registers| {
                            assert_eq!(*registers.pc, 0);
                            assert!(matches!(registers.get_upvalue(&ctx, capture), Value::Nil));
                        });
                    }
                    trace(ctx, host, before)
                })
            };
            let native = run(true);
            assert_eq!(native, run(false));
        });
    }
}

#[test]
fn admitted_pair_rechecks_dynamic_values_frames_fuel_mode_and_hooks() {
    fixture(|ctx, closure, region, _| {
        let pc = region.pair.program.key().pc;
        let Operation::Call { func, .. } = closure.prototype().opcodes[pc].decode() else {
            panic!()
        };
        let foreign = Closure::load(ctx, None, &b"return 9"[..]).unwrap();
        for case in 0..14 {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                for _ in 0..32 {
                    if host.with_registers(|_, registers| *registers.pc == pc) {
                        break;
                    }
                    host.run(ctx, 1, 1, 4).result.unwrap();
                }
                assert!(host.with_registers(|_, registers| *registers.pc == pc));
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let admitted = super::super::canonical::admission::Admitted::new(
                    &region.pair.program,
                    ctx,
                    closure,
                    host.frame_identity(),
                )
                .unwrap();
                let mut budget = 64;
                let mut prefix = 0;
                match case {
                    0 => budget = 3,
                    1 => host.test_fuel(Fuel::with(0)),
                    2 => host.test_variable_stack(),
                    3 => host.test_replace_closure(foreign),
                    4 => host.with_registers(|_, registers| {
                        registers.stack_frame[usize::from(func.0)] = Value::Integer(3)
                    }),
                    5 => host.with_registers(|_, registers| {
                        registers.stack_frame[usize::from(func.0) + 1] = Value::Number(3.0)
                    }),
                    6 => host.with_registers(|_, registers| {
                        registers.stack_frame[0] = Value::Number(3.0)
                    }),
                    7 => host.with_registers(|_, registers| {
                        let Value::Function(crate::Function::Closure(callee)) =
                            registers.stack_frame[usize::from(func.0)]
                        else {
                            panic!()
                        };
                        callee.set_upvalue(
                            &ctx,
                            0,
                            crate::closure::UpValue::new(
                                &ctx,
                                crate::closure::UpValueState::Closed(Value::Integer(3)),
                            ),
                        );
                    }),
                    8 => ctx.jit().0.borrow_mut().config.mode = JitMode::Off,
                    9 => {
                        let callback = crate::Callback::from_fn(&ctx, |_, _, _| {
                            panic!("admitted hook executed")
                        });
                        ctx.set_debug_hook(callback.into(), false, 1);
                        ctx.suppress_hook_at(1);
                    }
                    10 => host.with_registers(|_, registers| *registers.pc = pc + 1),
                    11 => {
                        let mut fuel = Fuel::with(10000);
                        fuel.interrupt();
                        host.test_fuel(fuel);
                    }
                    12 => {
                        prefix = 64;
                        host.test_fuel(Fuel::with(64));
                    }
                    13 => host.test_fuel(Fuel::with(i32::MIN)),
                    _ => unreachable!(),
                }
                let before_stats = stats(ctx);
                let before = trace(ctx, host, before_stats);
                let counters = ctx.jit().0.borrow().stats;
                assert!(
                    admitted.invoke(host, budget, prefix).is_none(),
                    "case={case}"
                );
                assert_eq!(trace(ctx, host, before_stats), before, "case={case}");
                assert_eq!(ctx.jit().0.borrow().stats, counters, "case={case}");
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                ctx.set_debug_hook(Value::Nil, false, 0);
            });
        }
        with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            let admitted = super::super::canonical::admission::Admitted::new(
                &region.pair.program,
                ctx,
                closure,
                host.frame_identity(),
            )
            .unwrap();
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |foreign| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                for _ in 0..32 {
                    if foreign.with_registers(|_, registers| *registers.pc == pc) {
                        break;
                    }
                    foreign.run(ctx, 1, 1, 4).result.unwrap();
                }
                assert!(foreign.with_registers(|_, registers| *registers.pc == pc));
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let before_stats = stats(ctx);
                let before = trace(ctx, foreign, before_stats);
                assert!(admitted.invoke(foreign, 64, 0).is_none());
                assert_eq!(trace(ctx, foreign, before_stats), before);
            });
        });
    });
}

#[test]
fn admitted_pair_reuses_source_binding_through_physical_call_and_return() {
    let source = b"local n=0 local function f(v) n=n+v end f(3) return n";
    fixture_source(source, |ctx, closure, region, _| {
        let pc = region.pair.program.key().pc;
        let run = |native| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, pc as u32, 4).result.unwrap();
                assert!(host.with_registers(|_, registers| *registers.pc == pc));
                let before = stats(ctx);
                if native {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let admitted = super::super::canonical::admission::Admitted::new(
                        &region.pair.program,
                        ctx,
                        closure,
                        host.frame_identity(),
                    )
                    .unwrap();
                    let native_before = ctx.jit().0.borrow().stats.native_instructions;
                    let registry = ctx.jit_registry().borrow_mut(&ctx);
                    let outcome = admitted.invoke(host, 64, 0).unwrap();
                    drop(registry);
                    assert_eq!((outcome.calls, outcome.returns), (1, 1));
                    outcome.result.unwrap();
                    assert_eq!(
                        ctx.jit().0.borrow().stats.native_instructions - native_before,
                        3
                    );
                } else {
                    host.run(ctx, 2, 64, 4).result.unwrap();
                }
                trace(ctx, host, before)
            })
        };
        assert_eq!(run(true), run(false));
    });
}

#[test]
fn admitted_pair_accepts_rebound_closure_but_not_identical_foreign_prototype() {
    let source = b"local n=0 local function f(v) n=n+v end f(3) return n";
    fixture_source(source, |ctx, closure, region, start| {
        let pc = region.pair.program.key().pc;
        let Operation::Call { func, .. } = closure.prototype().opcodes[pc].decode() else {
            panic!()
        };
        for same in [false, true] {
            let run = |native| {
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, pc as u32, 4).result.unwrap();
                    assert!(pc > start);
                    assert!(host.with_registers(|_, registers| *registers.pc == pc));
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let admitted = super::super::canonical::admission::Admitted::new(
                        &region.pair.program,
                        ctx,
                        closure,
                        host.frame_identity(),
                    )
                    .unwrap();
                    host.with_registers(|_, registers| {
                        let Value::Function(crate::Function::Closure(original)) =
                            registers.stack_frame[usize::from(func.0)]
                        else {
                            panic!()
                        };
                        let prototype = if same {
                            original.prototype()
                        } else {
                            Closure::load(ctx, None, &source[..])
                                .unwrap()
                                .prototype()
                                .prototypes[0]
                        };
                        let mut upvalues = allocator_api2::vec::Vec::new_in(
                            ottavino_gc_arena::allocator_api::MetricsAlloc::new(&ctx),
                        );
                        upvalues.extend(
                            original
                                .upvalues()
                                .iter()
                                .map(|value| ottavino_gc_arena::lock::Lock::new(value.get())),
                        );
                        let replacement = Closure::from_parts(&ctx, prototype, upvalues);
                        assert_ne!(replacement, original);
                        registers.stack_frame[usize::from(func.0)] = replacement.into();
                    });
                    let before_stats = stats(ctx);
                    let before = trace(ctx, host, before_stats);
                    if native {
                        let outcome = admitted.invoke(host, 64, 0);
                        if same {
                            let outcome = outcome.unwrap();
                            assert_eq!((outcome.calls, outcome.returns), (1, 1));
                            outcome.result.unwrap();
                        } else {
                            assert!(outcome.is_none());
                            assert_eq!(trace(ctx, host, before_stats), before);
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                            host.run(ctx, 2, 64, 4).result.unwrap();
                        }
                    } else {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                        host.run(ctx, 2, 64, 4).result.unwrap();
                    }
                    trace(ctx, host, before_stats)
                })
            };
            assert_eq!(run(true), run(false));
        }
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
fn cached_region_rebinds_live_callers_and_does_not_root_collected_sources() {
    let mut lua = Lua::empty();
    lua.set_gc_pacing(false);
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..JitConfig::default()
    })
    .unwrap();
    let (region, start, first, second, memory) = lua.enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local n=seed local function f(v) n=n+v end for i=1,20 do f(i) end return n",
        )
        .unwrap();
        assert_eq!(closure.upvalues().len(), 1);
        let rebind = |seed| {
            let environment = crate::Table::new(&ctx);
            environment.set(ctx, "seed", seed).unwrap();
            let mut upvalues = allocator_api2::vec::Vec::new_in(
                ottavino_gc_arena::allocator_api::MetricsAlloc::new(&ctx),
            );
            upvalues.push(ottavino_gc_arena::lock::Lock::new(
                crate::closure::UpValue::new(
                    &ctx,
                    crate::closure::UpValueState::Closed(environment.into()),
                ),
            ));
            Closure::from_parts(&ctx, closure.prototype(), upvalues)
        };
        let first = rebind(3);
        let second = rebind(9);
        let (region, start) = build_region(ctx, closure);
        (
            region,
            start,
            ctx.stash(first),
            ctx.stash(second),
            ctx.jit().0.borrow().memory.clone(),
        )
    });
    fn gc_free<T: 'static>(_: &T) {}
    gc_free(&region);
    let requested = memory.requested();
    assert!(requested > 0);
    for (live, expected) in [(&first, 213), (&second, 219), (&first, 213)] {
        lua.gc_collect();
        lua.gc_collect();
        lua.enter(|ctx| {
            let closure = ctx.fetch(live);
            let mut slices = 0;
            let mut run = |native| {
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, start as u32, 4).result.unwrap();
                    let before = stats(ctx);
                    if native {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                        let outcome = region.run(ctx, host, 64, 64).unwrap();
                        outcome.result.unwrap();
                        assert_eq!(outcome.pairs, 20);
                        slices = outcome.slices;
                    } else {
                        host.run(ctx, slices, 64, 4).result.unwrap();
                    }
                    let state = trace(ctx, host, before);
                    assert_eq!(state.1, vec![(abi::INTEGER, expected)]);
                    state
                })
            };
            let native = run(true);
            assert_eq!(native, run(false));
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
        });
        assert_eq!(memory.requested(), requested);
    }
    drop(first);
    drop(second);
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        assert!(ctx
            .jit_registry()
            .borrow()
            .resolve(ctx, region.source)
            .is_none());
        assert!(ctx.jit().lookup(region.source).is_none());
        assert!(ctx.jit().pair_lease(region.pair.program.key()).is_none());
        assert_eq!(ctx.jit().0.borrow().stats.registered_prototypes, 0);
    });
    assert_eq!(memory.requested(), requested);
    drop(lua);
    assert_eq!(memory.requested(), requested);
    drop(region);
    assert_eq!(memory.requested(), 0);
    assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn retired_region_owners_keep_mappings_live_until_the_region_is_dropped() {
    fixture(|ctx, closure, region, start| {
        let memory = ctx.jit().0.borrow().memory.clone();
        let before = memory.requested();
        assert!(before > 0);
        {
            let mut manager = ctx.jit().0.borrow_mut();
            manager.code.remove(&region.source).unwrap();
            manager
                .pairs
                .as_mut()
                .unwrap()
                .evict(region.pair.program.key());
        }
        assert_eq!(memory.requested(), before);
        with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
            let before_stats = stats(ctx);
            let before = trace(ctx, host, before_stats);
            assert!(region.run(ctx, host, 64, 64).is_none());
            assert_eq!(trace(ctx, host, before_stats), before);
        });
        drop(region);
        assert_eq!(memory.requested(), 0);
        assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
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
        (
            &b"local n=0 local function f(v) n=n+v end f(3) local a=7 local b=3 local q=a%b cb() return n,q"[..],
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
                    assert!(ctx.jit_registry().try_borrow_mut(&ctx).is_ok());
                    assert!(Closure::load(ctx, None, b"return 42").is_ok());
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

pub(super) fn trace(
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
fn interpreted_prefixes_preserve_paired_coverage_and_bounded_accounting() {
    for source in [
        &b"local n=0 local function f(v) n=n+v end for i=1,20 do local q=i%3 f(i) end return n"[..],
        &b"local n=0 local function f(v) n=n+v end local a='a' local b='b' for i=1,20 do local q=a<b f(i) end return n"[..],
    ] {
        fixture_source(source, |ctx, closure, region, start| {
            for limit in [1, 2, 3, 4, 8, 64] {
                for budget in [1, 2, 3, 4, 8, 64] {
                    for fuel in [-1, 0, 1, 8, 20, 10000] {
                        for select_pairs in [false, true] {
                            let (native, slices, error) = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                                host.run(ctx, 1, start as u32, 4).result.unwrap();
                                host.test_fuel(Fuel::with(fuel));
                                host.test_pair_selection(select_pairs);
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                                let before = stats(ctx);
                                let returns = ctx.jit().0.borrow().stats.native_pair_returns;
                                let outcome = region.run(ctx, host, limit, budget).unwrap();
                                assert!(outcome.slices > 0 && outcome.slices <= limit);
                                assert!(outcome.fragments > 0 && outcome.fragments <= outcome.slices);
                                assert!(ctx.jit().0.borrow().resume_scope.is_none());
                                assert_eq!(ctx.jit().0.borrow().stats.native_pair_returns - returns, outcome.pairs as u64);
                                if (limit, budget, fuel) == (64, 64, 10000) && select_pairs {
                                    assert_eq!(outcome.pairs, 20);
                                    assert!(matches!(host.test_trace().1.as_slice(), [Value::Integer(210)]));
                                }
                                if limit == 1 || budget < 4 || !select_pairs {
                                    assert_eq!(outcome.pairs, 0);
                                }
                                (trace(ctx, host, before), outcome.slices, outcome.result.err().map(|e| e.to_string()))
                            });
                            let canonical = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                                host.run(ctx, 1, start as u32, 4).result.unwrap();
                                host.test_fuel(Fuel::with(fuel));
                                let before = stats(ctx);
                                let result = host.run(ctx, slices, budget, 4).result;
                                (trace(ctx, host, before), result.err().map(|e| e.to_string()))
                            });
                            assert_eq!((native, error), canonical, "limit={limit} budget={budget} fuel={fuel} pairs={select_pairs}");
                        }
                    }
                }
            }
        });
    }
}

#[test]
fn interpreted_prefix_handoff_uses_the_selected_call_site() {
    for source in [
        &b"local n=0 local function f(v) n=n+v end for i=1,10 do local a=i%3 f(i) local divisor=2 local b=i%divisor f(i) end return n"[..],
        &b"local n=0 local function f(v) n=n+v end for i=1,10 do f(i) f(i) end return n"[..],
    ] {
    fixture_source(
        source,
        |ctx, closure, region, start| {
            let prototype = closure.prototype();
            let key = region.pair.program.key();
            let allocator = ctx.jit().0.borrow().snapshots.clone();
            for (pc, opcode) in prototype.opcodes.iter().enumerate() {
                if pc != key.pc && matches!(opcode.decode(), Operation::Call { .. }) {
                    let key = super::super::pairs::Key { pc, ..key };
                    ctx.jit().observe_pair(key);
                    ctx.jit().compile_pair(
                        key,
                        super::super::ir::Snapshot::new_in(&prototype, 4096, allocator.clone()).unwrap(),
                        super::super::ir::Snapshot::new_in(&prototype.prototypes[0], 4096, allocator.clone()).unwrap(),
                    ).unwrap();
                }
            }
            let (native, slices) = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let before = stats(ctx);
                let outcome = region.run(ctx, host, 64, 64).unwrap();
                outcome.result.unwrap();
                assert_eq!(outcome.pairs, 20);
                assert!(matches!(host.test_trace().1.as_slice(), [Value::Integer(110)]));
                (trace(ctx, host, before), outcome.slices)
            });
            let canonical = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                let before = stats(ctx);
                host.run(ctx, slices, 64, 4).result.unwrap();
                trace(ctx, host, before)
            });
            assert_eq!(native, canonical);
        },
    );
    }
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
