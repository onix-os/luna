use crate::{opcode::CallTransition, thread::activation::ActivationHost, Context};

use super::PairScope;

type CallerShadow<'gc, const N: usize> = super::caller_shadow::Shadow<'gc, N>;

fn flush_shadow<'gc, const N: usize>(
    host: &mut ActivationHost<'gc, '_>,
    shadow: &mut Option<CallerShadow<'gc, N>>,
) {
    if let Some(shadow) = shadow.as_ref() {
        assert!(host.with_registers(|closure, mut registers| shadow.flush(closure, &mut registers)));
    }
    *shadow = None;
}

fn shadow_prefix<'gc, const N: usize>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    budget: u32,
    shadow: &mut Option<CallerShadow<'gc, N>>,
) -> Option<Prefix<'gc>> {
    if budget == 0 || !host.lua_ready() || !ctx.jit().active() {
        return None;
    }
    let frame = host.frame_identity();
    let id = host.with_registers(|closure, _| {
        ctx.jit_registry()
            .borrow()
            .identity(ctx, closure.prototype())
    })?;
    if !ctx.jit().0.borrow().code.contains_key(&id) {
        return None;
    }
    let code = ctx.jit().lookup(id)?;
    if shadow.is_none() {
        *shadow =
            host.with_registers(|closure, registers| CallerShadow::capture(closure, &registers));
    }
    let instructions = shadow.as_mut()?.invoke(ctx, &code, host, budget)?;
    host.with_registers(|closure, registers| {
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
                    frame,
                    *registers.pc,
                    instructions,
                    code,
                )
            }),
        })
    })
}

fn shadow_pair<'gc, const N: usize>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    pair: &super::PreparedPair,
    shadow: &mut Option<CallerShadow<'gc, N>>,
    budget: u32,
    prefix: u32,
    #[cfg(test)] recoveries: &mut usize,
) -> Option<super::PairOutcome> {
    let frame = host.frame_identity();
    let caller = host.caller_frame()?;
    let transfer = pair.program.prepare_shadow(ctx, host, shadow.as_mut()?)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pair.invoke(ctx, host, budget, prefix)
    }));
    if host.lua_pending() && host.frame_identity() == frame {
        assert!(host.with_registers(|closure, registers| transfer.resume(closure, &registers)));
    } else {
        assert!(transfer.recover(host, &caller));
        #[cfg(test)]
        {
            *recoveries += 1;
        }
        *shadow = None;
    }
    match result {
        Ok(result) => result,
        Err(payload) => {
            flush_shadow(host, shadow);
            std::panic::resume_unwind(payload);
        }
    }
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
    let frame = host.frame_identity();
    host.with_registers(|closure, mut registers| {
        let id = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, closure.prototype())?;
        if !ctx.jit().0.borrow().code.contains_key(&id) {
            return None;
        }
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
                    frame,
                    *registers.pc,
                    instructions,
                    code,
                )
            }),
        })
    })
}

#[derive(Default)]
pub(crate) struct Driver {}

pub(crate) struct Outcome {
    pub slices: usize,
    #[cfg(test)]
    shadow_captures: usize,
    #[cfg(test)]
    shadow_recoveries: usize,
    #[cfg(test)]
    shadow_capacity: usize,
    #[cfg(test)]
    pairs: usize,
    #[cfg(test)]
    pub stack_growths: usize,
    pub result: Result<(), crate::thread::VMError>,
}

impl Driver {
    #[cfg(test)]
    pub(crate) fn run<'gc>(
        &mut self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
    ) -> Outcome {
        self.run_mode::<false>(ctx, host, limit, budget)
    }

    pub(crate) fn run_shadow<'gc>(
        &mut self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
    ) -> Outcome {
        self.run_mode::<true>(ctx, host, limit, budget)
    }

    fn run_mode<'gc, const DEFER: bool>(
        &mut self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
    ) -> Outcome {
        if !DEFER {
            return self.run_sized::<false, 0>(ctx, host, limit, budget);
        }
        let width = if host.lua_ready() {
            host.with_registers(|_, registers| registers.stack_frame.len())
        } else {
            0
        };
        match width {
            0..=8 => self.run_sized::<true, 8>(ctx, host, limit, budget),
            9..=16 => self.run_sized::<true, 16>(ctx, host, limit, budget),
            17..=32 => self.run_sized::<true, 32>(ctx, host, limit, budget),
            33..=64 => self.run_sized::<true, 64>(ctx, host, limit, budget),
            65..=128 => self.run_sized::<true, 128>(ctx, host, limit, budget),
            _ => self.run_sized::<true, 256>(ctx, host, limit, budget),
        }
    }

    #[inline(never)]
    fn run_sized<'gc, const DEFER: bool, const N: usize>(
        &mut self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
    ) -> Outcome {
        assert!(limit > 0);
        let mut scope = PairScope::default();
        let mut shadow: Option<CallerShadow<'gc, N>> = None;
        let mut outcome = Outcome {
            slices: 0,
            #[cfg(test)]
            shadow_captures: 0,
            #[cfg(test)]
            shadow_recoveries: 0,
            #[cfg(test)]
            shadow_capacity: N,
            #[cfg(test)]
            pairs: 0,
            #[cfg(test)]
            stack_growths: 0,
            result: Ok(()),
        };
        while outcome.slices < limit && host.lua_pending() {
            #[cfg(test)]
            let capacity = host.stack_capacity();
            #[cfg(test)]
            let capture = DEFER && shadow.is_none();
            let prefix = if DEFER {
                shadow_prefix(ctx, host, budget, &mut shadow)
            } else {
                prefix(ctx, host, budget)
            };
            #[cfg(test)]
            {
                outcome.shadow_captures += usize::from(capture && shadow.is_some());
            }
            let Some(prefix) = prefix else {
                flush_shadow(host, &mut shadow);
                let fallback = host.run_canonical(ctx, limit - outcome.slices, budget, 4);
                #[cfg(test)]
                {
                    outcome.slices += fallback.activations;
                    outcome.stack_growths += fallback.stack_growths;
                }
                outcome.result = fallback.result;
                break;
            };
            if prefix.instructions >= budget {
                host.charge_native_slice(prefix.instructions);
                outcome.slices += 1;
            } else if let Some(transition) = prefix.transition {
                let pair = if matches!(transition, CallTransition::Call { .. })
                    && limit - outcome.slices >= 2
                    && budget >= 4
                    && host.pairing_enabled(ctx)
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
                let paired = pair.as_ref().and_then(|pair| {
                    if DEFER {
                        shadow_pair(
                            ctx,
                            host,
                            pair,
                            &mut shadow,
                            budget,
                            prefix.instructions,
                            #[cfg(test)]
                            &mut outcome.shadow_recoveries,
                        )
                    } else {
                        pair.invoke(ctx, host, budget, prefix.instructions)
                    }
                });
                scope.cache = pair;
                if let Some(paired) = paired {
                    outcome.slices += 1 + paired.returns;
                    #[cfg(test)]
                    {
                        outcome.pairs += paired.returns;
                    }
                    outcome.result = paired.result;
                } else {
                    flush_shadow(host, &mut shadow);
                    outcome.result = host.native_transition(ctx, transition, prefix.instructions);
                    outcome.slices += 1;
                }
            } else {
                flush_shadow(host, &mut shadow);
                let select_pairs =
                    limit - outcome.slices >= 2 && budget >= 4 && host.pairing_enabled(ctx);
                let result = host.resume_native_paired(
                    ctx,
                    budget,
                    prefix.resume.unwrap(),
                    select_pairs.then_some(&mut scope),
                );
                outcome.slices += 1;
                let (result, charged) = if let Some(pair) = scope.handoff.take() {
                    let completed = result.expect("pair handoff with VM error");
                    assert!(completed < budget);
                    let paired = pair.invoke(ctx, host, budget, completed);
                    scope.cache = Some(pair);
                    if let Some(paired) = paired {
                        outcome.slices += paired.returns;
                        #[cfg(test)]
                        {
                            outcome.pairs += paired.returns;
                        }
                        (paired.result.map(|()| 0), true)
                    } else {
                        (
                            host.canonical_slice(ctx, budget - completed)
                                .map(|instructions| completed + instructions),
                            false,
                        )
                    }
                } else {
                    (result, false)
                };
                if let Ok(completed) = result {
                    host.charge_instructions(completed);
                }
                if !charged {
                    host.charge_native_slice(0);
                }
                outcome.result = result.map(|_| ());
            }
            #[cfg(test)]
            {
                outcome.stack_growths += usize::from(host.stack_capacity() > capacity);
            }
            if outcome.result.is_err() || !host.fuel().should_continue() {
                break;
            }
        }
        flush_shadow(host, &mut shadow);
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

    #[test]
    fn resident_capacity_classes_preserve_canonical_slices() {
        let mut capacities = std::collections::BTreeSet::new();
        for locals in [0, 8, 24, 56, 120, 180] {
            let mut source = String::from("local sum=0 ");
            for index in 0..locals {
                source.push_str(&format!("local p{index}={index} "));
            }
            source.push_str(
                "local function add(v) sum=sum+v end for i=1,200 do add(i) end return sum",
            );
            for budget in [8, 64] {
                let mut traces = Vec::new();
                for mode in [JitMode::Off, JitMode::Auto] {
                    let (mut lua, closure) = state(mode, source.as_bytes());
                    lua.enter(|ctx| {
                        crate::thread::activation::with_test_thread(
                            ctx,
                            ctx.fetch(&closure),
                            &mut Fuel::with(4096),
                            |host| {
                                ctx.jit().0.borrow_mut().stats = Default::default();
                                if mode == JitMode::Auto {
                                    let width = host.with_registers(|_, r| r.stack_frame.len());
                                    let outcome =
                                        Driver::default().run_shadow(ctx, host, 16, budget);
                                    outcome.result.unwrap();
                                    assert_eq!(
                                        outcome.shadow_capacity,
                                        width.next_power_of_two().max(8)
                                    );
                                    assert!(outcome.shadow_captures > 0);
                                    capacities.insert(outcome.shadow_capacity);
                                } else {
                                    host.run_canonical(ctx, 16, budget, 4).result.unwrap();
                                }
                                traces.push(trace(ctx, host));
                            },
                        );
                    });
                }
                assert_eq!(traces[0], traces[1], "locals={locals}, budget={budget}");
            }
        }
        assert_eq!(
            capacities.into_iter().collect::<Vec<_>>(),
            [8, 16, 32, 64, 128, 256]
        );
    }

    #[test]
    fn resident_small_capacity_falls_back_for_wider_callee() {
        let mut source = String::from("local function f(v) ");
        for index in 0..40 {
            source.push_str(&format!("local p{index}={index} "));
        }
        source.push_str("return v+p39 end local sum=0 for i=1,200 do sum=sum+f(i) end return sum");
        let mut traces = Vec::new();
        for mode in [JitMode::Off, JitMode::Auto] {
            let (mut lua, closure) = state(mode, source.as_bytes());
            lua.enter(|ctx| {
                crate::thread::activation::with_test_thread(
                    ctx,
                    ctx.fetch(&closure),
                    &mut Fuel::with(4096),
                    |host| {
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        if mode == JitMode::Auto {
                            let outcome =
                                Driver::default().run_sized::<true, 16>(ctx, host, 16, 64);
                            outcome.result.unwrap();
                            assert_eq!(outcome.shadow_captures, 1);
                            assert!(ctx.jit().0.borrow().stats.native_instructions > 0);
                        } else {
                            host.run_canonical(ctx, 16, 64, 4).result.unwrap();
                        }
                        traces.push(trace(ctx, host));
                    },
                );
            });
        }
        assert_eq!(traces[0], traces[1]);
    }

    #[test]
    fn scoped_resume_rejection_consumes_token_and_releases_lease() {
        let (mut lua, closure) = state(JitMode::Auto, SOURCE);
        lua.enter(|ctx| {
            crate::thread::activation::with_test_thread(
                ctx,
                ctx.fetch(&closure),
                &mut Fuel::with(4096),
                |host| {
                    let partial = prefix(ctx, host, 64).unwrap();
                    let resume = partial.resume.unwrap();
                    let id = ctx
                        .jit_registry()
                        .borrow()
                        .identity(ctx, ctx.fetch(&closure).prototype())
                        .unwrap();
                    let owners = super::super::owner::Shared::strong_count(
                        &ctx.jit().0.borrow().code[&id].code,
                    );
                    host.with_registers(|_, registers| *registers.pc += 1);
                    let original = trace(ctx, host);
                    let mut scope = PairScope::default();
                    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        host.resume_native_paired(ctx, 64, resume, Some(&mut scope))
                    }))
                    .is_err());
                    assert!(scope.resume.is_none() && scope.handoff.is_none());
                    assert_eq!(trace(ctx, host), original);
                    assert_eq!(
                        super::super::owner::Shared::strong_count(
                            &ctx.jit().0.borrow().code[&id].code
                        ),
                        owners - 1
                    );
                },
            );
        });
    }

    #[test]
    fn scoped_resume_refuses_overwriting_pending_work() {
        let (mut lua, closure) = state(JitMode::Auto, SOURCE);
        lua.enter(|ctx| {
            crate::thread::activation::with_test_thread(
                ctx,
                ctx.fetch(&closure),
                &mut Fuel::with(4096),
                |host| {
                    let partial = prefix(ctx, host, 64).unwrap();
                    let mut scope = PairScope::default();
                    scope.resume = partial.resume;
                    assert!(scope.resume.is_some());
                    let frame = host.frame_identity();
                    let replacement = host.with_registers(|closure, registers| {
                        let id = ctx
                            .jit_registry()
                            .borrow()
                            .identity(ctx, closure.prototype())
                            .unwrap();
                        crate::thread::NativeResume::new(
                            ctx,
                            closure,
                            id,
                            frame,
                            *registers.pc,
                            0,
                            ctx.jit().lookup(id).unwrap(),
                        )
                    });
                    let original = trace(ctx, host);
                    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        host.resume_native_paired(ctx, 64, replacement, Some(&mut scope))
                    }))
                    .is_err());
                    assert!(scope.resume.is_some());
                    assert_eq!(trace(ctx, host), original);
                    let original = scope.resume.take().unwrap();
                    assert!(host
                        .resume_native_paired(ctx, 64, original, Some(&mut scope))
                        .is_ok());
                    assert!(scope.resume.is_none());
                },
            );
        });
    }

    #[test]
    fn shadow_driver_recovers_generated_prefix_after_call_depth_refusal() {
        let source = b"local sum=0 local p=10 local function add(v) sum=sum+v end for i=1,10000 do p=p+1 add(i) end return p";
        let mut traces = Vec::new();
        for mode in [JitMode::Off, JitMode::Auto] {
            let (mut lua, closure) = state(mode, source);
            lua.enter(|ctx| {
                let prototype = ctx.fetch(&closure).prototype();
                let start = prototype
                    .opcodes
                    .iter()
                    .position(|op| {
                        matches!(
                            op.decode(),
                            crate::opcode::Operation::Add {
                                dest: crate::types::RegisterIndex(1),
                                ..
                            }
                        )
                    })
                    .unwrap();
                ctx.set_max_call_depth(1);
                crate::thread::activation::with_test_thread(
                    ctx,
                    ctx.fetch(&closure),
                    &mut Fuel::with(4096),
                    |host| {
                        for _ in 0..prototype.opcodes.len() * 2 {
                            if host.with_registers(|_, registers| *registers.pc == start) {
                                break;
                            }
                            host.run_canonical(ctx, 1, 1, 4).result.unwrap();
                        }
                        host.with_registers(|_, registers| {
                            assert_eq!(*registers.pc, start);
                            assert!(matches!(
                                registers.stack_frame[1],
                                crate::Value::Integer(10)
                            ));
                        });
                        host.test_fuel(Fuel::with(4096));
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        if mode == JitMode::Auto {
                            let outcome = Driver::default().run_shadow(ctx, host, 8, 64);
                            outcome.result.unwrap();
                            assert_eq!(outcome.slices, 1);
                            assert_eq!(outcome.shadow_recoveries, 1);
                            assert_eq!(ctx.jit().0.borrow().stats.native_pair_calls, 1);
                        } else {
                            host.run_canonical(ctx, 8, 64, 4).result.unwrap();
                        }
                        assert!(!host.lua_pending());
                        assert!(matches!(host.test_trace().1[1], crate::Value::Integer(11)));
                        traces.push(trace(ctx, host));
                    },
                );
            });
        }
        assert_eq!(traces[0], traces[1]);
    }

    #[test]
    fn shadow_caller_loop_matches_bounded_exits_and_reuses_one_snapshot() {
        let cases = [1, 4, 8, 64]
            .into_iter()
            .flat_map(|budget| {
                [1, 2, 3, 8, 16]
                    .into_iter()
                    .map(move |limit| (budget, limit, 4096))
            })
            .chain([-1, 0, 1, 8, 13, 64].into_iter().map(|fuel| (64, 8, fuel)));
        for (budget, limit, remaining) in cases {
            let mut traces = Vec::new();
            for mode in [JitMode::Off, JitMode::Auto] {
                let (mut lua, closure) = state(mode, SOURCE);
                lua.enter(|ctx| {
                    crate::thread::activation::with_test_thread(
                        ctx,
                        ctx.fetch(&closure),
                        &mut Fuel::with(4096),
                        |host| {
                            for _ in 0..2 {
                                host.run_canonical(ctx, 1, 64, 4).result.unwrap();
                            }
                            host.test_fuel(Fuel::with(remaining));
                            ctx.jit().0.borrow_mut().stats = Default::default();
                            if mode == JitMode::Auto {
                                let outcome =
                                    Driver::default().run_shadow(ctx, host, limit, budget);
                                outcome.result.unwrap();
                                assert!(outcome.slices > 0 && outcome.slices <= limit);
                                if budget == 64 && limit >= 8 && remaining == 4096 {
                                    assert!(outcome.pairs > 1);
                                    assert_eq!(outcome.shadow_captures, 1);
                                }
                            } else {
                                host.run_canonical(ctx, limit, budget, 4).result.unwrap();
                            }
                            traces.push(trace(ctx, host));
                        },
                    );
                });
            }
            assert_eq!(
                traces[0], traces[1],
                "budget={budget} limit={limit} fuel={remaining}"
            );
        }
    }

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
                                    host.run_canonical(ctx, 1, 64, 4).result.unwrap();
                                }
                                ctx.jit().0.borrow_mut().stats = Default::default();
                                if mode == JitMode::Auto {
                                    let mut driver = Driver::default();
                                    let outcome = driver.run(ctx, host, limit, budget);
                                    outcome.result.unwrap();
                                    assert_eq!(outcome.slices, limit);
                                    assert!(ctx.jit().0.borrow().stats.native_instructions != 0);
                                    if limit >= 2 {
                                        assert!(outcome.pairs != 0);
                                    }
                                } else {
                                    host.run_canonical(ctx, limit, budget, 4).result.unwrap();
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
                let partial = prefix(ctx, host, 64).unwrap();
                let instructions = partial.instructions;
                assert!(partial.transition.is_none());
                assert!(partial.resume.is_some());
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
                                host.run_canonical(ctx, 1, 64, 4).result.unwrap();
                            }
                            host.test_fuel(Fuel::with(remaining));
                            ctx.jit().0.borrow_mut().stats = Default::default();
                            if mode == JitMode::Auto {
                                let outcome = Driver::default().run(ctx, host, 8, 64);
                                outcome.result.unwrap();
                                assert!(outcome.slices > 0);
                            } else {
                                host.run_canonical(ctx, 8, 64, 4).result.unwrap();
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
        collection_boundary::<false>();
    }

    #[test]
    fn shadow_chains_materialize_captures_before_collection() {
        collection_boundary::<true>();
    }

    fn collection_boundary<const DEFER: bool>() {
        let (mut lua, closure) = state(JitMode::Auto, SOURCE);
        let thread = lua.enter(|ctx| {
            let thread = Thread::new(ctx);
            thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            crate::thread::activation::with_test_existing_thread(ctx, thread, &mut fuel, |host| {
                for _ in 0..2 {
                    host.run_canonical(ctx, 1, 64, 4).result.unwrap();
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
                        let outcome = driver.run_mode::<DEFER>(ctx, host, 16, 64);
                        outcome.result.unwrap();
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
    fn hooks_and_off_fall_back_without_native_work() {
        hook_boundary::<false>();
    }

    #[test]
    fn shadow_hooks_and_off_fall_back_without_native_work() {
        hook_boundary::<true>();
    }

    fn hook_boundary<const DEFER: bool>() {
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
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        let before = trace(ctx, host);
                        assert!(prefix(ctx, host, 64).is_none());
                        assert_eq!(trace(ctx, host), before);
                        let mut driver = Driver::default();
                        let outcome = driver.run_mode::<DEFER>(ctx, host, 16, 64);
                        outcome.result.unwrap();
                        assert!(outcome.slices > 0);
                        assert_eq!(ctx.jit().0.borrow().stats.native_instructions, 0);
                        assert_eq!(std::mem::size_of_val(&driver), 0);
                    },
                );
            });
        }
    }

    #[test]
    fn native_caller_stops_before_rust_callback_and_releases_host_borrows() {
        callback_boundary::<false>();
    }

    #[test]
    fn shadow_caller_materializes_before_rust_callback() {
        callback_boundary::<true>();
    }

    fn callback_boundary<const DEFER: bool>() {
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
                let outcome = driver.run_mode::<DEFER>(ctx, host, 16, 64);
                outcome.result.unwrap();
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
        panic_boundary::<false>(b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do local remainder=i%3 add(i) local unused=trigger end return sum");
    }

    #[test]
    fn caught_shadow_panic_releases_a_previously_cached_pair_lease() {
        panic_boundary::<true>(b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do local remainder=i%3 add(i) local unused=trigger end return sum");
    }

    #[test]
    fn shadow_helper_panic_materializes_pending_loop_state_after_native_pair() {
        panic_boundary::<true>(b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do add(i) local unused=trigger end return sum");
    }

    fn panic_boundary<const DEFER: bool>(source: &[u8]) {
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
                    host.run_canonical(ctx, 1, 64, 4).result.unwrap();
                }
                let mut driver = Driver::default();
                let setup = driver.run_mode::<DEFER>(ctx, host, 1, 1);
                setup.result.unwrap();
                assert_eq!(setup.slices, 1);
                let base = host.with_registers(|closure, registers| {
                    let crate::opcode::Operation::NumericForLoop { base, .. } =
                        closure.prototype().opcodes[*registers.pc].decode()
                    else {
                        panic!("expected loop head");
                    };
                    base.0 as usize
                });
                let before = ctx.jit().0.borrow().stats.native_pair_calls;
                let globals = ctx.globals().into_inner();
                let lock = globals.borrow_mut(&ctx);
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    driver.run_mode::<DEFER>(ctx, host, 16, 64)
                }));
                assert!(caught.is_err());
                drop(lock);
                assert_eq!(ctx.jit().0.borrow().stats.native_pair_calls - before, 1);
                host.with_registers(|_, registers| {
                    assert!(matches!(registers.stack_frame[0], crate::Value::Integer(3)));
                    assert!(matches!(
                        registers.stack_frame[base],
                        crate::Value::Integer(2)
                    ));
                    assert!(matches!(
                        registers.stack_frame[base + 3],
                        crate::Value::Integer(2)
                    ));
                });
                assert_eq!(super::super::owner::Shared::strong_count(&program), owners);
                drop(ctx.jit().0.borrow_mut());
                let reused = driver.run_mode::<DEFER>(ctx, host, 2, 64);
                reused.result.unwrap();
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
                                let outcome = Driver::default().run(ctx, host, 1, budget);
                                outcome.result.unwrap();
                                assert_eq!(outcome.slices, 1);
                                assert!(ctx.jit().0.borrow().stats.native_instructions > 0);
                            } else {
                                host.run_canonical(ctx, 1, budget, 4).result.unwrap();
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

    fn function_state(source: &[u8]) -> (Lua, crate::StashedClosure) {
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..JitConfig::default()
        })
        .unwrap();
        let function = lua.enter(|ctx| {
            let chunk = Closure::load(ctx, None, source).unwrap();
            let prototype = chunk.prototype().prototypes[0];
            assert!(prototype.upvalues.is_empty());
            ctx.stash(Closure::from_parts(
                &ctx,
                prototype,
                allocator_api2::vec::Vec::new_in(
                    ottavino_gc_arena::allocator_api::MetricsAlloc::new(&ctx),
                ),
            ))
        });
        lua.prepare_jit().unwrap();
        (lua, function)
    }

    #[test]
    fn resumed_guard_error_counts_one_native_attempt_and_no_successful_prefix_fuel() {
        let source = b"return function(a,b) local n=1 local m=2 local u=n+m return a+b end";
        let mut traces = Vec::new();
        let mut statistics = Vec::new();
        let mut errors = Vec::new();
        for resume in [false, true] {
            let (mut lua, function) = function_state(source);
            lua.enter(|ctx| {
                let table = crate::Table::new(&ctx);
                let thread = Thread::new(ctx);
                thread
                    .start(ctx, ctx.fetch(&function).into(), (table, 1))
                    .unwrap();
                let mut fuel = Fuel::with(4096);
                crate::thread::activation::with_test_existing_thread(
                    ctx,
                    thread,
                    &mut fuel,
                    |host| {
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        let result = if resume {
                            let outcome = Driver::default().run(ctx, host, 1, 64);
                            assert_eq!(outcome.slices, 1);
                            outcome.result
                        } else {
                            host.run_canonical(ctx, 1, 64, 4).result
                        };
                        errors.push(result.unwrap_err().to_string());
                        let stats = ctx.jit().0.borrow().stats;
                        assert_eq!(stats.native_entries, 1);
                        assert_eq!(stats.guard_exits, 1);
                        assert_eq!(stats.interpreted_instructions, 0);
                        assert_eq!(stats.interpreted_slices, 0);
                        assert_eq!(host.fuel().remaining(), 4092);
                        traces.push(trace(ctx, host));
                        statistics.push(stats);
                    },
                );
            });
        }
        assert_eq!(errors[0], errors[1]);
        assert_eq!(traces[0], traces[1]);
        assert_eq!(statistics[0], statistics[1]);
    }

    #[test]
    fn resumed_declined_helper_is_not_retried_and_earlier_table_write_is_not_replayed() {
        let source = b"return function(t) t.x=t.x+1 return t.missing end";
        let mut traces = Vec::new();
        let mut statistics = Vec::new();
        let (mut lua, function) = function_state(source);
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = calls.clone();
        let callback = lua.enter(|ctx| {
            ctx.stash(Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                counter.set(counter.get() + 1);
                stack.replace(ctx, 7);
                Ok(CallbackReturn::Return)
            }))
        });
        for resume in [false, true] {
            calls.set(0);
            let (table, executor) = lua.enter(|ctx| {
                let table = crate::Table::new(&ctx);
                table.set(ctx, "x", 0).unwrap();
                let metatable = crate::Table::new(&ctx);
                metatable.set(ctx, "__index", ctx.fetch(&callback)).unwrap();
                table.set_metatable(ctx, Some(metatable));
                let thread = Thread::new(ctx);
                thread
                    .start(ctx, ctx.fetch(&function).into(), table)
                    .unwrap();
                let mut fuel = Fuel::with(4096);
                crate::thread::activation::with_test_existing_thread(
                    ctx,
                    thread,
                    &mut fuel,
                    |host| {
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        if resume {
                            let outcome = Driver::default().run(ctx, host, 1, 64);
                            outcome.result.unwrap();
                            assert_eq!(outcome.slices, 1);
                        } else {
                            host.run_canonical(ctx, 1, 64, 4).result.unwrap();
                        }
                        let stats = ctx.jit().0.borrow().stats;
                        assert_eq!(stats.helper_declines, 1);
                        assert_eq!(stats.native_table_writes, 1);
                        assert_eq!(calls.get(), 0);
                        assert!(!host.lua_ready());
                        traces.push(trace(ctx, host));
                        statistics.push(stats);
                    },
                );
                (
                    ctx.stash(table),
                    ctx.stash(Executor::run(&ctx, thread).unwrap()),
                )
            });
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), 7);
            assert_eq!(calls.get(), 1);
            lua.enter(|ctx| {
                assert!(matches!(
                    ctx.fetch(&table).get::<_, crate::Value>(ctx, "x").unwrap(),
                    crate::Value::Integer(1)
                ))
            });
        }
        assert_eq!(traces[0], traces[1]);
        assert_eq!(statistics[0], statistics[1]);
    }

    #[test]
    fn resumed_driver_does_not_inherit_a_previous_activation_pair_handoff() {
        let source = b"local sum=0 local one=1 local scale=2 sum=sum+one local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";
        let mut traces = Vec::new();
        for driver in [false, true] {
            let (mut lua, closure) = state(JitMode::Auto, source);
            lua.enter(|ctx| {
                let thread = Thread::new(ctx);
                thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
                let mut fuel = Fuel::empty();
                crate::thread::activation::with_test_existing_thread(
                    ctx,
                    thread,
                    &mut fuel,
                    |host| {
                        host.run_canonical(ctx, 16, 4, 4).result.unwrap();
                        host.test_fuel(Fuel::with(4096));
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        if driver {
                            let outcome = Driver::default().run(ctx, host, 1, 64);
                            outcome.result.unwrap();
                            assert_eq!(outcome.slices, 1);
                        } else {
                            host.run_canonical(ctx, 1, 64, 4).result.unwrap();
                        }
                        traces.push(trace(ctx, host));
                    },
                );
            });
        }
        assert_eq!(traces[0], traces[1]);
    }

    #[test]
    fn cold_native_driver_preserves_canonical_traces_and_lookup_statistics() {
        let source = b"local sum=0 for i=1,1000 do sum=sum+i end return sum";
        for budget in [0, 1, 8, 64] {
            for limit in [1, 2, 8] {
                let mut traces = Vec::new();
                let mut statistics = Vec::new();
                for driver in 0..3 {
                    let mut lua = Lua::empty();
                    lua.set_jit_config(JitConfig {
                        mode: JitMode::Auto,
                        ..JitConfig::default()
                    })
                    .unwrap();
                    lua.enter(|ctx| {
                        let closure = Closure::load(ctx, None, source).unwrap();
                        let thread = Thread::new(ctx);
                        thread.start(ctx, closure.into(), ()).unwrap();
                        let mut fuel = Fuel::with(4096);
                        crate::thread::activation::with_test_existing_thread(
                            ctx,
                            thread,
                            &mut fuel,
                            |host| {
                                ctx.jit().0.borrow_mut().stats = Default::default();
                                if driver != 0 {
                                    let outcome = if driver == 1 {
                                        Driver::default().run(ctx, host, limit, budget)
                                    } else {
                                        Driver::default().run_shadow(ctx, host, limit, budget)
                                    };
                                    outcome.result.unwrap();
                                    assert_eq!(outcome.slices, limit);
                                } else {
                                    host.run_canonical(ctx, limit, budget, 4).result.unwrap();
                                }
                                assert_eq!(ctx.jit().0.borrow().stats.native_entries, 0);
                                traces.push(trace(ctx, host));
                                statistics.push(ctx.jit().0.borrow().stats);
                            },
                        );
                    });
                }
                assert_eq!(traces[0], traces[1], "budget={budget} limit={limit}");
                assert_eq!(
                    statistics[0], statistics[1],
                    "budget={budget} limit={limit}"
                );
                assert_eq!(traces[0], traces[2], "shadow budget={budget} limit={limit}");
                assert_eq!(
                    statistics[0], statistics[2],
                    "shadow budget={budget} limit={limit}"
                );
            }
        }
    }

    #[test]
    fn native_resume_interprets_after_code_retirement_or_mode_change() {
        let source = b"local sum=0 local one=1 local scale=2 sum=sum+one local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";
        let (mut control, closure) = state(JitMode::Auto, source);
        let expected = control.enter(|ctx| {
            let thread = Thread::new(ctx);
            thread.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            crate::thread::activation::with_test_existing_thread(ctx, thread, &mut fuel, |host| {
                ctx.jit().0.borrow_mut().stats = Default::default();
                host.run_canonical(ctx, 1, 64, 4).result.unwrap();
                trace(ctx, host)
            })
        });
        for off in [false, true] {
            let (mut lua, closure) = state(JitMode::Auto, source);
            let executor = lua.enter(|ctx| {
                let closure = ctx.fetch(&closure);
                let thread = Thread::new(ctx);
                thread.start(ctx, closure.into(), ()).unwrap();
                let mut fuel = Fuel::with(4096);
                crate::thread::activation::with_test_existing_thread(
                    ctx,
                    thread,
                    &mut fuel,
                    |host| {
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        let prefix = prefix(ctx, host, 64).unwrap();
                        let instructions = prefix.instructions;
                        assert!(instructions > 0);
                        let id = ctx
                            .jit_registry()
                            .borrow()
                            .identity(ctx, closure.prototype())
                            .unwrap();
                        {
                            let mut manager = ctx.jit().0.borrow_mut();
                            if off {
                                let config = JitConfig {
                                    mode: JitMode::Off,
                                    ..manager.config.clone()
                                };
                                manager.configure(config);
                            } else {
                                assert!(manager.code.remove(&id).is_some());
                            }
                        }
                        let before = ctx.jit().0.borrow().stats;
                        let completed =
                            host.resume_native(ctx, 64, prefix.resume.unwrap()).unwrap();
                        host.charge_native_slice(completed);
                        let after = ctx.jit().0.borrow().stats;
                        assert_eq!(after.native_entries, before.native_entries);
                        assert_eq!(after.native_instructions, before.native_instructions);
                        assert!(after.interpreted_instructions > before.interpreted_instructions);
                        assert_eq!(trace(ctx, host), expected);
                    },
                );
                ctx.stash(Executor::run(&ctx, thread).unwrap())
            });
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), 50005001);
        }
    }

    #[test]
    fn native_resume_refuses_changed_pc_or_exhausted_budget_before_effects() {
        let source = b"local sum=0 local one=1 local scale=2 sum=sum+one local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";
        for changed_pc in [false, true] {
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
                        let prefix = prefix(ctx, host, 64).unwrap();
                        let budget = if changed_pc {
                            host.with_registers(|_, registers| *registers.pc += 1);
                            64
                        } else {
                            prefix.instructions
                        };
                        let before = trace(ctx, host);
                        let statistics = ctx.jit().0.borrow().stats;
                        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            host.resume_native(ctx, budget, prefix.resume.unwrap())
                        }));
                        assert!(caught.is_err());
                        assert_eq!(trace(ctx, host), before);
                        assert_eq!(ctx.jit().0.borrow().stats, statistics);
                    },
                );
            });
        }
    }

    #[test]
    fn native_resume_refuses_a_different_thread_with_the_same_closure_and_pc() {
        let source = b"local sum=0 local one=1 local scale=2 sum=sum+one local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";
        let (mut lua, closure) = state(JitMode::Auto, source);
        lua.enter(|ctx| {
            let first = Thread::new(ctx);
            first.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            let (resume, pc) = crate::thread::activation::with_test_existing_thread(
                ctx,
                first,
                &mut fuel,
                |host| {
                    let token = prefix(ctx, host, 64).unwrap().resume.unwrap();
                    let pc = host.with_registers(|_, registers| *registers.pc);
                    (token, pc)
                },
            );
            let second = Thread::new(ctx);
            second.start(ctx, ctx.fetch(&closure).into(), ()).unwrap();
            let mut fuel = Fuel::with(4096);
            crate::thread::activation::with_test_existing_thread(ctx, second, &mut fuel, |host| {
                host.with_registers(|_, registers| *registers.pc = pc);
                let before = trace(ctx, host);
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    host.resume_native(ctx, 64, resume)
                }));
                assert!(caught.is_err());
                assert_eq!(trace(ctx, host), before);
            });
        });
    }
}
