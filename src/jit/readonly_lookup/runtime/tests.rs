use super::*;
use crate::jit::{
    backend,
    ir::Snapshot,
    resources::{BudgetAllocator, Ledger, MappingCounter},
    JitConfig, JitMode, JitStats,
};
use crate::{Callback, CallbackReturn, Closure, Executor, Fuel, Lua, StashedExecutor};
use std::sync::atomic::Ordering;

const SOURCE: &[u8] = b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum";

#[test]
fn scalar_hostless_route_matches_framed_public_steps_and_reference_fallback() {
    for references in [false, true] {
        let run = |scalar| {
            super::super::scalar_entry::with_enabled(scalar, || {
                let mut lua = configured(JitMode::Auto, true);
                if references {
                    lua.enter(|ctx| {
                        ctx.set_global(
                            "host_increment",
                            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                                let _: i64 = stack.consume(ctx)?;
                                assert!(ctx.globals().into_inner().try_borrow_mut(&ctx).is_ok());
                                stack.replace(ctx, crate::Table::new(&ctx));
                                Ok(CallbackReturn::Return)
                            }),
                        );
                    });
                }
                let executor = lua.enter(|ctx| {
                let text = if references { b"local n=0 for i=1,80 do local v=host_increment(i) if v then n=n+1 end end return n".as_slice() } else { SOURCE };
                let closure = Closure::load(ctx, Some("scalar-runtime-differential"), text).unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            });
                let mut trace = Vec::new();
                for turn in 0..4096 {
                    with_enabled(true, || lua.prepare_jit()).unwrap();
                    let report = lua.enter(|ctx| {
                        let mut fuel = Fuel::with([0, 1, 63, 64, 65, 4096][turn % 6]);
                        if turn % 11 == 3 {
                            fuel.interrupt();
                        }
                        let done = ctx.fetch(&executor).step(ctx, &mut fuel).unwrap();
                        (done, fuel.remaining())
                    });
                    trace.push((report, counters(lua.jit_stats())));
                    lua.gc_collect();
                    if report.0 {
                        let result = lua.enter(|ctx| {
                            ctx.fetch(&executor)
                                .take_result::<i64>(ctx)
                                .unwrap()
                                .unwrap()
                        });
                        return (trace, result, counters(lua.jit_stats()));
                    }
                }
                panic!("scalar route did not finish")
            })
        };
        let (framed, zero) = run(false);
        let (scalar, entries) = run(true);
        assert_eq!(zero, 0);
        assert_eq!(scalar, framed);
        if references {
            assert!(scalar.2[3] > entries);
        } else {
            assert!(entries > 0);
        }
        eprintln!("scalar_public_route references={references} entries={entries}");
    }
}

fn configured(mode: JitMode, replace: bool) -> Lua {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        ctx.set_global(
            "host_increment",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let value: i64 = stack.consume(ctx)?;
                assert!(ctx.globals().into_inner().try_borrow_mut(&ctx).is_ok());
                ctx.set_global("changed", value);
                if replace && value == 40 {
                    ctx.set_global(
                        "host_increment",
                        Callback::from_fn(&ctx, |ctx, _, mut stack| {
                            let value: i64 = stack.consume(ctx)?;
                            assert!(ctx.globals().into_inner().try_borrow_mut(&ctx).is_ok());
                            stack.replace(ctx, value + 2);
                            Ok(CallbackReturn::Return)
                        }),
                    );
                }
                stack.replace(ctx, value + 1);
                Ok(CallbackReturn::Return)
            }),
        );
    });
    lua
}

fn source(lua: &mut Lua) -> StashedExecutor {
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("readonly-runtime"), SOURCE).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    })
}

pub(super) fn counters(s: JitStats) -> [u64; 25] {
    [
        s.total_dispatches,
        s.interpreted_slices,
        s.interpreted_instructions,
        s.native_entries,
        s.native_pair_calls,
        s.native_pair_returns,
        s.native_pair_cache_hits,
        s.native_region_entries,
        s.native_region_fragments,
        s.code_lookups,
        s.code_leases,
        s.native_instructions,
        s.guard_exits,
        s.native_interpreter_exits,
        s.native_budget_exits,
        s.native_panic_exits,
        s.helper_calls,
        s.helper_instructions,
        s.helper_declines,
        s.native_table_reads,
        s.native_table_writes,
        s.native_upvalue_reads,
        s.native_upvalue_writes,
        s.native_allocations,
        s.hook_exits,
    ]
}

fn compare_counters(ordinary: JitStats, read: JitStats) -> u64 {
    let a = counters(ordinary);
    let b = counters(read);
    for index in (0..25).filter(|i| ![16, 17].contains(i)) {
        assert_eq!(a[index], b[index], "counter {index}")
    }
    let removed = a[16].checked_sub(b[16]).unwrap();
    assert_eq!(a[17] - b[17], removed);
    assert!(removed <= b[19]);
    removed
}

#[test]
fn selector_scopes_restore_nested_and_panicking_compilation() {
    assert!(!enabled());
    with_enabled(true, || {
        assert!(enabled());
        with_enabled(false, || assert!(!enabled()));
        assert!(enabled());
        assert!(std::panic::catch_unwind(|| with_enabled(false, || panic!("selector"))).is_err());
        assert!(enabled());
    });
    assert!(!enabled());
}

#[test]
fn public_executor_preserves_fuel_callbacks_rebinding_gc_and_actual_helper_counts() {
    for replace in [false, true] {
        for initial in [-1, 0, 1, 63, 64, 65, 65536, i32::MAX] {
            let mut states = [
                configured(JitMode::Off, replace),
                configured(JitMode::Auto, replace),
                configured(JitMode::Auto, replace),
            ];
            let executors = states.each_mut().map(source);
            let mut installed = false;
            let mut finished = false;
            for turn in 0..4096 {
                for (index, lua) in states.iter_mut().enumerate().skip(1) {
                    with_enabled(index == 2, || lua.prepare_jit()).unwrap();
                    assert_eq!(lua.jit_stats().compilation_failures, 0);
                }
                installed |= states[2].enter(|ctx| {
                    ctx.jit()
                        .0
                        .borrow()
                        .code
                        .values()
                        .any(|c| c.code.readonly_lookup.is_some())
                });
                let budget = if turn == 0 {
                    initial
                } else {
                    [-1, 0, 1, 63, 64, 65][turn % 6]
                };
                let reports: Vec<_> = states
                    .iter_mut()
                    .zip(&executors)
                    .map(|(lua, executor)| {
                        lua.enter(|ctx| {
                            let mut fuel = Fuel::with(budget);
                            if turn % 7 == 3 {
                                fuel.interrupt()
                            }
                            let executor = ctx.fetch(executor);
                            let done = executor.step(ctx, &mut fuel).unwrap();
                            (
                                done,
                                executor.mode(),
                                fuel.remaining(),
                                fuel.is_interrupted(),
                                executor.test_frame_snapshot(),
                            )
                        })
                    })
                    .collect();
                assert_eq!(reports[0], reports[1], "initial={initial} turn={turn}");
                assert_eq!(reports[1], reports[2], "initial={initial} turn={turn}");
                compare_counters(states[1].jit_stats(), states[2].jit_stats());
                assert_eq!(
                    states[0].jit_stats().total_dispatches,
                    states[2].jit_stats().total_dispatches
                );
                if reports[2].0 {
                    finished = true;
                    break;
                }
                if turn % 11 == 0 {
                    for lua in &mut states {
                        lua.gc_collect()
                    }
                }
            }
            assert!(finished && installed);
            assert!(compare_counters(states[1].jit_stats(), states[2].jit_stats()) > 0);
            assert!(states[2].jit_stats().code_bytes > states[1].jit_stats().code_bytes);
            for (lua, executor) in states.iter_mut().zip(&executors) {
                assert_eq!(
                    lua.execute::<i64>(executor).unwrap(),
                    if replace { 3360 } else { 3320 }
                );
                lua.clear_jit_cache();
                assert_eq!(lua.jit_stats().code_bytes, 0);
                assert_eq!(lua.jit_stats().code_requested_bytes, 0);
                assert_eq!(lua.jit_stats().snapshot_bytes, 0);
            }
        }
    }
}

#[test]
fn aggregate_child_admission_and_late_owners_keep_ordinary_entries_usable() {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, SOURCE).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let maps = MappingCounter::new(Ledger::new(usize::MAX));
        let ledger = Ledger::new(8 << 20);
        let work: crate::jit::work::Limits = (&JitConfig::default()).into();
        let ordinary = backend::compile_in(
            &source,
            maps.clone(),
            8 << 20,
            BudgetAllocator(ledger.clone()),
            work,
            backend::Failure::None,
        )
        .unwrap();
        let ordinary_relocations = ordinary.relocations;
        let base = (
            maps.load(Ordering::Relaxed),
            maps.requested(),
            ledger.current(),
        );
        for case in 0..7 {
            let mut limits = work;
            let mut mapping_limit = 8 << 20;
            let mut failure = backend::Failure::None;
            let bound = crate::jit::work::Expansion::admit(&source, work).unwrap();
            match case {
                0 => mapping_limit = base.0 * 2,
                1 => limits.instructions = bound.instructions * 2 + 63,
                2 => limits.blocks = bound.blocks * 2 + 2,
                3 => limits.relocations = ordinary_relocations,
                4 => failure = backend::Failure::RefuseReadonlyOwnerStorage,
                5 => failure = backend::Failure::RefuseReadonlyOwnerAllocation,
                6 => {}
                _ => unreachable!(),
            }
            let code = backend::readonly_lookup::compile_pair(
                &source,
                maps.clone(),
                mapping_limit,
                BudgetAllocator(ledger.clone()),
                limits,
                failure,
            )
            .unwrap();
            assert_eq!(code.readonly_lookup.is_some(), case == 6, "case={case}");
            if case == 6 {
                assert_eq!(
                    code.relocations,
                    ordinary_relocations + code.readonly_lookup.as_ref().unwrap().relocations
                );
                ledger.set_limit(
                    base.2 * 2 + crate::jit::owner::Shared::<backend::Code>::allocation_bytes(),
                );
                let code = code.into_shared(BudgetAllocator(ledger.clone())).unwrap();
                assert!(code.readonly_lookup.is_none());
                assert_eq!(code.relocations, ordinary_relocations);
                drop(code);
                ledger.set_limit(8 << 20);
            } else {
                assert_eq!(code.relocations, ordinary_relocations);
                drop(code);
            }
            assert_eq!(
                (
                    maps.load(Ordering::Relaxed),
                    maps.requested(),
                    ledger.current()
                ),
                base,
                "case={case}"
            );
        }
        drop(ordinary);
        assert_eq!(
            (
                maps.load(Ordering::Relaxed),
                maps.requested(),
                ledger.current()
            ),
            (0, 0, 0)
        );
    });
}

#[test]
fn callbacks_can_retire_code_change_modes_weaken_tables_and_toggle_count_one_hooks() {
    for case in 0..5 {
        for budget in [1, 17, 64, 4096] {
            let mut states = [
                configured(JitMode::Off, false),
                configured(JitMode::Auto, false),
                configured(JitMode::Auto, false),
            ];
            for lua in &mut states {
                lua.enter(|ctx| {
                    let interpreted = ctx.jit().0.borrow().config.mode == JitMode::Off;
                    ctx.set_global(
                        "host_increment",
                        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                            let value: i64 = stack.consume(ctx)?;
                            assert!(ctx.globals().into_inner().try_borrow_mut(&ctx).is_ok());
                            match case {
                                0 if value % 7 == 0 => ctx.jit().0.borrow_mut().clear(),
                                1 if !interpreted => {
                                    let mut manager = ctx.jit().0.borrow_mut();
                                    let mut config = manager.config.clone();
                                    config.mode = if value % 7 == 0 {
                                        JitMode::Off
                                    } else {
                                        JitMode::Auto
                                    };
                                    manager.configure(config);
                                }
                                2 if value == 20 => {
                                    let mt = crate::Table::new(&ctx);
                                    mt.set(ctx, "__mode", "v")?;
                                    ctx.globals().set_metatable(ctx, Some(mt));
                                }
                                3 if value == 20 => {
                                    let hook = Callback::from_fn(&ctx, |ctx, _, _| {
                                        assert!(ctx
                                            .globals()
                                            .into_inner()
                                            .try_borrow_mut(&ctx)
                                            .is_ok());
                                        Ok(CallbackReturn::Return)
                                    });
                                    ctx.set_debug_hook(hook.into(), false, 1);
                                }
                                3 if value == 40 => ctx.set_debug_hook(crate::Value::Nil, false, 0),
                                4 if value == 20 => {
                                    ctx.set_global("host_increment", crate::Value::Nil);
                                    let mt = crate::Table::new(&ctx);
                                    mt.set(
                                        ctx,
                                        "__index",
                                        Callback::from_fn(&ctx, |ctx, _, mut stack| {
                                            stack.replace(
                                                ctx,
                                                Callback::from_fn(&ctx, |ctx, _, mut stack| {
                                                    let value: i64 = stack.consume(ctx)?;
                                                    stack.replace(ctx, value + 1);
                                                    Ok(CallbackReturn::Return)
                                                }),
                                            );
                                            Ok(CallbackReturn::Return)
                                        }),
                                    )?;
                                    ctx.globals().set_metatable(ctx, Some(mt));
                                }
                                _ => {}
                            }
                            stack.replace(ctx, value + 1);
                            Ok(CallbackReturn::Return)
                        }),
                    );
                });
            }
            let anchors: Vec<_> = states
                .iter_mut()
                .map(|lua| {
                    lua.enter(|ctx| {
                        ctx.stash(ctx.get_global::<crate::Value>("host_increment").unwrap())
                    })
                })
                .collect();
            let executors = states.each_mut().map(source);
            let mut installed = false;
            let mut finished = false;
            for turn in 0..10000 {
                for (index, lua) in states.iter_mut().enumerate().skip(1) {
                    with_enabled(index == 2, || lua.prepare_jit()).unwrap();
                }
                installed |= states[2].enter(|ctx| {
                    ctx.jit()
                        .0
                        .borrow()
                        .code
                        .values()
                        .any(|c| c.code.readonly_lookup.is_some())
                });
                let reports: Vec<_> = states
                    .iter_mut()
                    .zip(&executors)
                    .map(|(lua, executor)| {
                        lua.enter(|ctx| {
                            let mut fuel = Fuel::with(budget);
                            if turn % 13 == 9 {
                                fuel.interrupt()
                            }
                            let executor = ctx.fetch(executor);
                            let done = executor.step(ctx, &mut fuel).unwrap();
                            (
                                done,
                                executor.mode(),
                                fuel.remaining(),
                                fuel.is_interrupted(),
                                executor.test_frame_snapshot(),
                            )
                        })
                    })
                    .collect();
                assert_eq!(
                    reports[0], reports[1],
                    "case={case} budget={budget} turn={turn}"
                );
                assert_eq!(
                    reports[1], reports[2],
                    "case={case} budget={budget} turn={turn}"
                );
                compare_counters(states[1].jit_stats(), states[2].jit_stats());
                if reports[2].0 {
                    finished = true;
                    break;
                }
                for lua in &mut states {
                    lua.gc_collect()
                }
            }
            assert!(finished && installed, "case={case} budget={budget}");
            assert_eq!(states[0].jit_stats().native_entries, 0);
            assert!(compare_counters(states[1].jit_stats(), states[2].jit_stats()) > 0);
            for (lua, executor) in states.iter_mut().zip(&executors) {
                assert_eq!(lua.execute::<i64>(executor).unwrap(), 3320);
                lua.clear_jit_cache();
                assert_eq!(lua.jit_stats().code_bytes, 0);
            }
            std::hint::black_box(anchors);
        }
    }
}

#[test]
fn late_runtime_cache_allocation_refusal_discards_child_and_installs_ordinary_owner() {
    let mut lua = configured(JitMode::Auto, false);
    lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().memory_failure = backend::Failure::RefuseReadonlyCacheAllocation
    });
    let executor = source(&mut lua);
    let mut done = false;
    for _ in 0..1000 {
        with_enabled(true, || lua.prepare_jit()).unwrap();
        done = lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut Fuel::with(64)).unwrap());
        if done {
            break;
        }
    }
    assert!(done);
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 3320);
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        assert_eq!(manager.code.len(), 1);
        assert!(manager
            .code
            .values()
            .all(|c| c.code.readonly_lookup.is_none()));
        assert!(manager.metadata.0.refusals() > 0);
        assert_eq!(manager.stats.compilation_failures, 0);
        assert!(manager.stats.native_instructions > 0);
    });
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
}

#[test]
fn retired_parent_lease_pins_both_images_until_its_last_release() {
    let mut lua = configured(JitMode::Auto, false);
    let executor = source(&mut lua);
    for _ in 0..100 {
        with_enabled(true, || lua.prepare_jit()).unwrap();
        if lua.jit_stats().code_bytes > 0 {
            break;
        }
        assert!(!lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut Fuel::with(1)).unwrap()));
    }
    let owner = lua.enter(|ctx| {
        ctx.jit()
            .0
            .borrow()
            .code
            .values()
            .next()
            .unwrap()
            .code
            .clone()
    });
    assert!(owner.readonly_lookup.is_some());
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 3320);
    assert!(lua.jit_stats().native_instructions > 0);
    let before = lua.jit_stats();
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, before.code_bytes);
    assert!(lua.jit_stats().code_bytes > 0);
    let child = owner.readonly_lookup.as_ref().unwrap().clone();
    drop(owner);
    assert!(lua.jit_stats().code_bytes > 0);
    assert!(lua.jit_stats().code_bytes < before.code_bytes);
    drop(child);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().code_requested_bytes, 0);
}

#[derive(Debug, thiserror::Error)]
#[error("readonly callback sentinel")]
struct Sentinel(std::sync::Arc<()>);

#[test]
fn typed_callback_errors_and_missing_or_noncallable_values_preserve_fault_frames() {
    for case in 0..3 {
        for budget in [1, 64, 4096] {
            let token = std::sync::Arc::new(());
            let mut states = [
                configured(JitMode::Off, false),
                configured(JitMode::Auto, false),
                configured(JitMode::Auto, false),
            ];
            for lua in &mut states {
                let token = token.clone();
                lua.enter(|ctx| {
                    ctx.set_global(
                        "host_increment",
                        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                            let value: i64 = stack.consume(ctx)?;
                            assert!(ctx.globals().into_inner().try_borrow_mut(&ctx).is_ok());
                            if value == 20 {
                                match case {
                                    0 => {
                                        return Err(crate::RuntimeError::new(Sentinel(
                                            token.clone(),
                                        ))
                                        .into())
                                    }
                                    1 => {
                                        ctx.set_global("host_increment", false);
                                    }
                                    2 => {
                                        ctx.set_global("host_increment", crate::Value::Nil);
                                    }
                                    _ => unreachable!(),
                                }
                            }
                            stack.replace(ctx, value + 1);
                            Ok(CallbackReturn::Return)
                        }),
                    );
                });
            }
            let executors = states.each_mut().map(source);
            let mut finished = false;
            for _ in 0..10000 {
                for (index, lua) in states.iter_mut().enumerate().skip(1) {
                    with_enabled(index == 2, || lua.prepare_jit()).unwrap();
                }
                let reports: Vec<_> = states
                    .iter_mut()
                    .zip(&executors)
                    .map(|(lua, executor)| {
                        lua.enter(|ctx| {
                            let executor = ctx.fetch(executor);
                            let mut fuel = Fuel::with(budget);
                            let done = executor.step(ctx, &mut fuel).unwrap();
                            (
                                done,
                                executor.mode(),
                                fuel.remaining(),
                                executor.test_frame_snapshot(),
                            )
                        })
                    })
                    .collect();
                assert_eq!(reports[0], reports[1], "case={case} budget={budget}");
                assert_eq!(reports[1], reports[2], "case={case} budget={budget}");
                compare_counters(states[1].jit_stats(), states[2].jit_stats());
                if reports[2].0 {
                    finished = true;
                    break;
                }
                for lua in &mut states {
                    lua.gc_collect()
                }
            }
            assert!(finished);
            assert!(compare_counters(states[1].jit_stats(), states[2].jit_stats()) > 0);
            let mut rendered = None;
            for (lua, executor) in states.iter_mut().zip(&executors) {
                let error = lua.execute::<i64>(executor).unwrap_err();
                if case == 0 {
                    assert!(std::sync::Arc::ptr_eq(
                        &error.root_cause().downcast_ref::<Sentinel>().unwrap().0,
                        &token
                    ));
                }
                if let Some(expected) = &rendered {
                    assert_eq!(&error.to_string(), expected)
                } else {
                    rendered = Some(error.to_string())
                }
                lua.clear_jit_cache();
                assert_eq!(lua.jit_stats().code_bytes, 0);
            }
        }
    }
}

#[test]
fn an_open_environment_uses_ordinary_helpers_for_installed_children() {
    let text=b"local _ENV=_ENV local function run() local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum end local value=run() return value";
    for budget in [1, 17, 64, 4096] {
        let mut states = [
            configured(JitMode::Off, false),
            configured(JitMode::Auto, false),
            configured(JitMode::Auto, false),
        ];
        let executors = states.each_mut().map(|lua| {
            lua.enter(|ctx| {
                let closure = Closure::load(ctx, Some("open-readonly-runtime"), text).unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            })
        });
        let mut installed = false;
        let mut finished = false;
        for _ in 0..10000 {
            for (index, lua) in states.iter_mut().enumerate().skip(1) {
                with_enabled(index == 2, || lua.prepare_jit()).unwrap();
            }
            installed |= states[2].enter(|ctx| {
                ctx.jit()
                    .0
                    .borrow()
                    .code
                    .values()
                    .any(|c| c.code.readonly_lookup.is_some())
            });
            let reports: Vec<_> = states
                .iter_mut()
                .zip(&executors)
                .map(|(lua, executor)| {
                    lua.enter(|ctx| {
                        let executor = ctx.fetch(executor);
                        let mut fuel = Fuel::with(budget);
                        let done = executor.step(ctx, &mut fuel).unwrap();
                        (
                            done,
                            executor.mode(),
                            fuel.remaining(),
                            executor.test_frame_snapshot(),
                        )
                    })
                })
                .collect();
            assert_eq!(reports[0], reports[1]);
            assert_eq!(reports[1], reports[2]);
            assert_eq!(
                counters(states[1].jit_stats()),
                counters(states[2].jit_stats())
            );
            if reports[2].0 {
                finished = true;
                break;
            }
            for lua in &mut states {
                lua.gc_collect()
            }
        }
        assert!(finished && installed);
        assert!(states[2].jit_stats().native_instructions > 0);
        for (lua, executor) in states.iter_mut().zip(&executors) {
            assert_eq!(lua.execute::<i64>(executor).unwrap(), 3320);
            lua.clear_jit_cache();
            assert_eq!(lua.jit_stats().code_bytes, 0);
        }
    }
}
