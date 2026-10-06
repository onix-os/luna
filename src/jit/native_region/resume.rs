use crate::jit::{owner::Shared, JitMode, Manager, Prepared, Runtime, RuntimeOwner};
use crate::{
    thread::{activation::ActivationHost, VMError},
    Closure, Context,
};

pub(in crate::jit) struct State {
    source: u64,
    closure: usize,
    stack: (usize, usize),
    pc: usize,
    owner: usize,
    lease: Option<Prepared>,
    claimed: bool,
    native: bool,
    pending: bool,
}

pub(super) struct Token<'gc> {
    runtime: Runtime,
    closure: Closure<'gc>,
    source: u64,
    frame: (usize, usize),
    pc: usize,
    prefix: u32,
    prepared: Prepared,
}

impl<'gc> Token<'gc> {
    pub(super) fn new(
        ctx: Context<'gc>,
        closure: Closure<'gc>,
        source: u64,
        frame: (usize, usize),
        pc: usize,
        prefix: u32,
        prepared: Prepared,
    ) -> Self {
        Self {
            runtime: ctx.jit().clone(),
            closure,
            source,
            frame,
            pc,
            prefix,
            prepared,
        }
    }

    pub(super) fn run(
        self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
    ) -> Result<u32, VMError> {
        self.run_paired(ctx, host, budget, None)
    }

    pub(super) fn run_paired(
        self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
        scope: Option<&mut crate::jit::PairScope<'gc>>,
    ) -> Result<u32, VMError> {
        assert!(self.prefix < budget);
        assert!(RuntimeOwner::ptr_eq(&self.runtime.0, &ctx.jit().0));
        assert_eq!(host.frame_identity(), self.frame);
        let stack = host.with_registers(|closure, registers| {
            assert_eq!(closure, self.closure);
            assert_eq!(*registers.pc, self.pc);
            assert_eq!(
                ctx.jit_registry()
                    .borrow()
                    .identity(ctx, closure.prototype()),
                Some(self.source)
            );
            registers.shadow_key()
        });
        let state = State {
            source: self.source,
            closure: ottavino_gc_arena::Gc::as_ptr(self.closure.into_inner()) as usize,
            stack,
            pc: self.pc,
            owner: std::ptr::from_ref(&*self.prepared.code) as usize,
            lease: Some(self.prepared),
            claimed: false,
            native: false,
            pending: true,
        };
        with_scope(ctx.jit(), state, || {
            host.canonical_slice_paired(ctx, budget - self.prefix, scope)
        })
        .map(|rest| {
            assert!(rest <= budget - self.prefix);
            self.prefix + rest
        })
    }
}

struct Guard<'a> {
    runtime: &'a Runtime,
    previous: Option<State>,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.runtime.0.borrow_mut().resume_scope = self.previous.take();
    }
}

fn with_scope<R>(runtime: &Runtime, state: State, body: impl FnOnce() -> R) -> R {
    let previous = runtime.0.borrow_mut().resume_scope.replace(state);
    let _guard = Guard { runtime, previous };
    body()
}

pub(in crate::jit) fn lookup(manager: &mut Manager, id: u64) -> Option<Option<Prepared>> {
    let state = manager.resume_scope.as_mut()?;
    if state.source != id || state.claimed {
        return None;
    }
    state.claimed = true;
    let prepared = state.lease.take().filter(|prepared| {
        manager.config.mode == JitMode::Auto
            && manager
                .code
                .get(&id)
                .is_some_and(|entry| Shared::ptr_eq(&entry.code, &prepared.code))
    });
    state.native = prepared.is_some();
    Some(prepared)
}

pub(in crate::jit) fn skip(
    runtime: &Runtime,
    prepared: &Prepared,
    closure: Closure<'_>,
    registers: &crate::thread::LuaRegisters<'_, '_>,
) -> bool {
    let mut manager = runtime.0.borrow_mut();
    let Some(state) = &mut manager.resume_scope else {
        return false;
    };
    if !state.claimed
        || !state.pending
        || !state.native
        || state.owner != std::ptr::from_ref(&*prepared.code) as usize
        || state.pc != *registers.pc
        || state.stack != registers.shadow_key()
        || state.closure != ottavino_gc_arena::Gc::as_ptr(closure.into_inner()) as usize
    {
        return false;
    }
    state.pending = false;
    true
}

pub(in crate::jit) fn skip_observation(manager: &mut Manager, id: u64) -> bool {
    let Some(state) = &mut manager.resume_scope else {
        return false;
    };
    if state.source == id && state.claimed && !state.native && state.pending {
        state.pending = false;
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::native_region::tests::{fixture_source, trace};
    use crate::{thread::activation::with_test_thread, Fuel};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    const SOURCE: &[u8] = b"local n=0 local function f(v) n=n+v end local a=8 local b=3 local q=a%b local s=q+2 f(s) return n";

    fn prefix<'gc>(
        ctx: Context<'gc>,
        closure: Closure<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        source: u64,
        budget: u32,
    ) -> (Prepared, u32, usize) {
        let code = ctx.jit().lookup(source).unwrap();
        host.clear_hook(ctx);
        let (count, pc) = host.with_registers(|_, mut registers| {
            let count = ctx.jit().run(&code, ctx, closure, &mut registers, budget);
            (count, *registers.pc)
        });
        (code, count, pc)
    }

    #[test]
    fn scoped_resume_matches_legacy_and_canonical_slices_including_errors() {
        for source in [
            SOURCE,
            &b"local n=0 local function f(v) n=n+v end local a={} local b=3 local q=a+b f(q) return n"[..],
            &b"local n=0 local function f(v) n=n+v end local t={} local a=3 t.x=a local q=a.foo f(q) return n"[..],
        ] {
            fixture_source(source, |ctx, closure, region, start| {
                for budget in [4, 8, 64] {
                    let mut expected = None;
                    for mode in 0..3 {
                        let actual = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                            host.run(ctx, 1, start as u32, 4).result.unwrap();
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                            ctx.jit().0.borrow_mut().stats = Default::default();
                            let result = if mode == 0 { host.run(ctx, 1, budget, 4).result }
                            else {
                                let (code, count, pc) = prefix(ctx, closure, host, region.source, budget);
                                let result = if count == budget { Ok(count) }
                                else if mode == 1 {
                                    let token = crate::thread::NativeResume::new(ctx, closure, region.source, host.frame_identity(), pc, count, code);
                                    host.resume_native(ctx, budget, token)
                                } else {
                                    Token::new(ctx, closure, region.source, host.frame_identity(), pc, count, code).run(ctx, host, budget)
                                };
                                if let Ok(count) = result { host.charge_instructions(count); }
                                host.charge_native_slice(0);
                                result.map(|_| ())
                            };
                            assert!(ctx.jit().0.borrow().resume_scope.is_none());
                            (trace(ctx, host, (0, 0)), result.err().map(|error| error.to_string()), ctx.jit().0.borrow().stats)
                        });
                        if let Some(expected) = &expected { assert_eq!(&actual, expected, "mode={mode} budget={budget}"); }
                        else { expected = Some(actual); }
                    }
                }
            });
        }
    }

    #[test]
    fn paired_scope_preserves_legacy_handoff_budget_fuel_and_work() {
        for (source, succeeds) in [
            (SOURCE, true),
            (&b"local n=0 local function f(v) n=n+v end local a=8 local b=3 local q=a<b local s=2 if q then s=3 end f(s) return n"[..], true),
            (&b"local n=0 local function f(v) n=n+v end local a={} local b=3 local q=a+b f(q) return n"[..], false),
        ] {
            fixture_source(source, |ctx, closure, region, start| {
                for budget in [1, 2, 3, 4, 5, 8, 16, 64] {
                    for fuel in [-1, 0, 1, 8, 20, 10000] {
                        let mut expected = None;
                        let mut slices = 0;
                        for scoped in [false, true] {
                            let actual = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                                host.run(ctx, 1, start as u32, 4).result.unwrap();
                                host.test_fuel(Fuel::with(fuel));
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                                ctx.jit().0.borrow_mut().stats = Default::default();
                                let (code, count, pc) = prefix(ctx, closure, host, region.source, budget);
                                let mut scope = crate::jit::PairScope::default();
                                let result = if count == budget { Ok(count) }
                                else if scoped {
                                    Token::new(ctx, closure, region.source, host.frame_identity(), pc, count, code)
                                        .run_paired(ctx, host, budget, (budget >= 4).then_some(&mut scope))
                                } else {
                                    let token = crate::thread::NativeResume::new(ctx, closure, region.source, host.frame_identity(), pc, count, code);
                                    host.resume_native_paired(ctx, budget, token, (budget >= 4).then_some(&mut scope))
                                };
                                assert!(ctx.jit().0.borrow().resume_scope.is_none());
                                slices = 1;
                                let handoff = scope.handoff.is_some();
                                let (result, charged) = if let Some(pair) = scope.handoff.take() {
                                    let completed = result.unwrap();
                                    assert!(completed < budget);
                                    if let Some(paired) = pair.invoke(ctx, host, budget, completed) {
                                        slices += paired.returns;
                                        (paired.result.map(|()| 0), true)
                                    } else {
                                        (host.canonical_slice(ctx, budget - completed).map(|n| completed + n), false)
                                    }
                                } else { (result, false) };
                                if let Ok(count) = result { host.charge_instructions(count); }
                                if !charged { host.charge_native_slice(0); }
                                if budget == 64 && fuel == 10000 {
                                    assert_eq!(handoff, succeeds);
                                    assert_eq!(slices, if succeeds { 2 } else { 1 });
                                    assert_eq!(ctx.jit().0.borrow().stats.native_pair_returns, u64::from(succeeds));
                                }
                                (trace(ctx, host, (0, 0)), result.err().map(|e| e.to_string()), ctx.jit().0.borrow().stats, slices, handoff)
                            });
                            if let Some(expected) = &expected {
                                assert_eq!(&actual, expected, "budget={budget} fuel={fuel}");
                            } else { expected = Some(actual); }
                        }
                        let canonical = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                            host.run(ctx, 1, start as u32, 4).result.unwrap();
                            host.test_fuel(Fuel::with(fuel));
                            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                            ctx.jit().0.borrow_mut().stats = Default::default();
                            let result = host.run(ctx, slices, budget, 4).result;
                            (trace(ctx, host, (0, 0)), result.err().map(|e| e.to_string()))
                        });
                        let expected = expected.unwrap();
                        assert_eq!(canonical, (expected.0, expected.1), "budget={budget} fuel={fuel}");
                    }
                }
            });
        }
    }

    #[test]
    fn paired_scope_rejects_pending_handoff_or_legacy_resume_before_effects() {
        fixture_source(SOURCE, |ctx, closure, region, start| {
            for legacy in [false, true] {
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, start as u32, 4).result.unwrap();
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let (code, count, pc) = prefix(ctx, closure, host, region.source, 64);
                    let mut scope = crate::jit::PairScope::default();
                    if legacy {
                        scope.resume = Some(crate::thread::NativeResume::new(
                            ctx,
                            closure,
                            region.source,
                            host.frame_identity(),
                            pc,
                            count,
                            Prepared {
                                code: code.code.clone(),
                            },
                        ));
                    } else {
                        scope.handoff = Some(crate::jit::PreparedPair {
                            program: region.pair.program.clone(),
                        });
                    }
                    let token = Token::new(
                        ctx,
                        closure,
                        region.source,
                        host.frame_identity(),
                        pc,
                        count,
                        code,
                    );
                    let before = trace(ctx, host, (0, 0));
                    let stats = ctx.jit().0.borrow().stats;
                    assert!(catch_unwind(AssertUnwindSafe(|| token.run_paired(
                        ctx,
                        host,
                        64,
                        Some(&mut scope)
                    )))
                    .is_err());
                    assert_eq!(trace(ctx, host, (0, 0)), before);
                    assert_eq!(ctx.jit().0.borrow().stats, stats);
                    assert!(ctx.jit().0.borrow().resume_scope.is_none());
                    assert_eq!(scope.resume.is_some(), legacy);
                    assert_eq!(scope.handoff.is_some(), !legacy);
                });
            }
        });
    }

    #[test]
    fn scoped_resume_refuses_changed_pc_frame_runtime_and_budget_before_effects() {
        fixture_source(SOURCE, |ctx, closure, region, start| {
            for fault in 0..4 {
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, start as u32, 4).result.unwrap();
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let (code, count, pc) = prefix(ctx, closure, host, region.source, 64);
                    let mut token = Token::new(
                        ctx,
                        closure,
                        region.source,
                        host.frame_identity(),
                        pc,
                        count,
                        code,
                    );
                    match fault {
                        0 => host.with_registers(|_, registers| *registers.pc += 1),
                        1 => token.frame.0 ^= 1,
                        2 => token.runtime = Runtime::new(),
                        _ => {}
                    }
                    let before = trace(ctx, host, (0, 0));
                    assert!(catch_unwind(AssertUnwindSafe(|| token.run(
                        ctx,
                        host,
                        if fault == 3 { count } else { 64 }
                    )))
                    .is_err());
                    assert_eq!(trace(ctx, host, (0, 0)), before);
                    assert!(ctx.jit().0.borrow().resume_scope.is_none());
                    drop(ctx.jit().0.borrow_mut());
                });
            }
        });
    }

    #[test]
    fn retired_or_disabled_scoped_resume_matches_legacy_without_native_reentry() {
        for source in [SOURCE, &b"local n=0 local function f(v) n=n+v end local a={} local b=3 local q=a+b f(q) return n"[..]] {
        for change in 0..3 {
            let mut expected = None;
            for scoped in [false, true] {
                fixture_source(source, |ctx, closure, region, start| {
                    with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                        host.run(ctx, 1, start as u32, 4).result.unwrap();
                        ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                        ctx.jit().0.borrow_mut().config.hot_threshold = 1;
                        ctx.jit().0.borrow_mut().stats = Default::default();
                        let (code, count, pc) = prefix(ctx, closure, host, region.source, 64);
                        match change {
                            0 => {
                                ctx.jit().0.borrow_mut().code.remove(&region.source);
                            }
                            1 => ctx.jit().0.borrow_mut().config.mode = JitMode::Off,
                            2 => {
                                let allocator = ctx.jit().0.borrow().snapshots.clone();
                                let snapshot = crate::jit::ir::Snapshot::new_in(
                                    &closure.prototype(),
                                    4096,
                                    allocator,
                                )
                                .unwrap();
                                ctx.jit().compile(region.source, snapshot).unwrap();
                            }
                            _ => unreachable!(),
                        }
                        let before = ctx.jit().0.borrow().stats.native_instructions;
                        let result = if scoped {
                            Token::new(
                                ctx,
                                closure,
                                region.source,
                                host.frame_identity(),
                                pc,
                                count,
                                code,
                            )
                            .run(ctx, host, 64)
                        } else {
                            let token = crate::thread::NativeResume::new(
                                ctx,
                                closure,
                                region.source,
                                host.frame_identity(),
                                pc,
                                count,
                                code,
                            );
                            host.resume_native(ctx, 64, token)
                        };
                        let error = result.as_ref().err().map(|error| error.to_string());
                        host.charge_native_slice(result.unwrap_or(0));
                        assert_eq!(ctx.jit().0.borrow().stats.native_instructions, before);
                        assert!(ctx.jit().0.borrow().resume_scope.is_none());
                        let actual = (trace(ctx, host, (0, 0)), error, ctx.jit().0.borrow().stats);
                        if let Some(expected) = &expected {
                            assert_eq!(&actual, expected, "change={change}");
                        } else {
                            expected = Some(actual);
                        }
                    });
                });
            }
        }
        }
    }

    fn state<'gc>(
        closure: Closure<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        source: u64,
        code: &Prepared,
    ) -> State {
        let (stack, pc) =
            host.with_registers(|_, registers| (registers.shadow_key(), *registers.pc));
        State {
            source,
            closure: ottavino_gc_arena::Gc::as_ptr(closure.into_inner()) as usize,
            stack,
            pc,
            owner: std::ptr::from_ref(&*code.code) as usize,
            lease: Some(Prepared {
                code: code.code.clone(),
            }),
            claimed: false,
            native: false,
            pending: true,
        }
    }

    #[test]
    fn scope_skips_only_exact_owner_closure_stack_and_pc_once() {
        fixture_source(SOURCE, |ctx, closure, region, _| {
            let other = Closure::load(ctx, None, b"return 9").unwrap();
            let id = ctx
                .jit_registry()
                .borrow()
                .identity(ctx, other.prototype())
                .unwrap();
            let allocator = ctx.jit().0.borrow().snapshots.clone();
            let snapshot =
                crate::jit::ir::Snapshot::new_in(&other.prototype(), 4096, allocator).unwrap();
            ctx.jit().compile(id, snapshot).unwrap();
            let other_code = ctx.jit().lookup(id).unwrap();
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                let code = ctx.jit().lookup(region.source).unwrap();
                let state = state(closure, host, region.source, &code);
                with_scope(ctx.jit(), state, || {
                    let selected = ctx.jit().lookup(region.source).unwrap();
                    assert!(Shared::ptr_eq(&code.code, &selected.code));
                    with_test_thread(ctx, closure, &mut Fuel::with(10000), |other| {
                        other.with_registers(|_, registers| {
                            assert!(!skip(ctx.jit(), &selected, closure, &registers))
                        });
                    });
                    host.with_registers(|_, registers| {
                        assert!(!skip(ctx.jit(), &other_code, closure, &registers));
                        assert!(!skip(ctx.jit(), &selected, other, &registers));
                        *registers.pc += 1;
                        assert!(!skip(ctx.jit(), &selected, closure, &registers));
                        *registers.pc -= 1;
                        assert!(skip(ctx.jit(), &selected, closure, &registers));
                        assert!(!skip(ctx.jit(), &selected, closure, &registers));
                    });
                });
                assert!(ctx.jit().0.borrow().resume_scope.is_none());
                host.with_registers(|_, mut registers| {
                    assert!(ctx.jit().run(&code, ctx, closure, &mut registers, 1) > 0);
                    assert!(matches!(registers.stack_frame[0], crate::Value::Integer(0)));
                });
            });
        });
    }

    #[test]
    fn nested_scopes_restore_pending_state_and_release_leases_on_unwind() {
        fixture_source(SOURCE, |ctx, closure, region, _| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                let code = ctx.jit().lookup(region.source).unwrap();
                let owners = Shared::strong_count(&code.code);
                let outer = state(closure, host, region.source, &code);
                with_scope(ctx.jit(), outer, || {
                    let mut inner = state(closure, host, region.source, &code);
                    inner.pc += 1;
                    assert!(catch_unwind(AssertUnwindSafe(|| with_scope(
                        ctx.jit(),
                        inner,
                        || panic!("nested resume sentinel")
                    )))
                    .is_err());
                    let manager = ctx.jit().0.borrow();
                    let restored = manager.resume_scope.as_ref().unwrap();
                    assert_eq!(restored.pc, 0);
                    assert!(!restored.claimed && restored.pending && restored.lease.is_some());
                    assert_eq!(Shared::strong_count(&code.code), owners + 1);
                });
                assert!(ctx.jit().0.borrow().resume_scope.is_none());
                assert_eq!(Shared::strong_count(&code.code), owners);
                let outer = state(closure, host, region.source, &code);
                assert!(catch_unwind(AssertUnwindSafe(|| with_scope(
                    ctx.jit(),
                    outer,
                    || panic!("outer resume sentinel")
                )))
                .is_err());
                assert!(ctx.jit().0.borrow().resume_scope.is_none());
                assert_eq!(Shared::strong_count(&code.code), owners);
            });
        });
    }
}
