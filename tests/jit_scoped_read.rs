#![cfg(all(
    feature = "jit",
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::{cell::Cell, rc::Rc};

use luna::{
    Callback, CallbackReturn, Closure, Executor, Fuel, JitConfig, JitMode, Lua, StashedClosure,
    Table, Value,
};

const SOURCE: &[u8] = b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum";

fn setup(limit: usize) -> (Lua, StashedClosure) {
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        max_code_bytes: limit,
        ..Default::default()
    })
    .unwrap();
    let closure = lua.enter(|ctx| {
        ctx.set_global(
            "host_increment",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let value: i64 = stack.consume(ctx)?;
                stack.replace(ctx, value + 1);
                Ok(CallbackReturn::Return)
            }),
        );
        ctx.stash(Closure::load(ctx, None, SOURCE).unwrap())
    });
    (lua, closure)
}

fn execute(lua: &mut Lua, closure: &StashedClosure) -> i64 {
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
    lua.execute::<i64>(&executor).unwrap()
}

#[test]
fn production_read_entry_removes_real_helper_calls_and_retires_with_runtime() {
    let (mut lua, closure) = setup(8 << 20);
    for _ in 0..2 {
        assert_eq!(execute(&mut lua, &closure), 12507500);
    }
    let before = lua.jit_stats();
    assert_eq!(execute(&mut lua, &closure), 12507500);
    let after = lua.jit_stats();
    assert!(after.native_instructions > before.native_instructions);
    assert_eq!(after.native_table_reads - before.native_table_reads, 5000);
    assert_eq!(
        after.native_upvalue_reads - before.native_upvalue_reads,
        5000
    );
    assert_eq!(after.helper_calls, before.helper_calls);
    assert_eq!(after.helper_instructions, before.helper_instructions);
    assert_eq!(after.compilation_requests, before.compilation_requests);
    lua.set_jit_config(JitConfig::default()).unwrap();
    assert_eq!(lua.jit_stats().code_bytes, 0);
    assert_eq!(execute(&mut lua, &closure), 12507500);
}

#[test]
fn production_scope_releases_table_borrow_and_samples_each_callback_rebinding() {
    let (mut lua, closure) = setup(8 << 20);
    let calls = Rc::new(Cell::new(0));
    lua.enter(|ctx| {
        let count = calls.clone();
        let first = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            count.set(count.get() + 1);
            let value: i64 = stack.consume(ctx)?;
            ctx.set_global("host_increment", ctx.get_global::<Value>("second")?);
            stack.replace(ctx, value + 1);
            Ok(CallbackReturn::Return)
        });
        let count = calls.clone();
        let second = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            count.set(count.get() + 1);
            let value: i64 = stack.consume(ctx)?;
            ctx.set_global("host_increment", ctx.get_global::<Value>("first")?);
            stack.replace(ctx, value + 2);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("first", first);
        ctx.set_global("second", second);
        ctx.set_global("host_increment", first);
    });
    for _ in 0..3 {
        assert_eq!(execute(&mut lua, &closure), 12510000);
    }
    assert_eq!(calls.get(), 15000);
    assert!(lua.jit_stats().native_instructions > 0);
}

#[test]
fn production_scope_falls_back_to_missing_key_metamethods() {
    let (mut lua, closure) = setup(8 << 20);
    let calls = Rc::new(Cell::new(0));
    lua.enter(|ctx| {
        ctx.set_global("target", ctx.get_global::<Value>("host_increment").unwrap());
        ctx.set_global("host_increment", Value::Nil);
        let count = calls.clone();
        let metatable = Table::new(&ctx);
        metatable
            .set(
                ctx,
                "__index",
                Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                    count.set(count.get() + 1);
                    stack.replace(ctx, ctx.get_global::<Value>("target")?);
                    Ok(CallbackReturn::Return)
                }),
            )
            .unwrap();
        ctx.globals().set_metatable(ctx, Some(metatable));
    });
    for _ in 0..3 {
        assert_eq!(execute(&mut lua, &closure), 12507500);
    }
    assert_eq!(calls.get(), 15000);
    assert!(lua.jit_stats().native_instructions > 0);
    assert!(lua.jit_stats().helper_declines > 0);
}

#[test]
fn optional_read_image_refusal_preserves_execution_with_small_code_budget() {
    let (mut lua, closure) = setup(4096);
    for _ in 0..3 {
        assert_eq!(execute(&mut lua, &closure), 12507500);
    }
    let stats = lua.jit_stats();
    assert!(stats.code_bytes <= 4096);
    if stats.code_bytes != 0 {
        assert!(stats.native_instructions > 0);
        assert!(stats.helper_calls > 5000);
    }
}

#[test]
fn production_scope_preserves_interrupted_fuel_slices_and_collection() {
    for initial in [-1, 0, 1, 63, 64, 65, 65536, i32::MAX] {
        let mut states = [setup(8 << 20).0, setup(8 << 20).0];
        let executors = states.each_mut().map(|lua| {
            let mut config = lua.jit_config();
            config.hot_threshold = 1;
            lua.set_jit_config(config).unwrap();
            lua.enter(|ctx| {
                let closure = Closure::load(
                    ctx,
                    None,
                    &b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum"[..],
                )
                .unwrap();
                ctx.stash(Executor::start(ctx, closure.into(), ()))
            })
        });
        states[0].set_jit_config(JitConfig::default()).unwrap();
        let mut finished = false;
        for turn in 0..4096 {
            states[1].prepare_jit().unwrap();
            let budget = if turn == 0 {
                initial
            } else {
                [-1, 0, 1, 63, 64, 65][turn % 6]
            };
            let reports = std::array::from_fn::<_, 2, _>(|index| {
                states[index].enter(|ctx| {
                    let mut fuel = Fuel::with(budget);
                    if turn % 7 == 3 {
                        fuel.interrupt();
                    }
                    let executor = ctx.fetch(&executors[index]);
                    (
                        executor.step(ctx, &mut fuel).unwrap(),
                        executor.mode(),
                        fuel.remaining(),
                        fuel.is_interrupted(),
                    )
                })
            });
            assert_eq!(reports[0], reports[1], "initial={initial} turn={turn}");
            assert_eq!(
                states[0].jit_stats().total_dispatches,
                states[1].jit_stats().total_dispatches,
                "initial={initial} turn={turn}"
            );
            if reports[1].0 {
                finished = true;
                break;
            }
            if turn % 11 == 0 {
                for lua in &mut states {
                    lua.gc_collect();
                }
            }
        }
        assert!(finished);
        assert!(states[1].jit_stats().native_table_reads > 0);
        for (lua, executor) in states.iter_mut().zip(&executors) {
            assert_eq!(lua.execute::<i64>(executor).unwrap(), 3320);
            lua.clear_jit_cache();
            let stats = lua.jit_stats();
            assert_eq!(stats.code_bytes, 0);
            assert_eq!(stats.code_requested_bytes, 0);
            assert_eq!(stats.snapshot_bytes, 0);
            assert_eq!(
                stats.accounted_jit_bytes,
                stats.metadata_bytes + stats.bootstrap_bytes
            );
        }
    }
}
