#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{
    Closure, Executor, ExternError, Fuel, JitConfig, JitError, JitMode, Lua, StashedClosure,
    StashedExecutor,
};

fn state(script: &[u8]) -> (Lua, StashedExecutor) {
    let (lua, closure, executor) = retained_state(script);
    drop(closure);
    (lua, executor)
}

fn retained_state(script: &[u8]) -> (Lua, StashedClosure, StashedExecutor) {
    let mut lua = Lua::empty();
    lua.gc_stop();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let (closure, executor) = lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("host-memory"), script).unwrap();
        (
            ctx.stash(closure),
            ctx.stash(Executor::start(ctx, closure.into(), ())),
        )
    });
    (lua, closure, executor)
}

#[test]
fn accounted_usage_matches_live_components_and_gc_remains_unchanged() {
    let (mut lua, executor) = state(b"local x=40 return x+2");
    let gc = lua.total_memory();
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let stats = lua.jit_stats();
    assert!(stats.code_bytes > 0 && stats.metadata_bytes > 0);
    assert_eq!(stats.snapshot_bytes, 0);
    assert_eq!(
        stats.accounted_jit_bytes,
        stats.code_bytes + stats.metadata_bytes
    );
    assert_eq!(lua.total_memory(), gc);
    assert_eq!(lua.total_memory(), lua.gc_metrics().total_allocation());
    assert_eq!(lua.accounted_memory(), gc + stats.accounted_jit_bytes);
    drop(executor);
    lua.gc_collect();
    lua.gc_collect();
    assert_eq!(lua.jit_stats().accounted_jit_bytes, 0);
    assert_eq!(lua.accounted_memory(), lua.total_memory());
}

#[test]
fn exact_combined_peak_admits_and_one_byte_less_refuses_and_recovers() -> Result<(), ExternError> {
    let script = b"local x=40 return x+2";
    let (mut probe, _executor) = state(script);
    assert_eq!(probe.prepare_jit().unwrap(), 1);
    let peak = probe.jit_stats().accounted_jit_peak_bytes;
    for shortage in [0, 1] {
        let (mut lua, closure, executor) = retained_state(script);
        let limit = lua.total_memory() + peak - shortage;
        lua.set_memory_limit(Some(limit));
        let result = lua.prepare_jit();
        if shortage == 0 {
            assert_eq!(result.unwrap(), 1);
            assert_eq!(lua.jit_stats().accounted_jit_peak_bytes, peak);
            assert_eq!(lua.jit_stats().host_allocation_refusals, 0);
            assert!(lua.accounted_memory() <= limit);
            assert_eq!(lua.execute::<i64>(&executor)?, 42);
            assert!(lua.jit_stats().native_entries > 0);
        } else {
            assert!(
                matches!(result, Err(JitError::ResourceLimit("host memory"))),
                "{result:?}"
            );
            let stats = lua.jit_stats();
            assert_eq!(
                (stats.snapshot_bytes, stats.code_bytes, stats.native_entries),
                (0, 0, 0)
            );
            assert!(stats.host_allocation_refusals > 0);
            assert_eq!(stats.accounted_jit_bytes, stats.metadata_bytes);
            assert!(lua.accounted_memory() <= limit);
            lua.set_memory_limit(None);
            let mut config = lua.jit_config();
            config.mode = JitMode::Off;
            lua.set_jit_config(config).unwrap();
            assert_eq!(lua.execute::<i64>(&executor)?, 42);
            assert_eq!(lua.jit_stats().native_entries, 0);
            let mut config = lua.jit_config();
            config.mode = JitMode::Auto;
            lua.set_jit_config(config).unwrap();
            lua.clear_jit_cache();
            assert_eq!(lua.prepare_jit().unwrap(), 1);
            let executor =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&closure).into(), ())));
            assert_eq!(lua.execute::<i64>(&executor)?, 42);
            assert!(lua.jit_stats().native_entries > 0);
        }
    }
    Ok(())
}

#[test]
fn service_refuses_snapshot_after_headroom_is_exhausted_and_preserves_frame(
) -> Result<(), ExternError> {
    let (mut lua, executor) = state(b"local x=0 for i=1,1000 do x=x+1 end return x");
    for _ in 0..10 {
        let mut fuel = Fuel::with(0);
        lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel))
            .unwrap();
        if lua.jit_stats().queued_requests > 0 {
            break;
        }
    }
    assert_eq!(lua.jit_stats().queued_requests, 1);
    lua.set_memory_limit(Some(lua.accounted_memory()));
    assert!(matches!(
        lua.service_jit(),
        Err(JitError::ResourceLimit("host memory"))
    ));
    let stats = lua.jit_stats();
    assert_eq!(
        (stats.snapshot_bytes, stats.code_bytes, stats.native_entries),
        (0, 0, 0)
    );
    assert_eq!(stats.accounted_jit_bytes, stats.metadata_bytes);
    assert!(stats.host_allocation_refusals > 0);
    lua.set_memory_limit(None);
    assert_eq!(lua.execute::<i64>(&executor)?, 1000);
    Ok(())
}

#[test]
fn finish_reclaims_cached_code_before_stopping_a_live_source() -> Result<(), ExternError> {
    let (mut lua, executor) = state(b"local x=40 return x+2");
    lua.prepare_jit().unwrap();
    let limit = lua.total_memory() + lua.jit_stats().metadata_bytes;
    assert!(lua.accounted_memory() > limit);
    lua.set_memory_limit(Some(limit));
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(lua.jit_stats().native_entries, 0);
    assert!(lua.accounted_memory() <= limit);
    Ok(())
}

#[test]
fn host_limits_are_independent_between_lua_states() -> Result<(), ExternError> {
    let (mut limited, _executor) = state(b"return 42");
    let (mut unrestricted, executor) = state(b"return 42");
    limited.set_memory_limit(Some(0));
    assert!(matches!(
        limited.prepare_jit(),
        Err(JitError::ResourceLimit("host memory"))
    ));
    assert_eq!(limited.jit_stats().code_bytes, 0);
    assert_eq!(unrestricted.prepare_jit().unwrap(), 1);
    assert_eq!(unrestricted.execute::<i64>(&executor)?, 42);
    assert!(unrestricted.jit_stats().native_entries > 0);
    Ok(())
}

#[cfg(feature = "async")]
#[test]
fn async_memory_limit_stops_before_polling_parked_foreign_future() {
    use luna::{async_sequence, Callback, CallbackReturn, ExecutorMode, SequenceReturn};
    use std::{
        cell::Cell,
        future::Future,
        rc::Rc,
        task::{Context, Poll, Waker},
    };

    let mut lua = Lua::empty();
    lua.gc_stop();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let polls = Rc::new(Cell::new(0));
    let executor = lua.enter(|ctx| {
        let polls = polls.clone();
        let callback = Callback::from_fn(&ctx, move |ctx, _, _| {
            ctx.set_global("retained", luna::String::from_slice(&ctx, vec![42; 65536]));
            let polls = polls.clone();
            Ok(CallbackReturn::Sequence(async_sequence(
                &ctx,
                move |_, mut seq| async move {
                    let value = seq
                        .await_future(async move {
                            polls.set(polls.get() + 1);
                            42
                        })
                        .await;
                    seq.enter(|ctx, _, _, mut stack| stack.replace(ctx, value));
                    Ok(SequenceReturn::Return)
                },
            )))
        });
        ctx.set_global("park", callback);
        let closure = Closure::load(
            ctx,
            None,
            b"local x=0 for i=1,50 do x=x+1 end return park()",
        )
        .unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    lua.prepare_jit().unwrap();
    lua.set_memory_limit(Some(lua.accounted_memory() + 4096));
    let mut future = Box::pin(lua.finish_async(&executor));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    drop(future);
    assert_eq!(polls.get(), 0);
    assert_eq!(
        lua.enter(|ctx| ctx.fetch(&executor).mode()),
        ExecutorMode::Stopped
    );
    assert!(lua.jit_stats().native_instructions > 0);
    assert!(lua.total_memory() > lua.memory_limit().unwrap());
}
