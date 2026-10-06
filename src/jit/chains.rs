use crate::{opcode::CallTransition, thread::activation::ActivationHost, Context};

use super::PairScope;

#[derive(Debug)]
enum Pause {
    Unavailable,
    Unsupported { instructions: u32 },
}

struct Prefix {
    instructions: u32,
    transition: Option<CallTransition>,
}

fn prefix<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    budget: u32,
) -> Option<Prefix> {
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
        Some(Prefix {
            instructions,
            transition: (instructions < budget)
                .then(|| closure.prototype().opcodes[*registers.pc].call_transition())
                .flatten(),
        })
    })
}

#[derive(Default)]
struct Driver {
    pair: PairScope,
}

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
                            &mut self.pair,
                        )
                    })
                } else {
                    None
                };
                let paired = pair
                    .as_ref()
                    .and_then(|pair| pair.invoke(ctx, host, budget, prefix.instructions));
                self.pair.cache = pair;
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
    use crate::{Closure, Executor, Fuel, JitConfig, JitMode, Lua, Thread};

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
}
