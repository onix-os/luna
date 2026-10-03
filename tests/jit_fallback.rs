#![cfg(all(
    feature = "jit",
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExecutorMode, Fuel, JitConfig, JitError, JitMode,
    Lua, StashedExecutor,
};

const SOURCE: &[u8] = br#"
local t = setmetatable({}, {__index=function() return 3 end})
local count = 0
local function bump(v) count = count + v; return count end
local co = coroutine.create(function()
    for i=1,20 do
        t[i] = bump(host(i))
        coroutine.yield(i)
    end
    return count
end)
local yielded = 0
while true do
    local ok, value = coroutine.resume(co)
    assert(ok)
    if coroutine.status(co) == "dead" then break end
    yielded = yielded + value
end
local ok, err = pcall(error, "caught", 0)
return count, yielded, t[20] + t.missing, not ok and err == "caught"
"#;

fn state(mode: JitMode) -> Lua {
    let mut lua = Lua::core();
    assert!(!lua.jit_capabilities().supported_target);
    assert!(lua.jit_capabilities().unavailable_reason.is_some());
    lua.set_jit_config(JitConfig {
        mode,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let host = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value: i64 = stack.consume(ctx)?;
            stack.replace(ctx, value + 1);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("host", host);
    });
    lua
}

fn load(lua: &mut Lua) -> StashedExecutor {
    lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("unsupported-target"), SOURCE)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })
    .unwrap()
}

fn no_native_work(lua: &Lua) {
    let stats = lua.jit_stats();
    assert_eq!(stats.native_entries, 0);
    assert_eq!(stats.native_instructions, 0);
    assert_eq!(stats.guard_exits, 0);
    assert_eq!(stats.native_interpreter_exits, 0);
    assert_eq!(stats.native_budget_exits, 0);
    assert_eq!(stats.native_panic_exits, 0);
    assert_eq!(stats.installed_regions, 0);
    assert_eq!(stats.code_bytes, 0);
    assert_eq!(stats.snapshot_bytes, 0);
    assert_eq!(stats.compilation_requests, 0);
    assert_eq!(stats.compilation_failures, 0);
    assert_eq!(stats.queued_requests, 0);
}

#[test]
fn unsupported_auto_preserves_exact_slices_through_callbacks_and_coroutines() {
    for budget in [-1, 0, 1, 64, 4096] {
        let run = |mode| {
            let mut lua = state(mode);
            let executor = load(&mut lua);
            if mode == JitMode::Auto {
                assert!(matches!(lua.prepare_jit(), Err(JitError::Unavailable(_))));
            } else {
                assert_eq!(lua.prepare_jit().unwrap(), 0);
            }
            let mut trace = Vec::new();
            loop {
                if mode == JitMode::Auto {
                    assert!(matches!(lua.service_jit(), Err(JitError::Unavailable(_))));
                }
                let slice = lua.enter(|ctx| {
                    let mut fuel = Fuel::with(budget);
                    let executor = ctx.fetch(&executor);
                    let done = executor.step(ctx, &mut fuel).unwrap();
                    (
                        done,
                        executor.mode(),
                        fuel.remaining(),
                        fuel.is_interrupted(),
                    )
                });
                trace.push(slice);
                lua.gc_collect();
                assert!(trace.len() < 10_000);
                if slice.0 {
                    assert_eq!(slice.1, ExecutorMode::Result);
                    break;
                }
            }
            let result = lua
                .try_enter(|ctx| {
                    ctx.fetch(&executor)
                        .take_result::<(i64, i64, i64, bool)>(ctx)?
                })
                .unwrap();
            assert_eq!(result, (230, 210, 233, true));
            assert!(lua.jit_stats().interpreted_instructions > 0);
            no_native_work(&lua);
            trace
        };
        assert_eq!(run(JitMode::Off), run(JitMode::Auto), "budget {budget}");
    }
}

#[test]
fn unavailable_service_does_not_break_convenience_execution_or_mode_switches() {
    let mut lua = state(JitMode::Off);
    for mode in [JitMode::Off, JitMode::Auto, JitMode::Off, JitMode::Auto] {
        let mut config = lua.jit_config();
        config.mode = mode;
        lua.set_jit_config(config).unwrap();
        let executor = load(&mut lua);
        if mode == JitMode::Auto {
            assert!(matches!(lua.prepare_jit(), Err(JitError::Unavailable(_))));
            assert!(matches!(lua.service_jit(), Err(JitError::Unavailable(_))));
        } else {
            assert_eq!(lua.prepare_jit().unwrap(), 0);
            assert_eq!(lua.service_jit().unwrap(), 0);
        }
        assert_eq!(
            lua.execute::<(i64, i64, i64, bool)>(&executor).unwrap(),
            (230, 210, 233, true)
        );
        lua.clear_jit_cache();
        drop(executor);
        lua.gc_collect();
        no_native_work(&lua);
    }
}
