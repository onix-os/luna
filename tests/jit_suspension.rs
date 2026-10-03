#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

#[path = "../examples/jit_support/suspension.rs"]
mod suspension;

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExecutorMode, ExternError, Fuel, Function,
    JitConfig, JitMode, Lua, StashedExecutor, Variadic,
};
use std::{cell::Cell, rc::Rc};

type StackSamples = Rc<Cell<(usize, usize, usize)>>;

fn tail_state(
    native: bool,
    depth: i64,
    tail: bool,
) -> Result<(Lua, StashedExecutor, StackSamples), ExternError> {
    recursion_state(native, depth, tail, 8)
}

fn recursion_state(
    native: bool,
    depth: i64,
    tail: bool,
    max_depth: usize,
) -> Result<(Lua, StashedExecutor, StackSamples), ExternError> {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: if native { JitMode::Auto } else { JitMode::Off },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let samples = Rc::new(Cell::new((usize::MAX, 0, 0)));
    let script = format!(
        r#"
        local n,total=...
        if n%128==0 then probe() end
        if n==0 then return total,nil,42,nil end
        {}
        "#,
        if tail {
            "return recurse(n-1,total+n)"
        } else {
            "local result=recurse(n-1,total+n) return result"
        }
    );
    let executor = lua.try_enter(|ctx| {
        ctx.set_max_call_depth(max_depth);
        let samples = samples.clone();
        ctx.set_global(
            "probe",
            Callback::from_fn(&ctx, move |_, _, mut stack| {
                let marker = std::hint::black_box(0u8);
                let address = std::hint::black_box(&marker) as *const u8 as usize;
                let (low, high, count) = samples.get();
                samples.set((low.min(address), high.max(address), count + 1));
                stack.clear();
                Ok(CallbackReturn::Return)
            }),
        );
        let closure = Closure::load(ctx, Some("tail-depth"), script.as_bytes())?;
        ctx.set_global("recurse", closure);
        Ok(ctx.stash(Executor::start(ctx, closure.into(), (depth, 0i64))))
    })?;
    assert_eq!(lua.prepare_jit().unwrap(), usize::from(native));
    Ok((lua, executor, samples))
}

#[test]
fn deep_native_tail_calls_keep_frames_memory_and_host_stack_bounded() -> Result<(), ExternError> {
    for fuel in [1, 64, 65536] {
        let mut shallow_peaks = [0; 2];
        for depth in [128, 16384] {
            let (mut reference, left, left_stack) = tail_state(false, depth, true)?;
            let (mut candidate, right, right_stack) = tail_state(true, depth, true)?;
            let mut peaks = [reference.total_memory(), candidate.total_memory()];
            let mut finished = false;
            for _ in 0..100_000 {
                let before = candidate.jit_stats();
                let expected = call_step(&mut reference, &left, fuel);
                let actual = call_step(&mut candidate, &right, fuel);
                assert_eq!(actual, expected, "depth={depth} fuel={fuel}");
                let after = candidate.jit_stats();
                let work = after.native_instructions - before.native_instructions
                    + after.interpreted_instructions
                    - before.interpreted_instructions;
                assert!(work <= fuel as u64 + 63);
                peaks[0] = peaks[0].max(reference.total_memory());
                peaks[1] = peaks[1].max(candidate.total_memory());
                reference.gc_collect();
                candidate.gc_collect();
                if actual.0 {
                    finished = true;
                    break;
                }
            }
            assert!(finished, "depth={depth} fuel={fuel}");
            let expected = vec![Some(depth * (depth + 1) / 2), None, Some(42), None];
            assert_eq!(call_result(&mut reference, &left)?, expected);
            assert_eq!(call_result(&mut candidate, &right)?, expected);
            assert!(candidate.jit_stats().native_instructions > depth as u64);
            assert_eq!(candidate.jit_stats().installed_regions, 1);
            assert_eq!(candidate.jit_stats().queued_requests, 0);
            assert_eq!(reference.jit_stats().native_instructions, 0);
            for (tier, samples) in [left_stack, right_stack].into_iter().enumerate() {
                let (low, high, count) = samples.get();
                assert_eq!(count, depth as usize / 128 + 1);
                assert!(high - low <= 32768, "host stack span {}", high - low);
                eprintln!(
                    "tail depth={depth} fuel={fuel} native={} samples={count} callback_stack_span={} arena_peak={}",
                    tier == 1,
                    high - low,
                    peaks[tier]
                );
            }
            if depth == 128 {
                shallow_peaks = peaks;
            } else {
                for (peak, shallow) in peaks.into_iter().zip(shallow_peaks) {
                    assert!(
                        peak <= shallow + 4096,
                        "arena peak {peak}, shallow {shallow}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn deep_non_tail_recursion_keeps_host_stack_bounded_and_slices_equivalent(
) -> Result<(), ExternError> {
    for fuel in [1, 64, 65536] {
        for depth in [128, 4096] {
            let (mut reference, left, left_stack) = recursion_state(false, depth, false, 8192)?;
            let (mut candidate, right, right_stack) = recursion_state(true, depth, false, 8192)?;
            let mut finished = false;
            for _ in 0..100_000 {
                let before = candidate.jit_stats();
                let expected = call_step(&mut reference, &left, fuel);
                let actual = call_step(&mut candidate, &right, fuel);
                assert_eq!(actual, expected, "depth={depth} fuel={fuel}");
                let after = candidate.jit_stats();
                let work = after.native_instructions - before.native_instructions
                    + after.interpreted_instructions
                    - before.interpreted_instructions;
                assert!(work <= fuel as u64 + 63);
                reference.gc_collect();
                candidate.gc_collect();
                if actual.0 {
                    finished = true;
                    break;
                }
            }
            assert!(finished, "depth={depth} fuel={fuel}");
            let expected = vec![Some(depth * (depth + 1) / 2)];
            assert_eq!(call_result(&mut reference, &left)?, expected);
            assert_eq!(call_result(&mut candidate, &right)?, expected);
            assert!(candidate.jit_stats().native_instructions > depth as u64);
            assert_eq!(candidate.jit_stats().installed_regions, 1);
            assert_eq!(candidate.jit_stats().queued_requests, 0);
            assert_eq!(reference.jit_stats().native_instructions, 0);
            for (tier, samples) in [left_stack, right_stack].into_iter().enumerate() {
                let (low, high, count) = samples.get();
                assert_eq!(count, depth as usize / 128 + 1);
                assert!(high - low <= 32768, "host stack span {}", high - low);
                eprintln!(
                    "non-tail depth={depth} fuel={fuel} native={} samples={count} callback_stack_span={}",
                    tier == 1,
                    high - low
                );
            }
        }
    }
    Ok(())
}

#[test]
fn native_non_tail_calls_still_obey_the_frame_limit() -> Result<(), ExternError> {
    for native in [false, true] {
        let (mut lua, executor, _) = tail_state(native, 128, false)?;
        let mut finished = false;
        for _ in 0..100 {
            if call_step(&mut lua, &executor, 1).0 {
                finished = true;
                break;
            }
            lua.gc_collect();
        }
        assert!(finished);
        let error = call_result(&mut lua, &executor).unwrap_err();
        assert_eq!(error.root_cause().to_string(), "stack overflow");
        assert_eq!(lua.jit_stats().native_instructions > 0, native);
    }
    Ok(())
}

#[test]
fn unbounded_native_tail_calls_return_at_each_fuel_boundary() -> Result<(), ExternError> {
    for fuel in [1, 64, 65536] {
        let (mut reference, left, _) = tail_state(false, -1, true)?;
        let (mut candidate, right, _) = tail_state(true, -1, true)?;
        for _ in 0..32 {
            let before = candidate.jit_stats();
            let expected = call_step(&mut reference, &left, fuel);
            let actual = call_step(&mut candidate, &right, fuel);
            assert_eq!(actual, expected);
            assert!(!actual.0);
            let after = candidate.jit_stats();
            assert!(after.native_instructions > before.native_instructions);
            let work = after.native_instructions - before.native_instructions
                + after.interpreted_instructions
                - before.interpreted_instructions;
            assert!(work <= fuel as u64 + 63);
            reference.gc_collect();
            candidate.gc_collect();
        }
    }
    Ok(())
}

#[test]
fn coroutine_close_runs_pending_handlers_in_both_tiers() -> Result<(), ExternError> {
    let mut observed = Vec::new();
    for native in [false, true] {
        let mut lua = Lua::core();
        lua.set_jit_config(JitConfig {
            mode: if native { JitMode::Auto } else { JitMode::Off },
            hot_threshold: u32::MAX,
            ..Default::default()
        })
        .unwrap();
        let executor = lua.try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                Some("coroutine-close"),
                br#"
                local closed=0
                local co=coroutine.create(function()
                    local resource <close> = setmetatable({}, {
                        __close=function() closed=closed+1 end
                    })
                    coroutine.yield()
                end)
                assert(coroutine.resume(co))
                assert(coroutine.close(co))
                return closed
            "#
                .as_slice(),
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        lua.prepare_jit().unwrap();
        observed.push(lua.execute::<i64>(&executor)?);
        assert_eq!(lua.jit_stats().native_instructions > 0, native);
    }
    assert_eq!(observed, [1, 1]);
    Ok(())
}

fn cleanup_case(
    script: &str,
    native: bool,
    fuel: i32,
) -> Result<(Vec<(bool, ExecutorMode, i32)>, luna::JitStats), ExternError> {
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: if native { JitMode::Auto } else { JitMode::Off },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("cleanup-matrix"), script.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.prepare_jit().unwrap();
    let mut slices = Vec::new();
    for _ in 0..1000 {
        let step = call_step(&mut lua, &executor, fuel);
        slices.push(step);
        lua.gc_collect();
        if step.0 {
            assert!(lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<bool>(ctx)?)?);
            return Ok((slices, lua.jit_stats()));
        }
    }
    panic!("cleanup did not complete");
}

#[test]
fn coroutine_cleanup_keeps_identity_order_errors_and_slice_boundaries() -> Result<(), ExternError> {
    for (action, check, outer_error) in [
        ("", "ok and err==nil", 0),
        ("error(marker)", "not ok and err==marker", 1),
        ("coroutine.yield(marker)", "not ok and err~=nil", 2),
        (
            "coroutine.yieldto(coroutine.create(function() error('must not run') end))",
            "not ok and err~=nil",
            2,
        ),
        (
            "local child=coroutine.create(function() coroutine.yield(17) return 18 end) local ok,v=coroutine.resume(child) assert(ok and v==17) ok,v=coroutine.resume(child) assert(ok and v==18)",
            "ok and err==nil",
            0,
        ),
    ] {
        let script = format!(
            r#"
            local target
            local marker={{}}
            local events={{}} local errors={{}} local total=0
            local function resource(id)
                return setmetatable({{}},{{__close=function(_,err)
                    local running,main=coroutine.running()
                    assert(running==target and not main)
                    assert(not coroutine.isyieldable())
                    events[#events+1]=id
                    errors[#errors+1]=err==nil and 0 or err==marker and 1 or 2
                    for i=1,100 do total=total+i end
                    if id==2 then {action} end
                end}})
            end
            target=coroutine.create(function()
                local a <close> = resource(1)
                local function inner()
                    local b <close> = resource(2)
                    coroutine.yield(42)
                    error('resumed after close')
                end
                inner()
            end)
            local resumed,value=coroutine.resume(target)
            assert(resumed and value==42 and #events==0)
            local ok,err=coroutine.close(target)
            assert({check})
            assert(coroutine.status(target)=='dead')
            assert(events[1]==2 and events[2]==1 and #events==2)
            assert(errors[1]==0 and errors[2]=={outer_error})
            assert(total==10100)
            assert(coroutine.close(target))
            assert(#events==2)
            return true
            "#
        );
        for fuel in [1, 64, 65536] {
            let (expected, off) = cleanup_case(&script, false, fuel)?;
            let (actual, native) = cleanup_case(&script, true, fuel)?;
            assert_eq!(actual, expected, "action={action} fuel={fuel}");
            assert_eq!(off.native_instructions, 0);
            assert!(native.native_upvalue_writes >= 200);
        }
    }
    Ok(())
}

#[test]
fn coroutine_close_handles_dead_unstarted_running_and_waiting_threads() -> Result<(), ExternError> {
    let script = r#"
        local marker={}
        local dead=coroutine.create(function() error(marker) end)
        local ok,err=coroutine.resume(dead)
        assert(not ok and err==marker)
        ok,err=coroutine.close(dead)
        assert(not ok and err==marker)
        assert(coroutine.close(dead))
        local fresh=coroutine.create(function() error('must not run') end)
        assert(coroutine.close(fresh) and coroutine.status(fresh)=='dead')
        local parent
        parent=coroutine.create(function()
            assert(not pcall(coroutine.close,coroutine.running()))
            local child=coroutine.create(function()
                assert(not pcall(coroutine.close,parent))
                return true
            end)
            assert(coroutine.resume(child))
        end)
        assert(coroutine.resume(parent))
        assert(coroutine.close(parent))
        return true
    "#;
    for fuel in [1, 64, 65536] {
        let (expected, _) = cleanup_case(script, false, fuel)?;
        let (actual, _) = cleanup_case(script, true, fuel)?;
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[test]
fn cleanup_continues_after_missing_or_noncallable_handlers() -> Result<(), ExternError> {
    for replacement in ["nil", "42", "{}"] {
        for suspended in [false, true] {
            let finish = if suspended {
                "coroutine.yield()"
            } else {
                "error(marker)"
            };
            let invoke = if suspended {
                "local co=coroutine.create(body) assert(coroutine.resume(co)) local ok,err=coroutine.close(co)"
            } else {
                "local ok,err=pcall(body)"
            };
            let first_error = if suspended { "nil" } else { "marker" };
            let script = format!(
                r#"
                local marker={{}}
                local events={{}} local errors={{}} local total=0
                local mt={{__close=function() error('replaced handler ran') end}}
                local function resource(id)
                    return setmetatable({{}},{{__close=function(_,err)
                        events[#events+1]=id
                        errors[id]=err
                        for i=1,100 do total=total+i end
                        if id==3 then mt.__close={replacement} end
                    end}})
                end
                local function body()
                    local a <close> = resource(1)
                    local b <close> = setmetatable({{}},mt)
                    local c <close> = resource(3)
                    {finish}
                end
                {invoke}
                assert(not ok and err~=nil)
                assert(#events==2 and events[1]==3 and events[2]==1,'remaining handler skipped')
                assert(errors[3]=={first_error})
                assert(errors[1]~=nil and errors[1]~=marker)
                assert(total==10100)
                return true
                "#
            );
            for fuel in [1, 64, 65536] {
                let (expected, _) = cleanup_case(&script, false, fuel)?;
                let (actual, stats) = cleanup_case(&script, true, fuel)?;
                assert_eq!(
                    actual, expected,
                    "replacement={replacement} suspended={suspended} fuel={fuel}"
                );
                assert!(stats.native_upvalue_writes >= 200);
            }
        }
    }
    Ok(())
}

#[test]
fn failed_coroutines_defer_cleanup_and_wrapped_coroutines_close_automatically(
) -> Result<(), ExternError> {
    for failure in ["body", "handler", "dispatch", "replace"] {
        for wrapped in [false, true] {
            let trigger = if matches!(failure, "body" | "replace") {
                "error(marker)"
            } else {
                "return 42"
            };
            let inner = match failure {
                "handler" => "error(marker)",
                "dispatch" => "mt.__close=nil",
                "replace" => "error(replacement)",
                _ => "",
            };
            let expected_before = if matches!(failure, "body" | "replace") {
                0
            } else {
                1
            };
            let final_error = if failure == "replace" {
                "assert(err==replacement)"
            } else {
                ""
            };
            let invoke = if wrapped {
                "local call=coroutine.wrap(body) local ok,err=pcall(call) assert(not ok and err~=nil)"
                    .to_owned()
            } else {
                format!(
                    "local co=coroutine.create(body) local ok,err=coroutine.resume(co) assert(not ok and err~=nil) assert(#events=={expected_before},'cleanup ran before close') assert(coroutine.status(co)=='dead') ok,err=coroutine.close(co) assert(not ok and err~=nil) assert(coroutine.close(co))"
                )
            };
            let expected_middle = if failure == "dispatch" { "1" } else { "2" };
            let expected_count = if failure == "dispatch" { 2 } else { 3 };
            let script = format!(
                r#"
                local marker={{}} local replacement={{}} local events={{}} local total=0
                local mt={{__close=function(_,err)
                    events[#events+1]=2
                    for i=1,100 do total=total+i end
                end}}
                local function resource(id)
                    return setmetatable({{}},{{__close=function(_,err)
                        events[#events+1]=id
                        for i=1,100 do total=total+i end
                        if id==3 then {inner} end
                    end}})
                end
                local function body()
                    local a <close> = resource(1)
                    local b <close> = setmetatable({{}},mt)
                    local c <close> = resource(3)
                    {trigger}
                end
                {invoke}
                {final_error}
                assert(#events=={expected_count} and events[1]==3 and events[2]=={expected_middle})
                assert(events[#events]==1)
                assert(total==5050*{expected_count})
                return true
                "#
            );
            for fuel in [1, 64, 65536] {
                let (expected, _) = cleanup_case(&script, false, fuel)?;
                let (actual, stats) = cleanup_case(&script, true, fuel)?;
                assert_eq!(
                    actual, expected,
                    "failure={failure} wrapped={wrapped} fuel={fuel}"
                );
                assert!(stats.native_upvalue_writes >= 200);
            }
        }
    }
    Ok(())
}

#[test]
fn deferred_cleanup_merges_nested_batches_with_live_handler_locals() -> Result<(), ExternError> {
    let script = r#"
        local marker={} local log='' local total=0 local access
        local function resource(name,check)
            return setmetatable({}, {__close=function(_,err)
                assert(err==marker)
                if check then check() end
                log=log..name
                for i=1,100 do total=total+i end
            end})
        end
        local co=coroutine.create(function()
            local a <close> = resource('a')
            local b <close> = resource('b')
            local c <close> = setmetatable({}, {__close=function()
                log=log..'C'
                local x <close> = resource('x')
                local function inner()
                    local d <close> = resource('d')
                    local e <close> = setmetatable({}, {__close=function()
                        log=log..'E'
                        local shared={value=9}
                        access=function(value)
                            if value then shared.value=value end
                            return shared.value
                        end
                        local f <close> = resource('f',function() assert(shared.value==17) end)
                        error(marker)
                    end})
                end
                inner()
            end})
        end)
        local ok,err=coroutine.resume(co)
        assert(not ok and err==marker and log=='CE')
        assert(access()==9 and access(17)==17)
        ok,err=coroutine.close(co)
        assert(not ok and err==marker)
        assert(log=='CEfdxba' and total==25250)
        assert(access()==17)
        return true
    "#;
    for fuel in [1, 64, 65536] {
        let (expected, _) = cleanup_case(script, false, fuel)?;
        let (actual, stats) = cleanup_case(script, true, fuel)?;
        assert_eq!(actual, expected);
        assert!(stats.native_upvalue_writes >= 500);
    }
    Ok(())
}

#[test]
fn protected_coroutine_errors_still_unwind_before_the_catch_returns() -> Result<(), ExternError> {
    let script = r#"
        local marker={} local log=''
        local function resource(name)
            return setmetatable({}, {__close=function(_,err)
                assert(err==marker) log=log..name
            end})
        end
        local co=coroutine.create(function()
            local a <close> = resource('a')
            local ok,err=pcall(function()
                local b <close> = resource('b')
                error(marker)
            end)
            assert(not ok and err==marker and log=='b')
            error(marker)
        end)
        local ok,err=coroutine.resume(co)
        assert(not ok and err==marker and log=='b')
        ok,err=coroutine.close(co)
        assert(not ok and err==marker and log=='ba')
        return true
    "#;
    for fuel in [1, 64, 65536] {
        let (expected, _) = cleanup_case(script, false, fuel)?;
        let (actual, stats) = cleanup_case(script, true, fuel)?;
        assert_eq!(actual, expected);
        assert!(stats.native_upvalue_writes >= 2);
    }
    Ok(())
}

#[test]
fn host_restart_discards_deferred_frames_without_running_lua() -> Result<(), ExternError> {
    for native in [false, true] {
        for suspended in [false, true] {
            let mut lua = Lua::core();
            lua.set_jit_config(JitConfig {
                mode: if native { JitMode::Auto } else { JitMode::Off },
                ..Default::default()
            })
            .unwrap();
            let executor=lua.try_enter(|ctx| {
                let closure=Closure::load(ctx,None,br#"
                    closed=0
                    local co=coroutine.create(function()
                        local resource <close> = setmetatable({}, {__close=function() closed=closed+1 end})
                        error('parked')
                    end)
                    assert(not coroutine.resume(co))
                    assert(closed==0)
                    return co
                "#.as_slice())?;
                Ok(ctx.stash(Executor::start(ctx,closure.into(),())))
            })?;
            lua.prepare_jit().unwrap();
            lua.finish(&executor).unwrap();
            let thread = lua.try_enter(|ctx| {
                Ok(ctx.stash(ctx.fetch(&executor).take_result::<luna::Thread>(ctx)??))
            })?;
            lua.gc_collect();
            let restarted = lua.try_enter(|ctx| {
                let thread = ctx.fetch(&thread);
                let closure = Closure::load(ctx, None, b"return 42".as_slice())?;
                if suspended {
                    thread.start_suspended(&ctx, closure.into())?;
                    thread.resume(ctx, ())?;
                } else {
                    thread.start(ctx, closure.into(), ())?;
                }
                Ok(ctx.stash(Executor::run(&ctx, thread)?))
            })?;
            lua.prepare_jit().unwrap();
            lua.gc_collect();
            assert_eq!(lua.execute::<i64>(&restarted)?, 42);
            assert_eq!(lua.enter(|ctx| ctx.get_global::<i64>("closed").unwrap()), 0);
        }
    }
    Ok(())
}

#[test]
fn coroutine_status_distinguishes_pending_terminal_and_yielded_results() -> Result<(), ExternError>
{
    for (body, expected) in [
        ("error({})", "dead"),
        ("return 42", "dead"),
        ("coroutine.yield(42)", "suspended"),
    ] {
        for native in [false, true] {
            let mut lua = Lua::core();
            lua.set_jit_config(JitConfig {
                mode: if native { JitMode::Auto } else { JitMode::Off },
                hot_threshold: u32::MAX,
                ..Default::default()
            })
            .unwrap();
            let (executor, inspector)=lua.try_enter(|ctx| {
                let source=format!("target=coroutine.create(function() {body} end) return coroutine.resume(target)");
                let main=Closure::load(ctx,None,source.as_bytes())?;
                let inspect=Closure::load(ctx,None,b"return coroutine.status(target)".as_slice())?;
                Ok((ctx.stash(Executor::start(ctx,main.into(),())),ctx.stash(Executor::start(ctx,inspect.into(),()))))
            })?;
            lua.prepare_jit().unwrap();
            let mut observed = false;
            for _ in 0..128 {
                let step = call_step(&mut lua, &executor, 1);
                lua.gc_collect();
                let result = lua.enter(|ctx| {
                    ctx.get_global::<luna::Thread>("target")
                        .is_ok_and(|thread| thread.mode() == luna::ThreadMode::Result)
                });
                if result {
                    assert_eq!(lua.execute::<String>(&inspector)?, expected);
                    observed = true;
                    break;
                }
                assert!(!step.0, "result boundary was not exposed");
            }
            assert!(observed);
            lua.execute::<()>(&executor)?;
            assert_eq!(lua.jit_stats().native_instructions > 0, native);
        }
    }
    Ok(())
}

#[test]
fn raw_reset_clears_close_markers_before_reusing_the_executor() -> Result<(), ExternError> {
    for native in [false, true] {
        let mut lua = Lua::core();
        lua.set_jit_config(JitConfig {
            mode: if native { JitMode::Auto } else { JitMode::Off },
            ..Default::default()
        })
        .unwrap();
        let executor = lua.try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                br#"
                local a,b,c,d,e=1,2,3,4,5
                local resource <close> = setmetatable({}, {
                    __close=function() error('raw reset must not run Lua') end
                })
                coroutine.yield(a+b+c+d+e)
            "#
                .as_slice(),
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        lua.prepare_jit().unwrap();
        assert_eq!(lua.execute::<i64>(&executor)?, 15);
        lua.try_enter(|ctx| {
            let next = Closure::load(ctx, None, b"return 42".as_slice())?;
            ctx.fetch(&executor).restart(ctx, next.into(), ());
            Ok(())
        })?;
        lua.prepare_jit().unwrap();
        lua.gc_collect();
        assert_eq!(lua.execute::<i64>(&executor)?, 42);
    }
    Ok(())
}

#[cfg(feature = "async")]
#[test]
fn coroutine_cleanup_rejects_foreign_wait_without_polling_it() -> Result<(), ExternError> {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };
    struct Wait(Rc<Cell<usize>>, Rc<Cell<usize>>);
    impl Future for Wait {
        type Output = i64;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<i64> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(42)
        }
    }
    impl Drop for Wait {
        fn drop(&mut self) {
            self.1.set(self.1.get() + 1);
        }
    }
    for native in [false, true] {
        let mut lua = Lua::core();
        lua.set_jit_config(JitConfig {
            mode: if native { JitMode::Auto } else { JitMode::Off },
            ..Default::default()
        })
        .unwrap();
        let polls = Rc::new(Cell::new(0));
        let drops = Rc::new(Cell::new(0));
        lua.enter(|ctx| {
            let polls = polls.clone();
            let drops = drops.clone();
            ctx.set_global(
                "wait",
                Callback::from_fn(&ctx, move |ctx, _, _| {
                    let wait = Wait(polls.clone(), drops.clone());
                    Ok(CallbackReturn::Sequence(luna::async_sequence(
                        &ctx,
                        move |_, mut seq| async move {
                            seq.await_future(wait).await;
                            Ok(luna::SequenceReturn::Return)
                        },
                    )))
                }),
            );
        });
        let executor = lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, None, br#"
                local closed=0
                local co=coroutine.create(function()
                    local a <close> = setmetatable({}, {__close=function() closed=closed+1 end})
                    local b <close> = setmetatable({}, {__close=function() closed=closed+1 wait() end})
                    coroutine.yield()
                end)
                assert(coroutine.resume(co))
                local ok,err=coroutine.close(co)
                assert(not ok and err~=nil)
                assert(coroutine.status(co)=='dead')
                return closed==2
            "#.as_slice())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        lua.prepare_jit().unwrap();
        let mut finished = false;
        for _ in 0..1000 {
            let step = call_step(&mut lua, &executor, 1);
            assert!(lua
                .enter(|ctx| ctx.fetch(&executor).take_pending_future(&ctx))
                .is_none());
            lua.gc_collect();
            if step.0 {
                finished = true;
                break;
            }
        }
        assert!(finished);
        assert!(lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<bool>(ctx)?)?);
        assert_eq!(polls.get(), 0);
        assert_eq!(drops.get(), 1);
        assert_eq!(lua.jit_stats().native_upvalue_writes >= 2, native);
    }
    Ok(())
}

fn mixed_call_state(
    caller_native: bool,
    callee_native: bool,
    tail: bool,
) -> Result<(Lua, StashedExecutor), ExternError> {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: if caller_native || callee_native {
            JitMode::Auto
        } else {
            JitMode::Off
        },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        ctx.set_global(
            "park",
            Callback::from_fn(&ctx, |_, _, _| {
                Ok(CallbackReturn::Yield {
                    to_thread: None,
                    then: None,
                })
            }),
        );
    });
    let caller = format!(
        r#"
        local t={{sum=0}}
        for i=1,100 do t.sum=t.sum+i end
        park(1,t.sum)
        {}
        "#,
        if tail {
            "return callee(t.sum)"
        } else {
            r#"
            local a,b,c,d=callee(t.sum)
            park(4,a,b,c,d)
            for i=1,100 do t.sum=t.sum+i end
            return 5,t.sum,a,b,c,d
            "#
        }
    );
    let callee = br#"
        local initial=...
        park(2,initial)
        local t={sum=initial}
        for i=1,100 do t.sum=t.sum+i end
        park(3,t.sum)
        return t.sum,nil,17,nil
    "#;
    for prepare in [true, false] {
        for (name, script, native) in [
            ("caller", caller.as_bytes(), caller_native),
            ("callee", callee.as_slice(), callee_native),
        ] {
            if native == prepare {
                lua.try_enter(|ctx| {
                    let closure = Closure::load(ctx, Some(name), script)?;
                    ctx.set_global(name, closure);
                    Ok(())
                })?;
            }
        }
        if prepare {
            assert_eq!(
                lua.prepare_jit().unwrap(),
                usize::from(caller_native) + usize::from(callee_native)
            );
        }
    }
    let executor = lua.try_enter(|ctx| {
        let caller: Function = ctx.get_global("caller")?;
        Ok(ctx.stash(Executor::start(ctx, caller, ())))
    })?;
    Ok((lua, executor))
}

fn call_step(lua: &mut Lua, executor: &StashedExecutor, fuel: i32) -> (bool, ExecutorMode, i32) {
    lua.enter(|ctx| {
        let executor = ctx.fetch(executor);
        let mut fuel = Fuel::with(fuel);
        (
            executor.step(ctx, &mut fuel).unwrap(),
            executor.mode(),
            fuel.remaining(),
        )
    })
}

fn call_result(lua: &mut Lua, executor: &StashedExecutor) -> Result<Vec<Option<i64>>, ExternError> {
    lua.try_enter(|ctx| {
        Ok(ctx
            .fetch(executor)
            .take_result::<Variadic<Vec<Option<i64>>>>(ctx)??
            .0)
    })
}

#[test]
fn selective_caller_callee_tiers_preserve_calls_tail_calls_and_suspension(
) -> Result<(), ExternError> {
    for caller_native in [false, true] {
        for callee_native in [false, true] {
            for tail in [false, true] {
                for fuel in [1, 64, 65536] {
                    let case = format!(
                        "caller={caller_native} callee={callee_native} tail={tail} fuel={fuel}"
                    );
                    let (mut reference, left) = mixed_call_state(false, false, tail)?;
                    let (mut candidate, right) =
                        mixed_call_state(caller_native, callee_native, tail)?;
                    let installed = candidate.jit_stats().installed_regions;
                    let mut boundary = 0;
                    let mut previous_native = 0;
                    let mut previous_writes = 0;
                    let mut stopped = false;
                    for _ in 0..1000 {
                        let expected = call_step(&mut reference, &left, fuel);
                        let actual = call_step(&mut candidate, &right, fuel);
                        assert_eq!(actual, expected, "{case}");
                        reference.gc_collect();
                        candidate.gc_collect();
                        let stats = candidate.jit_stats();
                        assert_eq!(stats.installed_regions, installed, "{case}");
                        assert_eq!(stats.queued_requests, 0, "{case}");
                        if !actual.0 {
                            continue;
                        }
                        boundary += 1;
                        let expected = call_result(&mut reference, &left)?;
                        let result = call_result(&mut candidate, &right)?;
                        assert_eq!(result, expected, "{case} boundary={boundary}");
                        let values = match boundary {
                            1 => vec![Some(1), Some(5050)],
                            2 => vec![Some(2), Some(5050)],
                            3 => vec![Some(3), Some(10100)],
                            4 if tail => vec![Some(10100), None, Some(17), None],
                            4 => vec![Some(4), Some(10100), None, Some(17), None],
                            5 => vec![Some(5), Some(10100), Some(10100), None, Some(17), None],
                            _ => panic!("unexpected boundary: {case}"),
                        };
                        assert_eq!(result, values, "{case}");
                        let tier = match boundary {
                            1 | 5 => Some(caller_native),
                            3 => Some(callee_native),
                            _ => None,
                        };
                        if let Some(native) = tier {
                            assert_eq!(
                                stats.native_instructions > previous_native,
                                native,
                                "{case}"
                            );
                            assert_eq!(
                                stats.native_table_writes > previous_writes,
                                native,
                                "{case}"
                            );
                        }
                        previous_native = stats.native_instructions;
                        previous_writes = stats.native_table_writes;
                        let final_boundary = if tail { 4 } else { 5 };
                        for (lua, executor) in [(&mut reference, &left), (&mut candidate, &right)] {
                            let mode = lua.enter(|ctx| ctx.fetch(executor).mode());
                            if boundary == final_boundary {
                                assert_eq!(mode, ExecutorMode::Stopped, "{case}");
                            } else {
                                assert_eq!(mode, ExecutorMode::Suspended, "{case}");
                                lua.enter(|ctx| ctx.fetch(executor).resume(ctx, ()))
                                    .unwrap();
                            }
                        }
                        if boundary == final_boundary {
                            stopped = true;
                            break;
                        }
                    }
                    assert!(stopped, "slice bound exceeded: {case}");
                    assert_eq!(reference.jit_stats().native_instructions, 0);
                }
            }
        }
    }
    Ok(())
}

fn check_observation(report: &suspension::Report, mode: suspension::Mode, fuel: i32) {
    assert!(report.steps >= 3);
    if fuel <= 64 {
        assert!(report.steps > 3);
    }
    assert!(report.counted_work_max <= fuel as u64 + 63);
    assert!(report.fuel_debit_max <= i64::from(fuel) + 67);
    assert!(report.elapsed_ns >= report.load_ns);
    assert!(report.poll_ns >= report.max_poll_ns);
    assert!(report.gc_peak > 0);
    assert!(report.code_peak >= report.stats.code_bytes);
    assert!(report.queue_peak <= 16);
    assert_eq!(report.native_table_write_segments.len(), 3);
    if mode != suspension::Mode::Off {
        let first = usize::from(mode == suspension::Mode::Auto && fuel == 65536);
        if first == 1 {
            assert_eq!(report.native_segments[0], 0);
            assert_eq!(report.native_table_write_segments[0], 0);
        }
        assert!(report.native_segments[first..]
            .iter()
            .all(|count| *count > 0));
        assert!(report.native_table_write_segments[first..]
            .iter()
            .all(|count| *count > 0));
    }
}

#[test]
fn public_coroutine_slices_resume_with_verified_tiered_heap_work() {
    for mode in [
        suspension::Mode::Off,
        suspension::Mode::Auto,
        suspension::Mode::Prepared,
    ] {
        for fuel in [1, 64, 65536] {
            let report = suspension::run("coroutine_resume", mode, fuel).unwrap();
            assert_eq!(report.boundary_values, [20100, 40200, 60300]);
            assert_eq!(report.suspensions, 2);
            assert_eq!(report.pending_polls, 0);
            assert_eq!(report.ready_polls, 0);
            assert_eq!(report.wakes, 0);
            assert_eq!(report.forced_gc_cycles, 4);
            check_observation(&report, mode, fuel);
        }
    }
}

#[cfg(feature = "async")]
#[test]
fn public_foreign_futures_park_outside_arena_and_resume_native_heap_work() {
    for mode in [
        suspension::Mode::Off,
        suspension::Mode::Auto,
        suspension::Mode::Prepared,
    ] {
        for fuel in [1, 64, 65536] {
            let report = suspension::run("foreign_await", mode, fuel).unwrap();
            assert_eq!(report.boundary_values, [60384]);
            assert_eq!(report.suspensions, 2);
            assert_eq!(report.pending_polls, 6);
            assert_eq!(report.ready_polls, 2);
            assert_eq!(report.wakes, 6);
            assert_eq!(report.forced_gc_cycles, 12);
            check_observation(&report, mode, fuel);
        }
    }
}
