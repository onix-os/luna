#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::sync::Arc;

use luna::{
    Callback, Closure, Executor, ExecutorMode, ExternError, Fuel, JitConfig, JitMode, JitStats,
    Lua, RuntimeError,
};

struct Run {
    result: Result<String, ExternError>,
    slices: Vec<(bool, ExecutorMode, i32)>,
    stats: JitStats,
}

fn run(source: &str, native: bool, budget: i32, error: Option<RuntimeError>) -> Run {
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: if native { JitMode::Auto } else { JitMode::Off },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        if let Some(error) = error {
            ctx.set_global(
                "typed",
                Callback::from_fn(&ctx, move |_, _, _| Err(error.clone().into())),
            );
        }
        let closure = Closure::load(ctx, Some("native-errors"), source.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    if native {
        while lua.prepare_jit().unwrap() != 0 {}
    }
    let installed = lua.jit_stats().installed_regions;
    let mut slices = Vec::new();
    for _ in 0..1000 {
        let slice = lua.enter(|ctx| {
            let executor = ctx.fetch(&executor);
            let mut fuel = Fuel::with(budget);
            (
                executor.step(ctx, &mut fuel).unwrap(),
                executor.mode(),
                fuel.remaining(),
            )
        });
        slices.push(slice);
        lua.gc_collect();
        assert_eq!(lua.jit_stats().installed_regions, installed);
        assert_eq!(lua.jit_stats().queued_requests, 0);
        if slice.0 {
            let result = lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<String>(ctx)?);
            let stats = lua.jit_stats();
            if native {
                assert!(stats.native_instructions > 0);
                assert!(stats.native_table_writes >= 101);
            } else {
                assert_eq!(stats.native_entries, 0);
            }
            return Run {
                result,
                slices,
                stats,
            };
        }
    }
    panic!("error scenario did not complete");
}

fn program(fault: &str, catch: &str) -> String {
    format!(
        "local function fail()\nlocal t={{n=0}}\nfor i=1,100 do t.n=t.n+1 end\n{fault}\nend\n{catch}"
    )
}

#[test]
fn native_guard_and_call_errors_keep_fault_lines_and_reference_slices() {
    for fault in [
        "local missing=nil; return missing.value",
        "return t+1",
        "t[nil]=1",
        "return t.n//0",
        "return t()",
        "local result=t(); return result",
        "error('expected')",
    ] {
        for (catch, caught) in [
            ("return fail()", false),
            ("local ok,e=pcall(fail); assert(not ok); return tostring(e)", true),
            ("local ok,e=xpcall(fail,function(e) return 'caught:'..tostring(e) end); assert(not ok); return e", true),
        ] {
            let source = program(fault, catch);
            for budget in [1, 64, 65536] {
                let reference = run(&source, false, budget, None);
                let native = run(&source, true, budget, None);
                assert_eq!(native.slices, reference.slices, "{source}, fuel={budget}");
                assert_eq!(native.result.is_ok(), caught, "{source}");
                let rendered = |result: Result<String, ExternError>| result.unwrap_or_else(|error| error.to_string());
                let actual = rendered(native.result);
                assert_eq!(actual, rendered(reference.result), "{source}, fuel={budget}");
                assert!(actual.contains("native-errors:4:"), "{actual}");
                if fault == "t[nil]=1" {
                    assert!(native.stats.helper_declines > 0);
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("typed callback sentinel")]
struct Sentinel(Arc<()>);

#[test]
fn native_prefix_preserves_typed_callback_payload_through_catch_and_rethrow() {
    for catch in [
        "return fail()",
        "local ok,e=pcall(fail); assert(not ok); error(e,0)",
        "local ok,e=xpcall(fail,function(e) return e end); assert(not ok); error(e,0)",
    ] {
        let source = program("return typed()", catch);
        for budget in [1, 64, 65536] {
            let token = Arc::new(());
            let error = RuntimeError::new(Sentinel(token.clone()));
            let reference = run(&source, false, budget, Some(error.clone()));
            let native = run(&source, true, budget, Some(error));
            assert_eq!(native.slices, reference.slices, "{source}, fuel={budget}");
            let expected = reference.result.unwrap_err();
            let actual = native.result.unwrap_err();
            assert_eq!(actual.to_string(), expected.to_string());
            for error in [expected, actual] {
                let sentinel = error.root_cause().downcast_ref::<Sentinel>().unwrap();
                assert!(Arc::ptr_eq(&sentinel.0, &token));
            }
        }
    }
}
