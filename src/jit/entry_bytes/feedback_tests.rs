use super::*;
use crate::jit::resources::Ledger;

fn entries() -> (EntryBytes, crate::jit::resources::LedgerRef) {
    let ledger = Ledger::new(4096);
    let entries = EntryBytes::try_new(
        [true, false, true].into_iter(),
        BudgetAllocator(ledger.clone()),
    )
    .unwrap();
    (entries, ledger)
}

#[test]
fn retry_window_is_exact_finite_and_does_not_change_static_admission() {
    with_feedback(|| {
        let (entries, ledger) = entries();
        assert!(entries.permit(0));
        entries.defer_short_guard(0);
        for visit in 0..64 {
            assert!(!entries.permit(0), "visit={visit}");
            assert_eq!(entries.get(0), Some(&true));
            assert!(entries.permit(2));
            assert!(!entries.permit(1));
        }
        assert!(entries.permit(0));
        assert!(entries.permit(0));
        assert!(entries.iter().copied().eq([true, false, true]));
        assert_eq!(ledger.current(), 6);
        drop(entries);
        assert_eq!(ledger.current(), 0);
    });
}

#[test]
fn hints_and_feedback_preserve_every_independent_bitfield() {
    with_feedback(|| {
        let (entries, _) = entries();
        let offsets: &[usize] = if cfg!(miri) {
            &[0, 1, 63, 126, 127, usize::MAX]
        } else {
            &(0..=255).collect::<Vec<_>>()
        };
        for &offset in offsets {
            entries.defer_short_guard(0);
            for remaining in (1..=64).rev() {
                let hint = entries.hint(0).unwrap();
                hint.remember(Some(offset));
                assert_eq!(hint.load(), (offset < 127).then_some(offset));
                assert_eq!(entries.bytes[0].load(Ordering::Relaxed) >> 8, remaining);
                assert!(!entries.permit(0));
            }
            assert!(entries.permit(0));
            assert_eq!(
                entries.hint(0).unwrap().load(),
                (offset < 127).then_some(offset)
            );
        }
        for pc in [1, 3, usize::MAX] {
            entries.defer_short_guard(pc);
            assert!(!entries.permit(pc));
        }
        entries.hint(1).unwrap().remember(Some(126));
        assert!(!entries.permit(1));
        assert_eq!(entries.bytes[1].load(Ordering::Relaxed) >> 8, 0);
    });
}

#[test]
fn renewed_feedback_starts_a_new_window_without_affecting_peers() {
    with_feedback(|| {
        let (entries, _) = entries();
        entries.defer_short_guard(0);
        for _ in 0..19 {
            assert!(!entries.permit(0));
        }
        entries.defer_short_guard(0);
        entries.defer_short_guard(2);
        for _ in 0..64 {
            assert!(!entries.permit(0));
        }
        assert!(entries.permit(0));
        assert_eq!(entries.bytes[2].load(Ordering::Relaxed) >> 8, 64);
        let (replacement, _) = super::feedback_tests::entries();
        assert!(replacement.permit(0));
        assert_eq!(replacement, entries);
    });
}

#[test]
fn concurrent_hint_and_feedback_updates_cannot_clobber_other_fields() {
    let (entries, _) = entries();
    let rounds = if cfg!(miri) { 8 } else { 8192 };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            with_feedback(|| {
                for _ in 0..rounds {
                    entries.defer_short_guard(0);
                }
            })
        });
        scope.spawn(|| {
            for round in 0..rounds {
                entries.hint(0).unwrap().remember(Some(round % 127));
            }
        });
    });
    assert_eq!(entries.bytes[0].load(Ordering::Relaxed) >> 8, 64);
    assert_eq!(entries.hint(0).unwrap().load(), Some((rounds - 1) % 127));
    assert!(entries[0]);
    with_feedback(|| {
        for _ in 0..64 {
            assert!(!entries.permit(0));
        }
        assert!(entries.permit(0));
    });
}

#[cfg(all(not(miri), luna_jit_native))]
#[test]
fn actual_metamethod_failures_do_not_poison_later_numeric_loops() {
    use crate::{Closure, Executor, JitConfig, JitMode, Lua};
    with_feedback(|| {
        let mut lua = Lua::core();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        let source = b"local mt={} mt.__add=function(a,b) return setmetatable({n=a.n+b.n},mt) end local s,one if numeric then s=0 one=1 else s=setmetatable({n=0},mt) one=setmetatable({n=1},mt) end for i=1,256 do s=s+one end if numeric then return s else return s.n end";
        let closure = lua
            .try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, source)?)))
            .unwrap();
        assert!(lua.prepare_jit().unwrap() > 0);
        for numeric in [false, true, false, true] {
            let before = lua.jit_stats();
            let executor = lua.enter(|ctx| {
                ctx.set_global("numeric", numeric);
                ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ()))
            });
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), 256);
            let after = lua.jit_stats();
            assert_eq!(after.compilation_failures, 0);
            if numeric {
                assert!(after.native_instructions - before.native_instructions > 128);
            } else {
                assert!(after.guard_exits - before.guard_exits < 32);
                assert!(after.native_instructions > before.native_instructions);
            }
            lua.gc_collect();
        }
    });
}

#[test]
fn only_short_helper_free_guards_defer_entries() {
    use crate::jit::helpers::Counts;
    with_feedback(|| {
        let (entries, _) = entries();
        for instructions in [2, 3, u32::MAX] {
            entries.defer_guard(0, instructions, &Counts::default());
            assert!(entries.permit(0));
        }
        for field in 0..8 {
            for value in [1, u64::MAX] {
                let mut counts = Counts::default();
                let fields = [
                    &mut counts.calls,
                    &mut counts.completed,
                    &mut counts.declined,
                    &mut counts.table_reads,
                    &mut counts.table_writes,
                    &mut counts.upvalue_reads,
                    &mut counts.upvalue_writes,
                    &mut counts.allocations,
                ];
                *fields[field] = value;
                for instructions in [0, 1] {
                    entries.defer_guard(0, instructions, &counts);
                    assert!(entries.permit(0));
                }
            }
        }
        for instructions in [0, 1] {
            entries.defer_guard(0, instructions, &Counts::default());
            for _ in 0..64 {
                assert!(!entries.permit(0));
            }
            assert!(entries.permit(0));
        }
    });
}

#[cfg(all(not(miri), luna_jit_native))]
#[test]
fn feedback_preserves_every_executor_slice_frame_fuel_gc_and_error() {
    use crate::{Closure, Executor, Fuel, JitConfig, JitMode, Lua};
    let sources: &[&[u8]] = &[
        b"local mt={} mt.__add=function(a,b) return setmetatable({n=a.n+b.n},mt) end local s=setmetatable({n=0},mt) local one=setmetatable({n=1},mt) for i=1,192 do s=s+one end return s.n",
        b"local mt={} mt.__add=function(a,b) if a.n==73 then error('feedback error') end return setmetatable({n=a.n+b.n},mt) end local s=setmetatable({n=0},mt) local one=setmetatable({n=1},mt) for i=1,192 do s=s+one end return s.n",
        b"local co=coroutine.create(function() local mt={} mt.__add=function(a,b) if a.n==73 then coroutine.yield(73) end return setmetatable({n=a.n+b.n},mt) end local s=setmetatable({n=0},mt) local one=setmetatable({n=1},mt) for i=1,192 do s=s+one end return s.n end) local ok,v=coroutine.resume(co) assert(ok and v==73) ok,v=coroutine.resume(co) assert(ok) return v",
    ];
    for (scenario, source) in sources.iter().enumerate() {
        for fuel in [1, 2, 64, 1024] {
            let run = || {
                let mut lua = Lua::core();
                lua.set_jit_config(JitConfig {
                    mode: JitMode::Auto,
                    hot_threshold: 1,
                    ..Default::default()
                })
                .unwrap();
                let executor = lua.enter(|ctx| {
                    let closure = Closure::load(ctx, None, source).unwrap();
                    ctx.stash(Executor::start(ctx, closure.into(), ()))
                });
                lua.prepare_jit().unwrap();
                let mut trace = Vec::new();
                for _ in 0..10000 {
                    let mut allowance = Fuel::with(fuel);
                    let (finished, error, mode, frame) = lua.enter(|ctx| {
                        let executor = ctx.fetch(&executor);
                        let outcome = executor.step(ctx, &mut allowance);
                        (
                            matches!(outcome, Ok(true)),
                            outcome.err().map(|e| e.to_string()),
                            format!("{:?}", executor.mode()),
                            executor.test_frame_snapshot(),
                        )
                    });
                    trace.push((allowance.remaining(), mode, frame, error.clone()));
                    lua.gc_collect();
                    if finished || error.is_some() {
                        break;
                    }
                }
                assert!(trace.len() < 10000);
                let result = lua.execute::<i64>(&executor).map_err(|e| e.to_string());
                let stats = lua.jit_stats();
                assert_eq!(
                    stats.native_entries,
                    stats.guard_exits
                        + stats.native_interpreter_exits
                        + stats.native_budget_exits
                        + stats.native_panic_exits
                );
                assert!(
                    stats.total_dispatches
                        >= stats.native_instructions + stats.interpreted_instructions
                );
                assert_eq!(stats.compilation_failures, 0);
                (trace, result, stats)
            };
            let control = without_feedback(run);
            let candidate = with_feedback(run);
            assert_eq!(candidate.0, control.0, "scenario={scenario} fuel={fuel}");
            assert_eq!(candidate.1, control.1, "scenario={scenario} fuel={fuel}");
            if scenario == 1 {
                assert!(candidate.1.is_err());
            } else {
                assert_eq!(candidate.1.unwrap(), 192);
            }
            assert_eq!(candidate.2.total_dispatches, control.2.total_dispatches);
            assert!(candidate.2.guard_exits <= control.2.guard_exits);
        }
    }
}

#[test]
fn requested_default_policy_and_scoped_controls_are_restored() {
    let requested = feedback_default();
    assert_eq!(FEEDBACK.get(), requested);
    without_feedback(|| assert!(!FEEDBACK.get()));
    assert_eq!(FEEDBACK.get(), requested);
    with_feedback(|| assert!(FEEDBACK.get()));
    assert_eq!(FEEDBACK.get(), requested);
    assert!(std::panic::catch_unwind(|| without_feedback(|| panic!("restore policy"))).is_err());
    assert_eq!(FEEDBACK.get(), requested);
    println!("guard_feedback_default_active={requested}");
}
