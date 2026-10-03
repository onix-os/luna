#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::{cell::RefCell, rc::Rc};

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExecutorMode, Fuel, JitConfig, JitMode, Lua, Table,
};

#[derive(Debug, PartialEq, Eq)]
struct Slice {
    done: bool,
    mode: ExecutorMode,
    fuel: i32,
    interrupted: bool,
    events_before_collection: usize,
    events_after_collection: Vec<String>,
    value: i64,
}

fn run(request: &str, native: bool, budget: i32) -> Vec<Slice> {
    let mut lua = Lua::core();
    lua.gc_stop();
    lua.set_jit_config(JitConfig {
        mode: if native { JitMode::Auto } else { JitMode::Off },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let events = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = lua.enter(|ctx| {
        let target = Table::new(&ctx);
        target.set(ctx, "value", 0).unwrap();
        ctx.set_global("target", target);
        let events = events.clone();
        ctx.set_global(
            "record",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                events.borrow_mut().push(stack.consume(ctx)?);
                Ok(CallbackReturn::Return)
            }),
        );
        let script = r#"
for i=1,100 do target.value=i end
local finalized=false
local weak=setmetatable({}, {__mode="v"})
local function make()
    weak.entry={}
    weak.finalized=setmetatable({}, {__gc=function()
        finalized=true
        record("finalizer")
    end})
end
make()
record("before")
local first=GC_REQUEST
record("between")
local second=GC_REQUEST
record("after")
assert(first==0 and second==0, "collection result")
assert(weak.entry==nil, "weak entry retained")
assert(finalized, "finalizer not run")
for i=1,100 do target.value=200+i end
return target.value
"#
        .replace("GC_REQUEST", request);
        let closure = Closure::load(ctx, Some("gc-request"), script.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    if native {
        while lua.prepare_jit().unwrap() != 0 {}
    }
    let mut slices = Vec::new();
    let mut interrupts = 0;
    let mut writes_at_last_request = 0;
    for _ in 0..10_000 {
        let mut slice = lua.enter(|ctx| {
            let mut fuel = Fuel::with(budget);
            let executor = ctx.fetch(&executor);
            let done = executor.step(ctx, &mut fuel).unwrap();
            let target: Table = ctx.get_global("target").unwrap();
            Slice {
                done,
                mode: executor.mode(),
                fuel: fuel.remaining(),
                interrupted: fuel.is_interrupted(),
                events_before_collection: events.borrow().len(),
                events_after_collection: Vec::new(),
                value: target.get(ctx, "value").unwrap(),
            }
        });
        lua.run_finalizers();
        slice.events_after_collection = events.borrow().clone();
        assert!(!lua.gc_is_automatic());
        assert_eq!(lua.jit_stats().queued_requests, 0);
        if slice.interrupted {
            interrupts += 1;
            assert_eq!(slice.value, 100);
            assert!(!slice
                .events_after_collection
                .iter()
                .any(|event| event == "after"));
            if interrupts == 1 {
                assert!(!slice
                    .events_after_collection
                    .iter()
                    .any(|event| event == "between"));
            }
            writes_at_last_request = lua.jit_stats().native_table_writes;
            if native {
                assert!(writes_at_last_request >= 100);
            }
        }
        let done = slice.done;
        slices.push(slice);
        if done {
            assert_eq!(
                lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<i64>(ctx)?)
                    .unwrap_or_else(|error| panic!(
                        "{request}, native={native}, fuel={budget}, events={:?}: {error}",
                        events.borrow()
                    )),
                300
            );
            assert_eq!(interrupts, 2);
            let events = events.borrow();
            assert_eq!(
                events.iter().filter(|event| *event == "finalizer").count(),
                1
            );
            assert!(
                events
                    .iter()
                    .position(|event| event == "finalizer")
                    .unwrap()
                    < events.iter().position(|event| event == "after").unwrap()
            );
            if native {
                assert_eq!(
                    lua.jit_stats().native_table_writes - writes_at_last_request,
                    100
                );
                assert!(lua.jit_stats().native_instructions > 0);
            } else {
                assert_eq!(lua.jit_stats().native_entries, 0);
            }
            return slices;
        }
    }
    panic!("collection scenario did not finish");
}

#[test]
fn collection_forms_preserve_interrupts_finalizers_and_native_resumption() {
    for request in [
        "collectgarbage('collect')",
        "collectgarbage()",
        "collectgarbage(nil)",
    ] {
        for budget in [-1, 0, 1, 64, 65536] {
            let reference = run(request, false, budget);
            let native = run(request, true, budget);
            assert_eq!(native, reference, "{request}, fuel={budget}");
        }
    }
}
