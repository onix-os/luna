#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{Closure, Executor, ExternError, Fuel, JitConfig, JitMode, Lua, StashedExecutor};

fn source(lua: &mut Lua, source: &[u8]) -> Result<StashedExecutor, ExternError> {
    lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("native-test"), source)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })
}

fn native() -> Lua {
    let mut lua = native_empty();
    lua.load_core();
    lua
}

fn native_empty() -> Lua {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    })
    .unwrap();
    lua
}

fn assert_exit_partition(stats: luna::JitStats) {
    assert_eq!(
        stats.native_entries,
        stats.guard_exits
            + stats.native_interpreter_exits
            + stats.native_budget_exits
            + stats.native_panic_exits
    );
}

#[test]
fn integer_leaf_activation_executes_without_upvalue_helpers() -> Result<(), ExternError> {
    let mut lua = native_empty();
    let executor = source(
        &mut lua,
        b"local sum=1 local function add(v) sum=sum+v end add(2) return sum",
    )?;
    assert_eq!(lua.prepare_jit().unwrap(), 2);
    assert_eq!(lua.execute::<i64>(&executor)?, 3);
    let stats = lua.jit_stats();
    assert_eq!(stats.native_upvalue_reads, 1);
    assert_eq!(stats.native_upvalue_writes, 1);
    assert!(stats.helper_calls < 2, "{stats:?}");
    assert_exit_partition(stats);
    Ok(())
}

#[test]
fn native_call_and_fallback_slices_preserve_step_boundaries() -> Result<(), ExternError> {
    let programs: &[&[u8]] = &[
        b"local n=0 local function f() n=n+1 return n end for i=1,120 do f() end return n",
        b"local function f(n) if n==0 then return 42 end return f(n-1) end return f(120)",
        b"local n=0 for i=1,120 do n=n+tonumber('1') end return n",
        b"local n=0 for i=1,120 do local x='1' n=n+x end return n",
        b"local t=setmetatable({}, {__index=function() return 1 end}) local n=0 for i=1,120 do n=n+t.x end return n",
        b"local n=0 for i=1,120 do n=n+1 if i%2==0 then n=n+1 end end return n",
        b"local function f(a,b,c) a=a+0 return a,b,c,nil,99 end local a,b,c,d,e=f(7) assert(a==7 and b==nil and c==nil and d==nil and e==99) local t={} a,b,c,d,e=f(7,nil,t,123) assert(a==7 and b==nil and c==t and d==nil and e==99) return 42",
        b"local function f(a,b,c) a=a+0 return a,b,c,nil,99 end local function g(...) return f(...) end local a,b,c,d,e=g(7,nil,13,123) assert(a==7 and b==nil and c==13 and d==nil and e==99) local function discard() f(7,8,9) return 41 end return discard()+1",
    ];
    for &program in programs {
        let mut reference = Lua::core();
        let mut candidate = native();
        let left = source(&mut reference, program)?;
        let right = source(&mut candidate, program)?;
        candidate.prepare_jit().unwrap();
        let step = |lua: &mut Lua, executor: &StashedExecutor, budget| {
            lua.enter(|ctx| {
                let mut fuel = Fuel::with(budget);
                let executor = ctx.fetch(executor);
                let finished = executor.step(ctx, &mut fuel).unwrap();
                (finished, executor.mode(), fuel.remaining())
            })
        };
        let mut finished = false;
        for index in 0..2000 {
            let budget = [-1, 0, 1, 63, 64, 65][index % 6];
            let expected = step(&mut reference, &left, budget);
            let actual = step(&mut candidate, &right, budget);
            assert_eq!(
                actual,
                expected,
                "{} at step {index}",
                String::from_utf8_lossy(program)
            );
            assert_eq!(
                candidate.jit_stats().total_dispatches,
                reference.jit_stats().total_dispatches
            );
            assert_exit_partition(candidate.jit_stats());
            if actual.0 {
                finished = true;
                break;
            }
            candidate.prepare_jit().unwrap();
        }
        assert!(finished, "{}", String::from_utf8_lossy(program));
        assert_eq!(
            candidate.execute::<i64>(&right)?,
            reference.execute::<i64>(&left)?
        );
        assert!(candidate.jit_stats().native_instructions > 0);
    }
    Ok(())
}

#[test]
fn consecutive_lua_frames_preserve_large_and_interrupted_step_boundaries() -> Result<(), ExternError>
{
    let programs: &[&[u8]] = &[
        b"local n=0 local function f(i) n=n+i return i end for i=1,200 do f(i) end return n",
        b"local function f(n) if n==0 then return 42 end return f(n-1) end return f(200)",
        b"local n=0 for i=1,200 do local ok=pcall(function() local a=1 return a.x end) if not ok then n=n+1 end end return n",
        b"local n=0 local mt={__close=function() n=n+1 end} local function f(i) local x <close> =setmetatable({},mt) return i end for i=1,200 do n=n+f(i) end return n",
    ];
    for &program in programs {
        for initial in [-1, 0, 1, 63, 64, 65, 65536, i32::MAX] {
            for interrupted in [false, true] {
                let mut reference = Lua::core();
                let mut candidate = native();
                let left = source(&mut reference, program)?;
                let right = source(&mut candidate, program)?;
                while candidate.prepare_jit().unwrap() != 0 {}
                let step = |lua: &mut Lua, executor: &StashedExecutor, budget, interrupt| {
                    let state = lua.enter(|ctx| {
                        let mut fuel = Fuel::with(budget);
                        if interrupt {
                            fuel.interrupt();
                        }
                        let executor = ctx.fetch(executor);
                        (
                            executor.step(ctx, &mut fuel).unwrap(),
                            executor.mode(),
                            fuel.remaining(),
                            fuel.is_interrupted(),
                        )
                    });
                    (state, lua.jit_stats().total_dispatches)
                };
                let mut finished = false;
                for index in 0..2000 {
                    let budget = if index == 0 { initial } else { 65536 };
                    let interrupt = index == 0 && interrupted;
                    let expected = step(&mut reference, &left, budget, interrupt);
                    let actual = step(&mut candidate, &right, budget, interrupt);
                    assert_eq!(
                        actual,
                        expected,
                        "{} at step {index}, fuel {initial}, interrupt {interrupted}",
                        String::from_utf8_lossy(program)
                    );
                    assert_exit_partition(candidate.jit_stats());
                    if actual.0 .0 {
                        finished = true;
                        break;
                    }
                    candidate.prepare_jit().unwrap();
                }
                assert!(finished);
                assert_eq!(
                    candidate.execute::<i64>(&right)?,
                    reference.execute::<i64>(&left)?
                );
                assert!(candidate.jit_stats().native_instructions > 0);
            }
        }
    }
    Ok(())
}

#[test]
fn native_exit_reasons_distinguish_budget_and_interpreter_handoff() -> Result<(), ExternError> {
    let mut lua = native_empty();
    let executor = source(
        &mut lua,
        b"local sum=0 for i=1,100 do sum=sum+i end return sum",
    )?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    lua.enter(|ctx| {
        assert!(!ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap());
    });
    let partial = lua.jit_stats();
    assert!(partial.native_instructions > 0);
    assert!(partial.native_budget_exits > 0);
    assert_eq!(partial.native_interpreter_exits, 0);
    assert_eq!(partial.guard_exits, 0);
    assert_eq!(partial.native_panic_exits, 0);
    assert_exit_partition(partial);
    assert_eq!(lua.execute::<i64>(&executor)?, 5050);
    let finished = lua.jit_stats();
    assert!(finished.native_interpreter_exits > 0);
    assert_eq!(finished.guard_exits, 0);
    assert_eq!(finished.native_panic_exits, 0);
    assert_exit_partition(finished);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().native_entries, finished.native_entries);
    assert_exit_partition(lua.jit_stats());
    Ok(())
}

#[test]
fn off_and_auto_dispatch_switches_preserve_slice_fuel_and_counters() -> Result<(), ExternError> {
    let mut reference = Lua::empty();
    let mut candidate = Lua::empty();
    let program = b"local sum=0 for i=1,1000 do sum=sum+i end return sum";
    let left = source(&mut reference, program)?;
    let right = source(&mut candidate, program)?;
    for mode in [
        JitMode::Off,
        JitMode::Auto,
        JitMode::Off,
        JitMode::Auto,
        JitMode::Off,
    ] {
        candidate
            .set_jit_config(JitConfig {
                mode,
                hot_threshold: 1,
                ..Default::default()
            })
            .unwrap();
        if mode == JitMode::Auto {
            assert_eq!(candidate.prepare_jit().unwrap(), 1);
        }
        let before = candidate.jit_stats();
        let step = |lua: &mut Lua, executor: &StashedExecutor| {
            lua.enter(|ctx| {
                let mut fuel = Fuel::empty();
                let executor = ctx.fetch(executor);
                (
                    executor.step(ctx, &mut fuel).unwrap(),
                    executor.mode(),
                    fuel.remaining(),
                )
            })
        };
        assert_eq!(step(&mut reference, &left), step(&mut candidate, &right));
        let after = candidate.jit_stats();
        assert_eq!(
            after.total_dispatches,
            reference.jit_stats().total_dispatches
        );
        if mode == JitMode::Off {
            assert_eq!(after.native_entries, before.native_entries);
            assert_eq!(after.native_instructions, before.native_instructions);
            assert_eq!(after.code_lookups, before.code_lookups);
            assert_eq!(after.compilation_requests, before.compilation_requests);
            assert_eq!(after.guard_exits, before.guard_exits);
            assert_eq!(
                after.native_interpreter_exits,
                before.native_interpreter_exits
            );
            assert_eq!(after.native_budget_exits, before.native_budget_exits);
            assert_eq!(after.native_panic_exits, before.native_panic_exits);
            assert_eq!(
                after.interpreted_instructions - before.interpreted_instructions,
                64
            );
            assert_eq!(after.interpreted_slices - before.interpreted_slices, 1);
        } else {
            assert!(after.native_entries > before.native_entries);
            assert!(after.native_instructions > before.native_instructions);
        }
        assert_exit_partition(after);
        reference.gc_collect();
        candidate.gc_collect();
    }
    assert_eq!(reference.execute::<i64>(&left)?, 500500);
    assert_eq!(candidate.execute::<i64>(&right)?, 500500);
    assert_eq!(candidate.jit_stats().code_bytes, 0);
    Ok(())
}

#[test]
fn scalar_and_mixed_tier_programs_really_execute_native_instructions() -> Result<(), ExternError> {
    let cases: &[(&[u8], i64)] = &[
        (b"local s=0 for i=1,200 do s=s+i end return s", 20100),
        (b"local s=0.0 for i=1,200 do s=s+0.5 end return s==100.0 and 100 or -1", 100),
        (b"local s=0 for i=10,1,-1 do s=s+i end return s", 55),
        (b"local s=0 for i=1.5,5.5,0.5 do s=s+1 end return s", 9),
        (b"local x=9223372036854775807 local y=x+1 return y==-9223372036854775807-1 and 42 or 0", 42),
        (b"local t={} for i=1,200 do t[i]=i end return t[200]", 200),
        (b"local n=0 local function f() n=n+1 end for i=1,100 do f() end return n", 100),
        (b"local t=setmetatable({n=0},{__add=function(a,b) a.n=a.n+b return a end}) for i=1,10 do t=t+i end return t.n", 55),
        (b"local t={} local alias=t local a=nil local b=false local c=true return alias==t and not b and c and 42 or 0", 42),
        (b"local n=1 local x='20' return x+21+n", 42),
    ];
    for &(program, expected) in cases {
        let mut lua = native();
        let executor = source(&mut lua, program)?;
        assert!(
            lua.prepare_jit().unwrap() > 0,
            "{}",
            String::from_utf8_lossy(program)
        );
        assert_eq!(
            lua.execute::<i64>(&executor)?,
            expected,
            "{}",
            String::from_utf8_lossy(program)
        );
        assert!(
            lua.jit_stats().native_instructions > 0,
            "{}",
            String::from_utf8_lossy(program)
        );
        assert!(lua.jit_stats().code_bytes > 0);
    }
    Ok(())
}

#[test]
fn native_slices_preserve_reference_fuel_even_when_interrupted() -> Result<(), ExternError> {
    for initial in [-1, 0, 1, 63, 64, 1000] {
        for interrupted in [false, true] {
            let mut interpreted = Lua::core();
            let mut compiled = native();
            let left = source(&mut interpreted, b"while true do end")?;
            let right = source(&mut compiled, b"while true do end")?;
            compiled.prepare_jit().unwrap();
            let step = |lua: &mut Lua, executor: &StashedExecutor| {
                lua.enter(|ctx| {
                    let mut fuel = Fuel::with(initial);
                    if interrupted {
                        fuel.interrupt();
                    }
                    let executor = ctx.fetch(executor);
                    let finished = executor.step(ctx, &mut fuel).unwrap();
                    (
                        finished,
                        executor.mode(),
                        fuel.remaining(),
                        fuel.is_interrupted(),
                    )
                })
            };
            assert_eq!(step(&mut interpreted, &left), step(&mut compiled, &right));
            assert!(compiled.jit_stats().native_instructions >= 64);
        }
    }
    Ok(())
}

#[test]
fn native_guard_bailout_does_not_repeat_effects_or_charge_extra_fuel() -> Result<(), ExternError> {
    let script = b"local n=0 local t=setmetatable({},{__add=function(a,b) n=n+1 return a end}) for i=1,100 do t=t+i end return n";
    let mut interpreted = Lua::core();
    let mut compiled = native();
    let left = source(&mut interpreted, script)?;
    let right = source(&mut compiled, script)?;
    compiled.prepare_jit().unwrap();
    for index in 0..1000 {
        let step = |lua: &mut Lua, executor: &StashedExecutor| {
            lua.enter(|ctx| {
                let mut fuel = Fuel::with(if index % 2 == 0 { 0 } else { 1 });
                let executor = ctx.fetch(executor);
                (
                    executor.step(ctx, &mut fuel).unwrap(),
                    executor.mode(),
                    fuel.remaining(),
                )
            })
        };
        let a = step(&mut interpreted, &left);
        let b = step(&mut compiled, &right);
        assert_eq!(a, b, "slice {index}");
        assert_eq!(
            compiled.jit_stats().total_dispatches,
            interpreted.jit_stats().total_dispatches,
            "dispatches at slice {index}"
        );
        compiled.gc_collect();
        if a.0 {
            break;
        }
        assert!(index < 999, "execution did not finish");
    }
    assert_eq!(compiled.execute::<i64>(&right)?, 100);
    assert_eq!(interpreted.execute::<i64>(&left)?, 100);
    assert!(compiled.jit_stats().guard_exits > 0);
    assert_exit_partition(compiled.jit_stats());
    Ok(())
}

#[test]
fn manual_steps_only_queue_compilation() -> Result<(), ExternError> {
    let mut lua = native();
    let executor = source(&mut lua, b"while true do end")?;
    lua.enter(|ctx| {
        ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap();
    });
    let queued = lua.jit_stats();
    assert!(queued.queued_requests > 0);
    assert_eq!(queued.installed_regions, 0);
    assert_eq!(queued.native_entries, 0);
    assert_eq!(lua.service_jit().unwrap(), 1);
    lua.enter(|ctx| {
        ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap();
    });
    assert!(lua.jit_stats().native_instructions >= 64);
    Ok(())
}

#[test]
fn callback_interrupt_stops_after_native_work_and_resumes_with_fresh_mutations(
) -> Result<(), ExternError> {
    use std::{cell::RefCell, rc::Rc};

    use luna::{Callback, CallbackReturn, ExecutorMode, Table};

    struct Case {
        lua: Lua,
        executor: StashedExecutor,
        events: Rc<RefCell<Vec<u8>>>,
    }

    fn case(native: bool) -> Result<Case, ExternError> {
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode: if native { JitMode::Auto } else { JitMode::Off },
            hot_threshold: u32::MAX,
            ..Default::default()
        })
        .unwrap();
        let events = Rc::new(RefCell::new(Vec::new()));
        let callback_events = events.clone();
        lua.enter(|ctx| {
            let target = Table::new(&ctx);
            target.set(ctx, "value", 0).unwrap();
            target.set(ctx, "after", 0).unwrap();
            ctx.set_global("target", target);
            ctx.set_global(
                "interrupt",
                Callback::from_fn(&ctx, move |ctx, mut exec, mut stack| {
                    let previous: Table = stack.consume(ctx)?;
                    assert_eq!(previous.get::<_, i64>(ctx, "value")?, 100);
                    callback_events.borrow_mut().push(1);
                    let replacement = Table::new(&ctx);
                    replacement.set(ctx, "value", 1000)?;
                    replacement.set(ctx, "after", 0)?;
                    let meta = Table::new(&ctx);
                    let events = callback_events.clone();
                    meta.set(
                        ctx,
                        "__index",
                        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                            let (_, key): (Table, String) = stack.consume(ctx)?;
                            assert_eq!(key, "missing");
                            events.borrow_mut().push(2);
                            stack.replace(ctx, 7);
                            Ok(CallbackReturn::Return)
                        }),
                    )?;
                    replacement.set_metatable(ctx, Some(meta));
                    ctx.set_global("target", replacement);
                    exec.fuel().interrupt();
                    stack.replace(ctx, ());
                    Ok(CallbackReturn::Return)
                }),
            );
        });
        let executor = source(
            &mut lua,
            b"for i=1,100 do target.value=i end interrupt(target) local answer=target.value+target.missing target.after=answer return answer",
        )?;
        if native {
            assert_eq!(lua.prepare_jit().unwrap(), 1);
        }
        Ok(Case {
            lua,
            executor,
            events,
        })
    }

    fn step(case: &mut Case, fuel: &mut Fuel) -> (bool, ExecutorMode, i32, bool) {
        case.lua.enter(|ctx| {
            let executor = ctx.fetch(&case.executor);
            (
                executor.step(ctx, fuel).unwrap(),
                executor.mode(),
                fuel.remaining(),
                fuel.is_interrupted(),
            )
        })
    }

    fn values(case: &mut Case) -> (i64, i64) {
        case.lua.enter(|ctx| {
            let target: Table = ctx.get_global("target").unwrap();
            (
                target.get(ctx, "value").unwrap(),
                target.get(ctx, "after").unwrap(),
            )
        })
    }

    for budget in [-1, 0, 1, 64, 65536] {
        let mut reference = case(false)?;
        let mut candidate = case(true)?;
        let mut fuel = [Fuel::with(budget), Fuel::with(budget)];
        let mut interrupted = false;
        let mut finished = false;
        for _ in 0..1000 {
            let expected = step(&mut reference, &mut fuel[0]);
            let actual = step(&mut candidate, &mut fuel[1]);
            assert_eq!(actual, expected, "fuel={budget}");
            assert_eq!(values(&mut candidate), values(&mut reference));
            assert_eq!(*candidate.events.borrow(), *reference.events.borrow());
            if actual.3 {
                assert!(!interrupted);
                interrupted = true;
                assert!(!actual.0);
                assert_eq!(actual.1, ExecutorMode::Normal);
                assert_eq!(values(&mut candidate), (1000, 0));
                assert_eq!(candidate.events.borrow().as_slice(), &[1]);
                assert_eq!(candidate.lua.jit_stats().native_table_writes, 100);
            }
            reference.lua.gc_collect();
            candidate.lua.gc_collect();
            if actual.0 {
                finished = true;
                break;
            }
            for fuel in &mut fuel {
                fuel.refill(budget, budget);
                assert!(!fuel.is_interrupted());
            }
        }
        assert!(finished && interrupted);
        for case in [&mut reference, &mut candidate] {
            assert_eq!(case.events.borrow().as_slice(), &[1, 2]);
            assert_eq!(values(case), (1000, 1007));
            assert_eq!(case.lua.execute::<i64>(&case.executor)?, 1007);
        }
        assert_eq!(reference.lua.jit_stats().native_entries, 0);
        assert_eq!(candidate.lua.jit_stats().native_table_writes, 101);
        assert!(candidate.lua.jit_stats().helper_declines > 0);
    }
    Ok(())
}

#[test]
fn one_vm_slice_reuses_its_lease_across_interpreted_fragments() -> Result<(), ExternError> {
    let mut lua = native_empty();
    let executor = source(
        &mut lua,
        b"local sum=0 for i=1,100 do local t={i,i+1} sum=sum+t[1] end return sum",
    )?;
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    lua.enter(|ctx| {
        ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap();
    });
    let stats = lua.jit_stats();
    assert_eq!(stats.code_lookups, 1);
    assert_eq!(stats.code_leases, 1);
    assert!(stats.native_entries > stats.code_leases);
    assert!(stats.interpreted_instructions > 0);
    assert!(stats.native_allocations > 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 5050);
    Ok(())
}

#[test]
fn fixed_helper_symbols_preserve_environment_and_reference_operands() -> Result<(), ExternError> {
    let mut lua = native_empty();
    let executor = source(&mut lua, b"answer=40 local n=answer local text='result' local alias=text local t={} t[alias]=n+2 local f=function(v) n=v return n end local result=f(t[text]) return result")?;
    assert_eq!(lua.prepare_jit().unwrap(), 2);
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    let stats = lua.jit_stats();
    assert!(stats.native_table_reads >= 2);
    assert!(stats.native_table_writes >= 2);
    assert!(stats.native_upvalue_reads > 0);
    assert!(stats.native_upvalue_writes > 0);
    assert!(stats.native_allocations > 0);
    Ok(())
}

#[test]
fn scratch_tiers_preserve_reference_results_at_every_capacity_boundary() -> Result<(), ExternError>
{
    for registers in [
        1u16, 2, 3, 4, 5, 6, 7, 8, 9, 16, 17, 32, 33, 64, 65, 128, 129, 255, 256,
    ] {
        let names: Vec<_> = (0..registers - 1)
            .map(|index| format!("r{index}"))
            .collect();
        let script = if names.is_empty() {
            "return 'anchored'".to_owned()
        } else {
            format!("local {} return 'anchored'", names.join(","))
        };
        for enabled in [false, true] {
            let mut lua = if enabled {
                native_empty()
            } else {
                Lua::empty()
            };
            let executor = lua.try_enter(|ctx| {
                let closure = Closure::load(ctx, Some("scratch-tier"), script.as_bytes())?;
                assert_eq!(closure.prototype().stack_size, registers);
                Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
            })?;
            if enabled {
                assert_eq!(lua.prepare_jit().unwrap(), 1);
            }
            assert_eq!(lua.execute::<String>(&executor)?, "anchored");
            lua.gc_collect();
            if enabled {
                assert!(lua.jit_stats().native_instructions >= if registers == 1 { 1 } else { 2 });
                assert!(lua.jit_stats().helper_instructions >= 1);
            }
        }
    }
    Ok(())
}

#[test]
fn code_quota_failure_reclaims_memory_and_keeps_interpreter_working() -> Result<(), ExternError> {
    let mut lua = native();
    let mut config = lua.jit_config();
    config.max_code_bytes = 1;
    lua.set_jit_config(config).unwrap();
    let executor = source(&mut lua, b"return 40+2")?;
    assert!(matches!(
        lua.prepare_jit(),
        Err(luna::JitError::ResourceLimit("native image size"))
    ));
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    Ok(())
}

#[test]
fn disabling_and_clearing_code_preserve_objects_and_reference_execution() -> Result<(), ExternError>
{
    let mut lua = native();
    let executor = source(&mut lua, b"local s=0 for i=1,1000 do s=s+i end return s")?;
    lua.prepare_jit().unwrap();
    lua.enter(|ctx| {
        ctx.fetch(&executor).step(ctx, &mut Fuel::empty()).unwrap();
    });
    let native_count = lua.jit_stats().native_instructions;
    assert!(native_count > 0);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    let mut config = lua.jit_config();
    config.mode = JitMode::Off;
    lua.set_jit_config(config).unwrap();
    lua.gc_collect();
    assert_eq!(lua.execute::<i64>(&executor)?, 500500);
    assert_eq!(lua.jit_stats().native_instructions, native_count);
    Ok(())
}

#[test]
fn binary_and_manually_created_prototypes_are_not_registered() -> Result<(), ExternError> {
    let mut lua = native_empty();
    let executor = lua.try_enter(|ctx| {
        let proto = luna::FunctionPrototype::compile(
            ctx,
            "manual",
            b"local s=0 for i=1,100 do s=s+i end return s",
        )?;
        let closure = Closure::new(&ctx, proto, Some(ctx.globals())).unwrap();
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.prepare_jit().unwrap(), 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 5050);
    assert_eq!(lua.jit_stats().native_entries, 0);
    let bytes = lua.try_enter(|ctx| {
        let proto = luna::FunctionPrototype::compile(ctx, "binary", b"local x=40 return x+2")?;
        Ok(luna::dump::dump(&proto, false))
    })?;
    let mut second = native_empty();
    let executor = source(&mut second, &bytes)?;
    assert_eq!(second.prepare_jit().unwrap(), 0);
    assert_eq!(second.execute::<i64>(&executor)?, 42);
    assert_eq!(second.jit_stats().native_entries, 0);
    Ok(())
}

#[test]
fn collected_source_prototypes_retire_code_and_identity() -> Result<(), ExternError> {
    let mut lua = native_empty();
    let executor = source(&mut lua, b"local x=40 return x+2")?;
    lua.prepare_jit().unwrap();
    assert!(lua.jit_stats().registered_prototypes > 0);
    assert!(lua.jit_stats().code_bytes > 0);
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    drop(executor);
    for _ in 0..3 {
        lua.gc_collect();
    }
    assert_eq!(lua.service_jit().unwrap(), 0);
    assert_eq!(lua.jit_stats().registered_prototypes, 0);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    let replacement = source(&mut lua, b"local x=100 return x+2")?;
    lua.prepare_jit().unwrap();
    assert_eq!(lua.execute::<i64>(&replacement)?, 102);
    Ok(())
}

#[test]
fn mixed_numeric_comparisons_have_exact_results_without_guard_exits() -> Result<(), ExternError> {
    type Answers = (bool, bool, bool, bool, bool, bool);
    let cases: &[(i64, f64, Answers)] = &[
        (-1, -1.5, (false, false, false, false, true, true)),
        (0, -0.5, (false, false, false, false, true, true)),
        (-2, -1.5, (false, true, true, false, false, false)),
        (1, 1.5, (false, true, true, false, false, false)),
        (2, 1.5, (false, false, false, false, true, true)),
        (-1, -1.0, (true, false, true, true, false, true)),
        (0, -0.0, (true, false, true, true, false, true)),
        (
            0,
            f64::from_bits(1),
            (false, true, true, false, false, false),
        ),
        (
            0,
            -f64::from_bits(1),
            (false, false, false, false, true, true),
        ),
        (
            9_007_199_254_740_993,
            9_007_199_254_740_992.0,
            (false, false, false, false, true, true),
        ),
        (
            -9_007_199_254_740_993,
            -9_007_199_254_740_992.0,
            (false, true, true, false, false, false),
        ),
        (
            i64::MAX,
            9_223_372_036_854_775_808.0,
            (false, true, true, false, false, false),
        ),
        (
            i64::MIN,
            -9_223_372_036_854_775_808.0,
            (true, false, true, true, false, true),
        ),
        (
            i64::MIN + 1,
            -9_223_372_036_854_775_808.0,
            (false, false, false, false, true, true),
        ),
        (
            i64::MAX,
            f64::INFINITY,
            (false, true, true, false, false, false),
        ),
        (
            i64::MIN,
            f64::NEG_INFINITY,
            (false, false, false, false, true, true),
        ),
        (-1, f64::NAN, (false, false, false, false, false, false)),
        (0, f64::NAN, (false, false, false, false, false, false)),
        (1, f64::NAN, (false, false, false, false, false, false)),
    ];
    for &(integer, float, expected) in cases {
        let mut interpreted = Lua::empty();
        let mut compiled = native_empty();
        let start = |lua: &mut Lua| {
            lua.try_enter(|ctx| {
                let closure = Closure::load(
                    ctx,
                    Some("mixed-numeric"),
                    &b"local a,b=... return a==b,a<b,a<=b,b==a,b<a,b<=a"[..],
                )?;
                Ok(ctx.stash(Executor::start(ctx, closure.into(), (integer, float))))
            })
        };
        let left = start(&mut interpreted)?;
        let right = start(&mut compiled)?;
        compiled.prepare_jit().unwrap();
        for index in 0..100 {
            let step = |lua: &mut Lua, executor: &StashedExecutor| {
                lua.enter(|ctx| {
                    let executor = ctx.fetch(executor);
                    let mut fuel = Fuel::with(index % 2);
                    (
                        executor.step(ctx, &mut fuel).unwrap(),
                        executor.mode(),
                        fuel.remaining(),
                    )
                })
            };
            let a = step(&mut interpreted, &left);
            let b = step(&mut compiled, &right);
            assert_eq!(a, b, "integer={integer} float={float:?} slice={index}");
            interpreted.gc_collect();
            compiled.gc_collect();
            if a.0 {
                break;
            }
            assert!(index < 99);
        }
        assert_eq!(interpreted.execute::<Answers>(&left)?, expected);
        assert_eq!(compiled.execute::<Answers>(&right)?, expected);
        let stats = compiled.jit_stats();
        assert!(stats.native_instructions >= 6);
        assert_eq!(stats.guard_exits, 0, "integer={integer} float={float:?}");
    }
    Ok(())
}

#[test]
fn prepared_code_obeys_debug_hooks_and_mixed_numeric_comparison() -> Result<(), ExternError> {
    let mut lua = native();
    lua.load_debug();
    let executor = source(&mut lua, b"local hits=0 debug.sethook(function() hits=hits+1 end,'',10) local s=0 for i=1,200 do s=s+i end debug.sethook() assert(hits>0) local x=9007199254740993 assert(x>9007199254740992.0) return s")?;
    lua.prepare_jit().unwrap();
    assert_eq!(lua.execute::<i64>(&executor)?, 20100);
    assert!(lua.jit_stats().hook_exits > 0);
    assert!(lua.jit_stats().native_instructions > 0);
    Ok(())
}
