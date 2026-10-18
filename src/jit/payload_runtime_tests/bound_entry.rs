use super::*;

#[path = "../../../examples/jit_support/workloads.rs"]
#[allow(dead_code)]
mod workloads;

mod timing;

fn configured(mode: JitMode, pair: bool, references: bool) -> Lua {
    let mut lua = super::preferred_entry::configured(mode, false, references);
    lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().memory_failure = if pair {
            backend::Failure::PreferBoundPair
        } else {
            backend::Failure::None
        }
    });
    lua
}

#[test]
fn pair_preserves_callback_fuel_mutation_gc_and_ordinary_native_coverage() {
    for (script, expected, references) in [
        ("local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum", 3320, false),
        ("local sum=0 for i=1,80 do local t=host_increment(i); sum=sum+t.n+target.n end return sum", 9800, true),
    ] {
        for initial in [-1, 0, 1, 63, 64, 65, 65536, i32::MAX] {
            let mut states = [configured(JitMode::Off, false, references),
                configured(JitMode::Auto, false, references), configured(JitMode::Auto, true, references)];
            let executors = states.each_mut().map(|lua| source(lua, script));
            let mut finished = false;
            let mut installed = false;
            for turn in 0..4096 {
                for lua in &mut states[1..] {
                    lua.prepare_jit().unwrap();
                    assert_eq!(lua.jit_stats().compilation_failures, 0);
                }
                installed |= states[2].enter(|ctx| {
                    let manager = ctx.jit().0.borrow();
                    manager.code.values().any(|cached| cached.code.bound_entry.is_some())
                });
                let budget = if turn == 0 { initial } else { [-1, 0, 1, 63, 64, 65][turn % 6] };
                let reports: Vec<_> = states.iter_mut().zip(&executors).map(|(lua, executor)| lua.enter(|ctx| {
                    let mut fuel = Fuel::with(budget);
                    if turn % 7 == 3 { fuel.interrupt(); }
                    let executor = ctx.fetch(executor);
                    let finished = executor.step(ctx, &mut fuel).unwrap();
                    (finished, executor.mode(), fuel.remaining(), fuel.is_interrupted())
                })).collect();
                assert_eq!(reports[0], reports[1], "initial={initial} turn={turn}");
                assert_eq!(reports[1], reports[2], "initial={initial} turn={turn}");
                assert_eq!(counters(states[1].jit_stats()), counters(states[2].jit_stats()));
                assert_eq!(states[0].jit_stats().total_dispatches, states[2].jit_stats().total_dispatches);
                if reports[2].0 { finished = true; break; }
                if turn % 11 == 0 { for lua in &mut states { lua.gc_collect(); } }
            }
            assert!(finished && installed);
            for (lua, executor) in states.iter_mut().zip(&executors) {
                assert_eq!(lua.execute::<i64>(executor).unwrap(), expected);
            }
            assert!(states[2].jit_stats().native_instructions > 0);
            assert_eq!(states[2].jit_stats().installed_regions, 1);
            for lua in &mut states {
                lua.clear_jit_cache();
                assert_eq!(lua.jit_stats().code_bytes, 0);
                assert_eq!(lua.jit_stats().snapshot_bytes, 0);
            }
        }
    }
}

#[inline(never)]
fn profile_execute(lua: &mut Lua, executor: &StashedExecutor) -> i64 {
    std::hint::black_box(lua.execute::<i64>(executor).unwrap())
}

fn run_callback(pair: bool, profile: bool) -> [u64; 25] {
    let workload = workloads::WORKLOADS
        .iter()
        .find(|w| w.name == "rust_callbacks")
        .unwrap();
    let mut lua = configured(JitMode::Auto, pair, false);
    let closure = lua
        .enter(|ctx| ctx.stash(Closure::load(ctx, Some(workload.name), workload.source).unwrap()));
    for _ in 0..2 {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), workload.expected);
    }
    lua.enter(|ctx| {
        let manager = ctx.jit().0.borrow();
        assert_eq!(manager.code.len(), 1);
        assert_eq!(
            manager
                .code
                .values()
                .next()
                .unwrap()
                .code
                .bound_entry
                .is_some(),
            pair
        );
    });
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
    let before = lua.jit_stats();
    let actual = if profile {
        profile_execute(&mut lua, &executor)
    } else {
        lua.execute::<i64>(&executor).unwrap()
    };
    let after = lua.jit_stats();
    assert_eq!(actual, workload.expected);
    assert_eq!(after.compilation_failures, 0);
    assert_eq!(after.installed_regions, 1);
    assert_eq!(after.compilation_requests, before.compilation_requests);
    assert!(after.native_instructions > before.native_instructions);
    let delta = std::array::from_fn(|i| counters(after)[i] - counters(before)[i]);
    if profile {
        let directory =
            std::path::PathBuf::from(std::env::var_os("LUNA_JIT_DIAGNOSTIC_DIR").unwrap());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("workload.lua"), workload.source).unwrap();
        lua.enter(|ctx| {
            let manager = ctx.jit().0.borrow();
            let code = &manager.code.values().next().unwrap().code;
            let metadata = code.dump_finalized(&directory.join("ordinary.bin"));
            std::fs::write(directory.join("ordinary.metadata"), metadata).unwrap();
            if let Some(binding) = &code.bound_entry {
                let metadata = binding.code.dump_finalized(&directory.join("budget.bin"));
                std::fs::write(directory.join("budget.metadata"), metadata).unwrap();
            }
        });
        println!("case=rust_callbacks pair={pair} validated=true result={actual} counters={delta:?} code_bytes={} code_requested_bytes={} metadata_bytes={} snapshot_peak_bytes={}",
            after.code_bytes, after.code_requested_bytes, after.metadata_bytes, after.snapshot_peak_bytes);
    }
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    delta
}

#[test]
fn frozen_callback_preserves_native_and_helper_counters() {
    assert_eq!(run_callback(false, false), run_callback(true, false));
}

#[test]
fn callback_cache_clear_and_mode_switch_preserve_resumption() {
    for switch_mode in [false, true] {
        let mut states = [
            configured(JitMode::Auto, false, false),
            configured(JitMode::Auto, true, false),
        ];
        for lua in &mut states {
            lua.enter(|ctx| {
                let callback = crate::Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                    let value: i64 = stack.consume(ctx)?;
                    let mut manager = ctx.jit().0.borrow_mut();
                    if switch_mode {
                        let mut config = manager.config.clone();
                        config.mode = if value % 7 == 0 {
                            JitMode::Off
                        } else {
                            JitMode::Auto
                        };
                        manager.configure(config);
                    } else if value % 7 == 0 {
                        manager.clear();
                    }
                    drop(manager);
                    stack.replace(ctx, value + 1);
                    Ok(crate::CallbackReturn::Return)
                });
                ctx.set_global("host_increment", callback);
            });
        }
        let executors = states.each_mut().map(|lua| {
            source(
                lua,
                "local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum",
            )
        });
        let mut finished = false;
        let mut installed = false;
        for _ in 0..4096 {
            for lua in &mut states {
                lua.prepare_jit().unwrap();
            }
            installed |= states[1].enter(|ctx| {
                ctx.jit()
                    .0
                    .borrow()
                    .code
                    .values()
                    .any(|cached| cached.code.bound_entry.is_some())
            });
            let reports: Vec<_> = states
                .iter_mut()
                .zip(&executors)
                .map(|(lua, executor)| {
                    lua.enter(|ctx| {
                        let mut fuel = Fuel::with(16);
                        let executor = ctx.fetch(executor);
                        (
                            executor.step(ctx, &mut fuel).unwrap(),
                            executor.mode(),
                            fuel.remaining(),
                        )
                    })
                })
                .collect();
            assert_eq!(reports[0], reports[1]);
            assert_eq!(
                counters(states[0].jit_stats()),
                counters(states[1].jit_stats())
            );
            if reports[1].0 {
                finished = true;
                break;
            }
        }
        assert!(finished && installed);
        for (lua, executor) in states.iter_mut().zip(&executors) {
            assert_eq!(lua.execute::<i64>(executor).unwrap(), 3320);
        }
        assert_eq!(
            counters(states[0].jit_stats()),
            counters(states[1].jit_stats())
        );
        assert!(states[1].jit_stats().native_instructions > 0);
        for lua in &mut states {
            lua.gc_collect();
            lua.clear_jit_cache();
            assert_eq!(lua.jit_stats().code_bytes, 0);
            assert_eq!(lua.jit_stats().snapshot_bytes, 0);
        }
    }
}

#[test]
#[ignore]
fn profile_bound_pair_callback() {
    let pair = match std::env::var("LUNA_BOUND_PAIR").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("invalid bound pair selection"),
    };
    run_callback(pair, true);
}
