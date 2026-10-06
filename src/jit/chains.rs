use crate::{opcode::CallTransition, thread::activation::ActivationHost, Context};

use super::PairScope;

#[derive(Debug)]
enum Pause {
    Unavailable,
    Unsupported { instructions: u32 },
}

struct Prefix<'gc> {
    instructions: u32,
    transition: Option<CallTransition>,
    resume: Option<crate::thread::NativeResume<'gc>>,
}

fn prefix<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    budget: u32,
) -> Option<Prefix<'gc>> {
    if budget == 0 || !host.lua_ready() || !ctx.jit().active() {
        return None;
    }
    host.clear_hook(ctx);
    if ctx.hook_enabled() {
        return None;
    }
    host.with_registers(|closure, mut registers| {
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, closure.prototype())?;
        let code = ctx.jit().lookup(id)?;
        let instructions = ctx.jit().run(&code, ctx, closure, &mut registers, budget);
        let transition = (instructions < budget)
            .then(|| closure.prototype().opcodes[*registers.pc].call_transition())
            .flatten();
        Some(Prefix {
            instructions,
            transition,
            resume: (instructions < budget && transition.is_none()).then(|| {
                crate::thread::NativeResume::new(
                    ctx,
                    closure,
                    id,
                    *registers.pc,
                    instructions,
                    code,
                )
            }),
        })
    })
}

#[derive(Default)]
struct Driver {}

struct Outcome {
    slices: usize,
    pairs: usize,
    pause: Option<Pause>,
    result: Result<(), crate::thread::VMError>,
}

impl Driver {
    fn run<'gc>(
        &mut self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
    ) -> Outcome {
        let mut scope = PairScope::default();
        let mut outcome = Outcome {
            slices: 0,
            pairs: 0,
            pause: None,
            result: Ok(()),
        };
        while outcome.slices < limit && host.lua_ready() {
            let Some(prefix) = prefix(ctx, host, budget) else {
                outcome.pause = Some(Pause::Unavailable);
                break;
            };
            if prefix.instructions >= budget {
                host.charge_native_slice(prefix.instructions);
                outcome.slices += 1;
            } else if let Some(transition) = prefix.transition {
                let pair = if matches!(transition, CallTransition::Call { .. })
                    && limit - outcome.slices >= 2
                    && budget >= 4
                    && ctx.jit().call_pairs_enabled()
                {
                    host.with_registers(|closure, registers| {
                        ctx.jit().prepare_call_at(
                            ctx,
                            closure,
                            &registers,
                            *registers.pc,
                            &mut scope,
                        )
                    })
                } else {
                    None
                };
                let paired = pair
                    .as_ref()
                    .and_then(|pair| pair.invoke(ctx, host, budget, prefix.instructions));
                scope.cache = pair;
                if let Some(paired) = paired {
                    outcome.slices += 1 + paired.returns;
                    outcome.pairs += paired.returns;
                    outcome.result = paired.result;
                } else {
                    outcome.result =
                        host.test_native_transition(ctx, transition, prefix.instructions);
                    outcome.slices += 1;
                }
            } else {
                outcome.pause = Some(Pause::Unsupported {
                    instructions: prefix.instructions,
                });
                break;
            }
            if outcome.result.is_err() || !host.fuel().should_continue() {
                break;
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Callback, CallbackReturn, Closure, Executor, Fuel, JitConfig, JitMode, Lua, Thread,
    };

    const SOURCE: &[u8] =
        b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";

    #[derive(Debug, PartialEq)]
    struct Trace {
        frames: Vec<String>,
        slots: Vec<(u64, u64)>,
        open: usize,
        fuel: i32,
        dispatches: u64,
        instructions: u64,
    }

    fn trace(ctx: Context<'_>, host: &ActivationHost<'_, '_>) -> Trace {
        let (frames, values, open, fuel) = host.test_trace();
        let stats = ctx.jit().0.borrow().stats;
        Trace {
            frames,
            slots: values
                .into_iter()
                .map(|value| {
                    let slot = super::super::abi::Slot::from_value(value);
                    (slot.tag, slot.bits)
                })
                .collect(),
            open,
            fuel,
            dispatches: stats.total_dispatches,
            instructions: stats.native_instructions + stats.interpreted_instructions,
        }
    }

    fn state(mode: JitMode, source: &[u8]) -> (Lua, crate::StashedClosure) {
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode,
            hot_threshold: 1,
            ..JitConfig::default()
        })
        .unwrap();
        let closure = lua.enter(|ctx| ctx.stash(Closure::load(ctx, None, source).unwrap()));
        if mode == JitMode::Auto {
            let executor =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
            let _ = lua.execute::<i64>(&executor).unwrap();
        }
        (lua, closure)
    }

    #[test]
    fn direct_native_caller_prefixes_preserve_canonical_bounded_slices() {
        for budget in [4, 8, 64] {
            for limit in [1, 2, 3, 8, 16] {
                let mut traces = Vec::new();
                for mode in [JitMode::Off, JitMode::Auto] {
                    let (mut lua, closure) = state(mode, SOURCE);
                    lua.enter(|ctx| {
                        let thread = Thread::new(ctx);
                        thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
                        let mut fuel = Fuel::with(4096);
                        crate::thread::activation::with_test_existing_thread(
                            ctx,
                            thread,
                            &mut fuel,
                            |host| {
                                for _ in 0..2 {
                                    host.run(ctx, 1, 64, 4).result.unwrap();
                                }
                                ctx.jit().0.borrow_mut().stats = Default::default();
                                if mode == JitMode::Auto {
                                    let mut driver = Driver::default();
                                    let outcome = driver.run(ctx, host, limit, budget);
                                    outcome.result.unwrap();
                                    assert!(
                                        outcome.pause.is_none(),
                                        "budget={budget} limit={limit} pause={:?}",
                                        outcome.pause
                                    );
                                    assert_eq!(outcome.slices, limit);
                                    assert!(ctx.jit().0.borrow().stats.native_instructions != 0);
                                    if limit >= 2 {
                                        assert!(outcome.pairs != 0);
                                    }
                                } else {
                                    host.run(ctx, limit, budget, 4).result.unwrap();
                                }
                                traces.push(trace(ctx, host));
                            },
                        );
                    });
                }
                assert_eq!(traces[0], traces[1], "budget={budget} limit={limit}");
            }
        }
    }

    #[test]
    fn unsupported_native_prefix_is_materialized_without_replaying_or_charging_it() {
        let source = b"local sum=0 local one=1 local scale=2 sum=sum+one local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";
        let (mut lua, closure) = state(JitMode::Auto, source);
        lua.enter(|ctx| {
            let thread = Thread::new(ctx);
            thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            crate::thread::activation::with_test_existing_thread(ctx, thread, &mut fuel, |host| {
                let fuel_before = host.fuel().remaining();
                let before = ctx.jit().0.borrow().stats.native_instructions;
                let outcome = Driver::default().run(ctx, host, 8, 64);
                outcome.result.unwrap();
                assert_eq!(outcome.slices, 0);
                let Some(Pause::Unsupported { instructions }) = outcome.pause else {
                    panic!("expected explicit partial exit: {:?}", outcome.pause)
                };
                assert!(instructions > 0);
                assert_eq!(
                    ctx.jit().0.borrow().stats.native_instructions - before,
                    u64::from(instructions)
                );
                assert_eq!(host.fuel().remaining(), fuel_before);
                host.with_registers(|closure, registers| {
                    assert!(matches!(
                        closure.prototype().opcodes[*registers.pc].decode(),
                        crate::opcode::Operation::Closure { .. }
                    ));
                    assert!(matches!(registers.stack_frame[0], crate::Value::Integer(1)));
                });
            });
        });
    }

    #[test]
    fn native_prefixes_preserve_exhausted_and_small_fuel_transitions() {
        for remaining in [-1, 0, 1, 8, 13, 64] {
            let mut traces = Vec::new();
            for mode in [JitMode::Off, JitMode::Auto] {
                let (mut lua, closure) = state(mode, SOURCE);
                lua.enter(|ctx| {
                    let thread = Thread::new(ctx);
                    thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
                    let mut fuel = Fuel::with(4096);
                    crate::thread::activation::with_test_existing_thread(
                        ctx,
                        thread,
                        &mut fuel,
                        |host| {
                            for _ in 0..2 {
                                host.run(ctx, 1, 64, 4).result.unwrap();
                            }
                            host.test_fuel(Fuel::with(remaining));
                            ctx.jit().0.borrow_mut().stats = Default::default();
                            if mode == JitMode::Auto {
                                let outcome = Driver::default().run(ctx, host, 8, 64);
                                outcome.result.unwrap();
                                assert!(outcome.pause.is_none());
                                assert!(outcome.slices > 0);
                            } else {
                                host.run(ctx, 8, 64, 4).result.unwrap();
                            }
                            traces.push(trace(ctx, host));
                        },
                    );
                });
            }
            assert_eq!(traces[0], traces[1], "remaining={remaining}");
        }
    }

    #[test]
    fn native_chains_release_leases_and_materialize_captures_before_collection() {
        let (mut lua, closure) = state(JitMode::Auto, SOURCE);
        let thread = lua.enter(|ctx| {
            let thread = Thread::new(ctx);
            thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            crate::thread::activation::with_test_existing_thread(ctx, thread, &mut fuel, |host| {
                for _ in 0..2 {
                    host.run(ctx, 1, 64, 4).result.unwrap();
                }
            });
            ctx.stash(thread)
        });
        let mut driver = Driver::default();
        for _ in 0..32 {
            lua.enter(|ctx| {
                let mut fuel = Fuel::with(4096);
                crate::thread::activation::with_test_existing_thread(
                    ctx,
                    ctx.fetch(&thread),
                    &mut fuel,
                    |host| {
                        let outcome = driver.run(ctx, host, 16, 64);
                        outcome.result.unwrap();
                        assert!(outcome.pause.is_none());
                        assert_eq!(outcome.slices, 16);
                        assert_eq!(outcome.pairs, 8);
                    },
                );
            });
            assert_eq!(std::mem::size_of_val(&driver), 0);
            lua.gc_collect();
        }
        let executor = lua.enter(|ctx| ctx.stash(Executor::run(&ctx, ctx.fetch(&thread)).unwrap()));
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 50005000);
    }

    #[test]
    fn hooks_and_off_decline_before_native_work_or_frame_changes() {
        for hooked in [false, true] {
            let (mut lua, closure) = state(JitMode::Auto, SOURCE);
            if !hooked {
                lua.set_jit_config(JitConfig::default()).unwrap();
            }
            lua.enter(|ctx| {
                let thread = Thread::new(ctx);
                thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
                if hooked {
                    let hook = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
                    ctx.set_debug_hook(crate::Value::Function(hook.into()), true, 1);
                }
                let mut fuel = Fuel::with(4096);
                crate::thread::activation::with_test_existing_thread(
                    ctx,
                    thread,
                    &mut fuel,
                    |host| {
                        let before = trace(ctx, host);
                        let mut driver = Driver::default();
                        let outcome = driver.run(ctx, host, 16, 64);
                        outcome.result.unwrap();
                        assert_eq!(outcome.slices, 0);
                        assert!(matches!(outcome.pause, Some(Pause::Unavailable)));
                        assert_eq!(trace(ctx, host), before);
                        assert_eq!(std::mem::size_of_val(&driver), 0);
                    },
                );
            });
        }
    }

    #[test]
    fn native_caller_stops_before_rust_callback_and_releases_host_borrows() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..JitConfig::default()
        })
        .unwrap();
        let counter = calls.clone();
        let closure = lua.enter(|ctx| {
            let increment = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                drop(ctx.jit().0.borrow_mut());
                counter.set(counter.get() + 1);
                let value: i64 = stack.consume(ctx)?;
                stack.replace(ctx, value.wrapping_add(1));
                Ok(CallbackReturn::Return)
            });
            ctx.set_global("host_increment", increment);
            ctx.stash(
                Closure::load(
                    ctx,
                    None,
                    b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum",
                )
                .unwrap(),
            )
        });
        let warm = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        assert_eq!(lua.execute::<i64>(&warm).unwrap(), 12507500);
        calls.set(0);
        let executor = lua.enter(|ctx| {
            let thread = Thread::new(ctx);
            thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            crate::thread::activation::with_test_existing_thread(ctx, thread, &mut fuel, |host| {
                let mut driver = Driver::default();
                let outcome = driver.run(ctx, host, 16, 64);
                outcome.result.unwrap();
                assert!(outcome.pause.is_none());
                assert_eq!(outcome.slices, 1);
                assert!(!host.lua_ready());
                assert_eq!(calls.get(), 0);
                assert_eq!(std::mem::size_of_val(&driver), 0);
            });
            ctx.stash(Executor::run(&ctx, thread).unwrap())
        });
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 12507500);
        assert_eq!(calls.get(), 5000);
    }

    #[test]
    fn caught_native_panic_releases_a_previously_cached_pair_lease() {
        let source = b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do add(i) local unused=trigger end return sum";
        let (mut lua, closure) = state(JitMode::Auto, source);
        lua.enter(|ctx| {
            let closure = ctx.fetch(&closure);
            let prototype = closure.prototype();
            let pc = prototype
                .opcodes
                .iter()
                .position(|opcode| {
                    matches!(opcode.call_transition(), Some(CallTransition::Call { .. }))
                })
                .unwrap();
            let key = super::super::pairs::Key {
                caller: ctx
                    .jit_registry()
                    .borrow()
                    .identity(ctx, prototype)
                    .unwrap(),
                callee: ctx
                    .jit_registry()
                    .borrow()
                    .identity(ctx, prototype.prototypes[0])
                    .unwrap(),
                pc,
            };
            let program = ctx.jit().pair_lease(key).unwrap();
            let owners = super::super::owner::Shared::strong_count(&program);
            let thread = Thread::new(ctx);
            thread.start(ctx, closure.into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            crate::thread::activation::with_test_existing_thread(ctx, thread, &mut fuel, |host| {
                for _ in 0..2 {
                    host.run(ctx, 1, 64, 4).result.unwrap();
                }
                let mut driver = Driver::default();
                let setup = driver.run(ctx, host, 1, 1);
                setup.result.unwrap();
                assert_eq!(setup.slices, 1);
                assert!(setup.pause.is_none());
                host.with_registers(|closure, registers| {
                    assert!(matches!(
                        closure.prototype().opcodes[*registers.pc].decode(),
                        crate::opcode::Operation::NumericForLoop { .. }
                    ));
                });
                let before = ctx.jit().0.borrow().stats.native_pair_calls;
                let globals = ctx.globals().into_inner();
                let lock = globals.borrow_mut(&ctx);
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    driver.run(ctx, host, 16, 64)
                }));
                assert!(caught.is_err());
                drop(lock);
                assert_eq!(ctx.jit().0.borrow().stats.native_pair_calls - before, 1);
                assert_eq!(super::super::owner::Shared::strong_count(&program), owners);
                drop(ctx.jit().0.borrow_mut());
                let reused = driver.run(ctx, host, 2, 64);
                reused.result.unwrap();
                assert!(reused.pause.is_none());
                assert_eq!(reused.slices, 2);
                assert_eq!(super::super::owner::Shared::strong_count(&program), owners);
            });
        });
    }

    #[test]
    fn resumed_unsupported_prefix_preserves_the_original_vm_slice() {
        let source = b"local sum=0 local one=1 local scale=2 sum=sum+one local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";
        for budget in [5, 8, 64] {
            let mut traces = Vec::new();
            let mut statistics = Vec::new();
            for resume in [false, true] {
                let (mut lua, closure) = state(JitMode::Auto, source);
                lua.enter(|ctx| {
                    let thread = Thread::new(ctx);
                    thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
                    let mut fuel = Fuel::with(4096);
                    crate::thread::activation::with_test_existing_thread(
                        ctx,
                        thread,
                        &mut fuel,
                        |host| {
                            ctx.jit().0.borrow_mut().stats = Default::default();
                            if resume {
                                let prefix = prefix(ctx, host, budget).unwrap();
                                assert!(prefix.instructions > 0 && prefix.instructions < budget);
                                assert!(prefix.transition.is_none());
                                let completed = host
                                    .test_resume_native(ctx, budget, prefix.resume.unwrap())
                                    .unwrap();
                                host.charge_native_slice(completed);
                            } else {
                                host.run(ctx, 1, budget, 4).result.unwrap();
                            }
                            traces.push(trace(ctx, host));
                            statistics.push(ctx.jit().0.borrow().stats);
                        },
                    );
                });
            }
            assert_eq!(traces[0], traces[1], "budget={budget}");
            assert_eq!(statistics[0], statistics[1], "budget={budget}");
        }
    }
}
