#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::{cell::RefCell, rc::Rc};

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExecutorMode, Fuel, JitConfig, JitMode, JitStats,
    Lua, Table,
};

#[derive(Debug, PartialEq, Eq)]
struct Slice {
    done: bool,
    mode: ExecutorMode,
    fuel: i32,
    interrupted: bool,
    events: usize,
}

struct Run {
    result: String,
    events: Vec<(String, String)>,
    slices: Vec<Slice>,
    checkpoints: Vec<(String, u64)>,
    stats: JitStats,
}

fn run(source: &str, native: bool, budget: i32) -> Run {
    let mut lua = Lua::core();
    lua.load_debug();
    lua.set_jit_config(JitConfig {
        mode: if native { JitMode::Auto } else { JitMode::Off },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let executor = lua.enter(|ctx| {
        let target = Table::new(&ctx);
        target.set(ctx, "value", 0).unwrap();
        ctx.set_global("target", target);
        let recorded = events.clone();
        ctx.set_global(
            "record",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let event: (String, String) = stack.consume(ctx)?;
                recorded.borrow_mut().push(event);
                assert!(recorded.borrow().len() < 10_000);
                Ok(CallbackReturn::Return)
            }),
        );
        let recorded = events.clone();
        ctx.set_global(
            "checkpoint",
            Callback::from_fn(&ctx, move |ctx, mut exec, mut stack| {
                let name: String = stack.consume(ctx)?;
                recorded.borrow_mut().push(("checkpoint".into(), name));
                exec.fuel().interrupt();
                Ok(CallbackReturn::Return)
            }),
        );
        let closure = Closure::load(ctx, Some("jit-debug"), source.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    if native {
        while lua.prepare_jit().unwrap() != 0 {}
    }
    let installed = lua.jit_stats().installed_regions;
    let mut slices = Vec::new();
    let mut checkpoints = Vec::new();
    for _ in 0..20_000 {
        let slice = lua.enter(|ctx| {
            let mut fuel = Fuel::with(budget);
            let executor = ctx.fetch(&executor);
            Slice {
                done: executor.step(ctx, &mut fuel).unwrap(),
                mode: executor.mode(),
                fuel: fuel.remaining(),
                interrupted: fuel.is_interrupted(),
                events: events.borrow().len(),
            }
        });
        if slice.interrupted {
            let (kind, name) = events.borrow().last().unwrap().clone();
            assert_eq!(kind, "checkpoint");
            checkpoints.push((name, lua.jit_stats().native_table_writes));
        }
        let done = slice.done;
        slices.push(slice);
        lua.gc_collect();
        assert_eq!(lua.jit_stats().installed_regions, installed);
        assert_eq!(lua.jit_stats().queued_requests, 0);
        if done {
            let result = lua
                .try_enter(|ctx| ctx.fetch(&executor).take_result::<String>(ctx)?)
                .unwrap();
            let events = events.borrow().clone();
            let stats = lua.jit_stats();
            if native {
                assert!(stats.native_instructions > 0);
                assert!(stats.native_table_writes >= 100);
            } else {
                assert_eq!(stats.native_entries, 0);
            }
            return Run {
                result,
                events,
                slices,
                checkpoints,
                stats,
            };
        }
    }
    panic!("debug scenario did not finish");
}

fn compare(source: &str, budget: i32) -> Run {
    let reference = run(source, false, budget);
    let native = run(source, true, budget);
    assert_eq!(native.result, reference.result, "fuel {budget}");
    assert_eq!(native.events, reference.events, "fuel {budget}");
    assert_eq!(native.slices, reference.slices, "fuel {budget}");
    native
}

#[test]
fn hook_events_and_replacement_match_reference_between_native_prefix_and_suffix() {
    for (mask, count) in [("l", 0), ("", 7), ("l", 7)] {
        for replace in [false, true] {
            let source = format!(
                r#"
for i=1,100 do target.value=i end
checkpoint("prefix")
local hits = 0
local function second(event, line)
    record("second:" .. event, tostring(line or 0))
end
local function first(event, line)
    record("first:" .. event, tostring(line or 0))
    hits = hits + 1
    if {replace} and hits == 3 then debug.sethook(second, "{mask}", {count}) end
end
debug.sethook(first, "{mask}", {count})
local sum = 0
for i=1,30 do
    sum = sum + i
    target.value = 100 + i
end
debug.sethook()
checkpoint("clear")
for i=1,100 do target.value=200+i end
checkpoint("suffix")
return tostring(target.value)
"#
            );
            for budget in [-1, 0, 1, 64, 4096] {
                let native = compare(&source, budget);
                assert_eq!(native.result, "300");
                assert_eq!(
                    native.checkpoints,
                    [
                        ("prefix".into(), 100),
                        ("clear".into(), 100),
                        ("suffix".into(), 200)
                    ]
                );
                assert!(native.stats.hook_exits > 0);
                assert!(native
                    .events
                    .iter()
                    .any(|(kind, _)| kind.starts_with("first:")));
                assert_eq!(
                    native
                        .events
                        .iter()
                        .any(|(kind, _)| kind.starts_with("second:")),
                    replace
                );
                if mask.is_empty() {
                    assert!(native
                        .events
                        .iter()
                        .any(|(kind, _)| kind.ends_with(":count")));
                } else {
                    assert!(native
                        .events
                        .iter()
                        .any(|(kind, _)| kind.ends_with(":line")));
                }
            }
        }
    }
}

#[test]
fn mixed_tier_tracebacks_preserve_normal_tail_and_error_handler_frames() {
    for tail in [false, true] {
        let call = if tail {
            "return inner()"
        } else {
            "local value=inner(); return value"
        };
        let source = format!(
            r#"
for i=1,100 do target.value=i end
checkpoint("prefix")
local function inner()
    local value = target.value + 1
    record("trace", debug.traceback("inner"))
    error("expected", 0)
    return value
end
local function outer()
    {call}
end
local ok, trace = xpcall(outer, function(err)
    record("error", tostring(err))
    return debug.traceback("handler")
end)
record("trace", trace)
assert(not ok)
for i=1,100 do target.value=200+i end
checkpoint("suffix")
return tostring(target.value)
"#
        );
        for budget in [-1, 0, 1, 64, 4096] {
            let native = compare(&source, budget);
            assert_eq!(native.result, "300");
            assert_eq!(
                native.checkpoints,
                [("prefix".into(), 100), ("suffix".into(), 200)]
            );
            let traces: Vec<_> = native
                .events
                .iter()
                .filter(|(kind, _)| kind == "trace")
                .collect();
            assert_eq!(traces.len(), 2);
            assert!(traces[0].1.starts_with("inner\nstack traceback:"));
            assert_eq!(
                traces[0].1.matches("jit-debug:").count(),
                if tail { 2 } else { 3 }
            );
            assert!(traces[1].1.starts_with("handler\nstack traceback:"));
            assert!(native.events.contains(&("error".into(), "expected".into())));
        }
    }
}

#[test]
fn hook_errors_keep_tracebacks_catch_order_and_native_resumption() {
    for (mask, count) in [("l", 0), ("", 3), ("l", 3)] {
        let source = format!(
            r#"
for i=1,100 do target.value=i end
checkpoint("prefix")
local ok, err = pcall(function()
    debug.sethook(function(event, line)
        record(event, tostring(line or 0))
        record("trace", debug.traceback("hook"))
        debug.sethook()
        error("hook fault", 0)
    end, "{mask}", {count})
    for i=1,30 do target.value=100+i end
end)
assert(not ok and err == "hook fault")
record("caught", err)
checkpoint("clear")
for i=1,100 do target.value=200+i end
checkpoint("suffix")
return tostring(target.value)
"#
        );
        for budget in [-1, 0, 1, 64, 4096] {
            let native = compare(&source, budget);
            assert_eq!(native.result, "300");
            assert_eq!(
                native.checkpoints,
                [
                    ("prefix".into(), 100),
                    ("clear".into(), 100),
                    ("suffix".into(), 200)
                ]
            );
            assert!(native.stats.hook_exits > 0);
            let trace = native
                .events
                .iter()
                .position(|(kind, _)| kind == "trace")
                .unwrap();
            let caught = native
                .events
                .iter()
                .position(|(kind, _)| kind == "caught")
                .unwrap();
            assert!(trace < caught);
            assert!(native.events[trace].1.starts_with("hook\nstack traceback:"));
            assert_eq!(native.events[caught].1, "hook fault");
            assert_eq!(
                native
                    .events
                    .iter()
                    .filter(|(kind, _)| kind == "line" || kind == "count")
                    .count(),
                1
            );
        }
    }
}
