#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExternError, Fuel, Function, JitConfig, JitMode,
    Lua, StashedExecutor, Table, UserData, Value,
};

fn state(native: bool) -> Lua {
    let mut lua = Lua::core();
    if native {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..JitConfig::default()
        })
        .unwrap();
    }
    lua
}

fn source(lua: &mut Lua, source: &[u8]) -> Result<StashedExecutor, ExternError> {
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("heap-test"), source)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.prepare_jit().unwrap();
    Ok(executor)
}

#[test]
fn helper_dense_slices_keep_local_counts_bounded_and_cumulative_totals_wide(
) -> Result<(), ExternError> {
    let script = format!("local t={{}} {} return 42", "t[1]=42 ".repeat(256));
    let mut reference = Lua::empty();
    let mut native = Lua::empty();
    native
        .set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
    let left = source(&mut reference, script.as_bytes())?;
    let right = source(&mut native, script.as_bytes())?;
    let mut finished = false;
    let mut full_helper_slice = false;
    for slice in 0..100 {
        let step = |lua: &mut Lua, executor: &StashedExecutor| {
            lua.enter(|ctx| {
                let executor = ctx.fetch(executor);
                let mut fuel = Fuel::empty();
                (
                    executor.step(ctx, &mut fuel).unwrap(),
                    executor.mode(),
                    fuel.remaining(),
                )
            })
        };
        let before = native.jit_stats();
        let expected = step(&mut reference, &left);
        let actual = step(&mut native, &right);
        assert_eq!(actual, expected, "slice {slice}");
        let after = native.jit_stats();
        assert!(after.helper_calls - before.helper_calls <= 64);
        full_helper_slice |= after.helper_calls - before.helper_calls == 64;
        if slice == 0 {
            assert_eq!(after.native_instructions - before.native_instructions, 64);
        }
        reference.gc_collect();
        native.gc_collect();
        if actual.0 {
            finished = true;
            break;
        }
    }
    assert!(finished);
    assert!(full_helper_slice);
    assert_eq!(reference.execute::<i64>(&left)?, 42);
    assert_eq!(native.execute::<i64>(&right)?, 42);
    let totals = native.jit_stats();
    assert_eq!(totals.native_allocations, 1);
    assert_eq!(totals.native_table_writes, 256);
    assert!(totals.helper_calls > 255);
    assert_eq!(totals.helper_calls, totals.helper_instructions);
    assert_eq!(totals.helper_declines, 0);
    Ok(())
}

#[test]
fn native_heap_roots_and_barriers_match_reference_under_every_slice_gc() -> Result<(), ExternError>
{
    let script = b"local t={} local alias=t for i=1,1000 do t[i]={n=i} end local sum=0 for i=1,1000 do sum=sum+t[i].n end return sum,alias==t";
    let mut reference = state(false);
    let mut native = state(true);
    let left = source(&mut reference, script)?;
    let right = source(&mut native, script)?;
    for slice in 0..10000 {
        let step = |lua: &mut Lua, executor: &StashedExecutor| {
            lua.enter(|ctx| {
                let executor = ctx.fetch(executor);
                let mut fuel = Fuel::empty();
                (
                    executor.step(ctx, &mut fuel).unwrap(),
                    executor.mode(),
                    fuel.remaining(),
                )
            })
        };
        let a = step(&mut reference, &left);
        let b = step(&mut native, &right);
        assert_eq!(a, b, "slice {slice}");
        reference.gc_collect();
        native.gc_collect();
        if a.0 {
            break;
        }
        assert!(slice < 9999);
    }
    assert_eq!(reference.execute::<(i64, bool)>(&left)?, (500500, true));
    assert_eq!(native.execute::<(i64, bool)>(&right)?, (500500, true));
    let stats = native.jit_stats();
    assert!(stats.native_allocations >= 1001);
    assert!(stats.native_table_writes >= 2000);
    assert!(stats.native_table_reads >= 2000);
    assert!(stats.helper_instructions > 5000);
    Ok(())
}

#[test]
fn native_open_and_closed_upvalues_use_current_canonical_state() -> Result<(), ExternError> {
    let scripts: &[(&[u8],i64)] = &[
        (b"local n=0 local function add(x) n=n+x end for i=1,200 do add(i) end return n",20100),
        (b"local function make() local n=0 return function(x) n=n+x return n end end local f=make() local result for i=1,200 do result=f(i) end return result",20100),
        (b"local function make() local t={} return function(x) t.value=x return t.value end end local f=make() local result for i=1,200 do result=f(i) end return result",200),
    ];
    for &(script, expected) in scripts {
        let mut lua = state(true);
        let executor = source(&mut lua, script)?;
        assert_eq!(lua.execute::<i64>(&executor)?, expected);
        let stats = lua.jit_stats();
        assert!(stats.native_upvalue_reads >= 200);
        if expected == 20100 {
            assert!(stats.native_upvalue_writes >= 200);
        } else {
            assert!(stats.native_table_writes >= 200);
        }
    }
    Ok(())
}

#[test]
fn rust_mutation_and_write_interception_are_freshly_guarded() -> Result<(), ExternError> {
    for native in [false, true] {
        let mut lua = state(native);
        lua.enter(|ctx| {
            let mutate = Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let table: Table = stack.consume(ctx)?;
                table.set_intercept_all_writes(&ctx, true);
                Ok(CallbackReturn::Return)
            });
            ctx.set_global("intercept", mutate);
        });
        let executor=source(&mut lua,b"local writes=0 local mt={__newindex=function(t,k,v) writes=writes+1 end} local t={x=1} setmetatable(t,mt) t.x=2 intercept(t) t.x=3 t.x=4 return writes*10+t.x")?;
        assert_eq!(lua.execute::<i64>(&executor)?, 22);
        if native {
            let stats = lua.jit_stats();
            assert!(stats.native_table_reads > 0);
            assert!(stats.native_table_writes > 0);
            assert!(stats.helper_declines >= 2);
        }
    }
    Ok(())
}

#[test]
fn weak_table_reads_and_writes_preserve_collection_semantics() -> Result<(), ExternError> {
    let script=b"local weak=setmetatable({},{__mode='v'}) local live={} weak[1]=live assert(weak[1]==live) do local dead={} weak[2]=dead end collectgarbage('collect') coroutine.yield() assert(weak[1]==live) assert(weak[2]==nil) local key={} local wk=setmetatable({},{__mode='k'}) wk[key]=42 assert(wk[key]==42) key=nil collectgarbage('collect') coroutine.yield() return next(wk)==nil and 42 or 0";
    for native in [false, true] {
        let mut lua = state(native);
        let executor = source(&mut lua, script)?;
        for _ in 0..2 {
            lua.finish(&executor).unwrap();
            lua.try_enter(|ctx| {
                let executor = ctx.fetch(&executor);
                executor.take_result::<()>(ctx)??;
                executor.resume(ctx, ()).unwrap();
                Ok(())
            })?;
            lua.gc_collect();
        }
        assert_eq!(lua.execute::<i64>(&executor)?, 42);
        if native {
            let stats = lua.jit_stats();
            assert!(stats.native_table_reads >= 3);
            assert!(stats.native_table_writes >= 3);
        }
    }
    Ok(())
}

#[test]
fn interleaved_executors_keep_replaced_table_and_userdata_roots_fresh() -> Result<(), ExternError> {
    fn mutate(lua: &mut Lua, tick: i64) {
        lua.enter(|ctx| {
            let shared: Table = ctx.get_global("shared").unwrap();
            match shared.get_value(ctx, "item") {
                Value::Table(table) => {
                    table.set_field(ctx, "value", tick % 17 + 1);
                }
                Value::UserData(data) => {
                    let proxy: Table = data.metatable().unwrap().get(ctx, "__index").unwrap();
                    proxy.set_field(ctx, "value", tick % 17 + 1);
                }
                Value::Nil => {}
                _ => unreachable!(),
            }
            let item = Table::new(&ctx);
            item.set_field(ctx, "id", tick);
            item.set_field(ctx, "value", tick % 11 + 1);
            if tick % 3 == 0 {
                let data = UserData::new_static(&ctx, tick);
                let mt = Table::new(&ctx);
                mt.set_field(ctx, "__index", item);
                data.set_metatable(ctx, Some(mt));
                shared.set_field(ctx, "item", data);
            } else {
                shared.set_field(ctx, "item", item);
            }
            shared.set(ctx, tick % 128 + 1, tick).unwrap();
            shared.set(ctx, (tick + 64) % 128 + 1, Value::Nil).unwrap();
        });
    }
    fn snapshot(lua: &mut Lua) -> (i64, &'static str, i64, i64) {
        lua.enter(|ctx| {
            let shared: Table = ctx.get_global("shared").unwrap();
            let progress = shared.get(ctx, "progress").unwrap();
            let (kind, id, value) = match shared.get_value(ctx, "last") {
                Value::Nil => ("nil", -1, 0),
                Value::Table(table) => (
                    "table",
                    table.get(ctx, "id").unwrap(),
                    table.get(ctx, "value").unwrap(),
                ),
                Value::UserData(data) => {
                    let proxy: Table = data.metatable().unwrap().get(ctx, "__index").unwrap();
                    (
                        "userdata",
                        *data.downcast_static::<i64>().unwrap(),
                        proxy.get(ctx, "value").unwrap(),
                    )
                }
                _ => unreachable!(),
            };
            (progress, kind, id, value)
        })
    }
    let mut reference = state(false);
    let mut native = state(true);
    for lua in [&mut reference, &mut native] {
        lua.enter(|ctx| {
            let shared = Table::new(&ctx);
            shared.set_field(ctx, "progress", 0);
            ctx.set_global("shared", shared);
        });
        mutate(lua, 1);
    }
    let scripts = [
        b"local sum=0 for i=1,48 do local object=shared.item sum=sum+object.value shared.last=object shared.progress=i end return sum".as_slice(),
        b"local sum=100 for i=1,37 do local object=shared.item sum=sum+object.value shared.last=object shared.progress=i end return sum".as_slice(),
    ];
    let mut left = Vec::new();
    let mut right = Vec::new();
    for script in scripts {
        left.push(source(&mut reference, script)?);
        right.push(source(&mut native, script)?);
    }
    let mut done = [false; 2];
    let mut completed = false;
    for tick in 0..4000 {
        let index = if done[tick % 2] {
            1 - tick % 2
        } else {
            tick % 2
        };
        let budget = [1, 3, 7, 64][tick % 4];
        let step = |lua: &mut Lua, executor: &StashedExecutor| {
            lua.enter(|ctx| {
                let executor = ctx.fetch(executor);
                let mut fuel = Fuel::with(budget);
                let result = executor.step(ctx, &mut fuel).unwrap();
                (result, executor.mode(), fuel.remaining())
            })
        };
        let expected = step(&mut reference, &left[index]);
        let actual = step(&mut native, &right[index]);
        assert_eq!(actual, expected, "executor {index}, tick {tick}");
        assert_eq!(snapshot(&mut native), snapshot(&mut reference));
        done[index] = actual.0;
        for lua in [&mut reference, &mut native] {
            mutate(lua, tick as i64 + 2);
            lua.gc_collect();
        }
        assert_eq!(snapshot(&mut native), snapshot(&mut reference));
        if done.iter().all(|done| *done) {
            completed = true;
            break;
        }
    }
    assert!(completed);
    for index in 0..2 {
        let expected = reference.execute::<i64>(&left[index])?;
        assert!(expected > 0);
        assert_eq!(native.execute::<i64>(&right[index])?, expected);
    }
    let stats = native.jit_stats();
    assert!(stats.native_instructions > 0);
    assert!(stats.native_table_reads >= 85);
    assert!(stats.native_table_writes >= 85);
    assert!(stats.helper_declines > 0);
    assert_eq!(reference.jit_stats().native_instructions, 0);
    Ok(())
}

#[test]
fn rust_weak_mode_mutation_and_reattachment_preserve_native_collection() -> Result<(), ExternError>
{
    fn run(lua: &mut Lua, closure: &luna::StashedClosure) -> Result<i64, ExternError> {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
        let mut completed = false;
        for tick in 0..1000 {
            let done = lua.enter(|ctx| {
                ctx.fetch(&executor)
                    .step(ctx, &mut Fuel::with([1, 7, 64][tick % 3]))
                    .unwrap()
            });
            lua.gc_collect();
            if done {
                completed = true;
                break;
            }
        }
        assert!(completed);
        lua.execute(&executor)
    }
    for native in [false, true] {
        let mut lua = state(native);
        lua.enter(|ctx| {
            let cache = Table::new(&ctx);
            let item = Table::new(&ctx);
            item.set_field(ctx, "marker", 42);
            cache.set(ctx, 1, item).unwrap();
            let mt = Table::new(&ctx);
            cache.set_metatable(ctx, Some(mt));
            ctx.set_global("cache", cache);
            ctx.set_global("mode", mt);
        });
        let (getter, setter) = lua.try_enter(|ctx| {
            let getter = Closure::load(ctx, None, b"local object=cache[1] if object then return object.marker end return 0")?;
            let setter = Closure::load(ctx, None, b"local object={marker=71} cache[1]=object local result=cache[1].marker object=nil return result")?;
            Ok((ctx.stash(getter), ctx.stash(setter)))
        })?;
        lua.prepare_jit().unwrap();
        assert_eq!(run(&mut lua, &getter)?, 42);
        lua.enter(|ctx| {
            let mt: Table = ctx.get_global("mode").unwrap();
            mt.set_field(ctx, "__mode", "v");
        });
        lua.gc_collect();
        lua.gc_collect();
        let before = lua.jit_stats();
        assert_eq!(run(&mut lua, &getter)?, 42);
        if native {
            assert!(lua.jit_stats().native_table_reads > before.native_table_reads);
        }
        lua.enter(|ctx| {
            let cache: Table = ctx.get_global("cache").unwrap();
            let mt: Table = ctx.get_global("mode").unwrap();
            cache.set_metatable(ctx, Some(mt));
        });
        lua.gc_collect();
        lua.gc_collect();
        let before = lua.jit_stats();
        assert_eq!(run(&mut lua, &getter)?, 0);
        if native {
            assert!(lua.jit_stats().native_table_reads > before.native_table_reads);
        }
        let before = lua.jit_stats();
        assert_eq!(run(&mut lua, &setter)?, 71);
        if native {
            let after = lua.jit_stats();
            assert_eq!(after.native_allocations - before.native_allocations, 1);
            assert_eq!(after.native_table_writes - before.native_table_writes, 2);
            assert!(after.native_table_reads >= before.native_table_reads + 2);
        }
        lua.gc_collect();
        lua.gc_collect();
        assert_eq!(run(&mut lua, &getter)?, 0);
        assert_eq!(lua.jit_stats().native_instructions > 0, native);
    }
    Ok(())
}

#[test]
fn weak_key_ephemeron_registration_follows_reattached_modes() -> Result<(), ExternError> {
    fn run(lua: &mut Lua, closure: &luna::StashedClosure) -> Result<i64, ExternError> {
        let executor =
            lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ())));
        lua.execute(&executor)
    }
    fn collect(lua: &mut Lua) {
        lua.gc_collect();
        lua.gc_collect();
    }
    fn attach(lua: &mut Lua, mode: &str) {
        lua.enter(|ctx| {
            let cache: Table = ctx.get_global("cache").unwrap();
            let mt: Table = ctx.get_global("mode").unwrap();
            mt.set_field(ctx, "__mode", ctx.intern(mode.as_bytes()));
            cache.set_metatable(ctx, Some(mt));
        });
    }
    for native in [false, true] {
        let mut lua = state(native);
        lua.enter(|ctx| {
            let cache = Table::new(&ctx);
            let mt = Table::new(&ctx);
            mt.set_field(ctx, "__mode", "k");
            cache.set_metatable(ctx, Some(mt));
            ctx.set_global("cache", cache);
            ctx.set_global("mode", mt);
            ctx.set_global("key", Table::new(&ctx));
        });
        let (getter, setter) = lua.try_enter(|ctx| {
            let getter = Closure::load(
                ctx,
                None,
                b"local value=cache[key] if value then return value.marker end return 0",
            )?;
            let setter = Closure::load(
                ctx,
                None,
                b"local value={marker=113} cache[key]=value return cache[key].marker",
            )?;
            Ok((ctx.stash(getter), ctx.stash(setter)))
        })?;
        lua.prepare_jit().unwrap();
        assert_eq!(run(&mut lua, &setter)?, 113);
        collect(&mut lua);
        assert_eq!(run(&mut lua, &getter)?, 113);
        lua.enter(|ctx| {
            let mt: Table = ctx.get_global("mode").unwrap();
            mt.set_field(ctx, "__mode", "kv");
        });
        collect(&mut lua);
        assert_eq!(run(&mut lua, &getter)?, 113);
        attach(&mut lua, "kv");
        collect(&mut lua);
        assert_eq!(run(&mut lua, &getter)?, 0);
        for weak_mode in ["kv", "v", "kv", "v"] {
            attach(&mut lua, "k");
            assert_eq!(run(&mut lua, &setter)?, 113);
            collect(&mut lua);
            assert_eq!(run(&mut lua, &getter)?, 113);
            attach(&mut lua, weak_mode);
            collect(&mut lua);
            assert_eq!(run(&mut lua, &getter)?, 0);
        }
        let stats = lua.jit_stats();
        assert_eq!(stats.native_instructions > 0, native);
        if native {
            assert_eq!(stats.native_allocations, 5);
            assert_eq!(stats.native_table_writes, 10);
            assert!(stats.native_table_reads >= 12);
        }
    }
    Ok(())
}

#[test]
fn readonly_and_invalid_key_failures_remain_typed_interpreter_errors() -> Result<(), ExternError> {
    for script in [
        b"local x=target[1] target[1]=x+1".as_slice(),
        b"local x=target[1] local t={} t[0/0]=x",
        b"local x=target[1] local t={} t[nil]=x",
    ] {
        let mut errors = Vec::new();
        for native in [false, true] {
            let mut lua = state(native);
            lua.enter(|ctx| {
                let table = Table::new(&ctx);
                table.set(ctx, 1, 40).unwrap();
                table.set_readonly(&ctx, true);
                ctx.set_global("target", table);
            });
            let executor = source(&mut lua, script)?;
            let error = lua.execute::<()>(&executor).unwrap_err();
            assert!(error
                .root_cause()
                .downcast_ref::<luna::table::InvalidTableKey>()
                .is_some());
            errors.push(error.to_string());
            if native {
                assert!(lua.jit_stats().native_table_reads >= 2);
                assert!(lua.jit_stats().helper_declines > 0);
            }
        }
        assert_eq!(errors[0], errors[1]);
    }
    Ok(())
}

#[test]
fn helper_panics_return_through_native_code_before_rust_unwinding() -> Result<(), ExternError> {
    for native in [false, true] {
        let mut lua = state(native);
        lua.enter(|ctx| {
            let table = Table::new(&ctx);
            table.set(ctx, "x", 42).unwrap();
            ctx.set_global("target", table);
        });
        let executor = source(&mut lua, b"local alias=target return alias.x")?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lua.enter(|ctx| {
                let table: Table = ctx.get_global("target").unwrap();
                let _borrow = table.into_inner().borrow_mut(&ctx);
                ctx.fetch(&executor)
                    .step(ctx, &mut Fuel::with(100))
                    .unwrap();
            });
        }));
        assert!(result.is_err());
        lua.enter(|ctx| {
            ctx.fetch(&executor).stop(&ctx);
        });
        lua.gc_collect();
        let next = source(&mut lua, b"local x=40 return x+2")?;
        assert_eq!(lua.execute::<i64>(&next)?, 42);
    }
    Ok(())
}

#[test]
fn helper_panic_materializes_pending_scalars_before_host_inspection() -> Result<(), ExternError> {
    for native in [false, true] {
        let mut lua = state(native);
        let (executor, thread) = lua.try_enter(|ctx| {
            let target = Table::new(&ctx);
            target.set(ctx, "x", 2).unwrap();
            ctx.set_global("target", target);
            let closure = Closure::load(
                ctx,
                None,
                b"local n=1 n=n+39 local t={} t.value=n+2 local alias=target return alias.x+n",
            )?;
            let thread = luna::Thread::new(ctx);
            thread.start(ctx, closure.into(), ()).unwrap();
            let executor = Executor::run(&ctx, thread).unwrap();
            Ok((ctx.stash(executor), ctx.stash(thread)))
        })?;
        lua.prepare_jit().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lua.enter(|ctx| {
                let target: Table = ctx.get_global("target").unwrap();
                let _borrow = target.into_inner().borrow_mut(&ctx);
                ctx.fetch(&executor)
                    .step(ctx, &mut Fuel::with(100))
                    .unwrap();
            });
        }));
        assert!(result.is_err());
        lua.enter(|ctx| {
            let thread = ctx.fetch(&thread).into_inner().borrow();
            let stack = thread.stack().borrow();
            assert!(stack.iter().any(|value| matches!(value, luna::Value::Integer(40))));
            assert!(stack.iter().any(|value| matches!(value, luna::Value::Table(table) if matches!(table.get_value(ctx, "value"), luna::Value::Integer(42)))));
            drop(stack);
            drop(thread);
            ctx.fetch(&executor).stop(&ctx);
        });
        if native {
            let stats = lua.jit_stats();
            assert_eq!(stats.native_entries, 1);
            assert!(stats.native_instructions > 0);
            assert!(stats.helper_instructions >= 2);
            assert!(stats.helper_calls > stats.helper_instructions);
        }
    }
    Ok(())
}

#[test]
fn native_resumption_observes_debug_local_and_upvalue_mutation() -> Result<(), ExternError> {
    let script=b"local n=1 local function f(x) return n+x end assert(f(2)==3) debug.setupvalue(f,1,40) local a=f(2) local function g(x) local n=1 debug.setlocal(1,2,40) return n+x end local b=g(2) local k=100 local function h(x) return k+x end debug.upvaluejoin(f,1,h,1) return a+b+f(2)";
    for native in [false, true] {
        let mut lua = state(native);
        lua.load_debug();
        let executor = source(&mut lua, script)?;
        assert_eq!(lua.execute::<i64>(&executor)?, 186);
        if native {
            assert!(lua.jit_stats().native_upvalue_reads >= 3);
            assert!(lua.jit_stats().native_instructions > 0);
        }
    }
    Ok(())
}

#[test]
fn native_finalizer_resurrection_preserves_heap_and_upvalue_effects() -> Result<(), ExternError> {
    let script=b"local ran=0 local saved local function make() local t=setmetatable({marker=40},{__gc=function(self) ran=ran+1 self.marker=self.marker+2 saved=self end}) return tostring(t) end make() for i=1,2000 do local t={} t[i]=i end collectgarbage('collect') collectgarbage('collect') assert(saved.marker==42) saved=nil for i=1,2000 do local t={} t[i]=i end collectgarbage('collect') collectgarbage('collect') return ran";
    for native in [false, true] {
        let mut lua = state(native);
        let executor = source(&mut lua, script)?;
        assert_eq!(lua.execute::<i64>(&executor)?, 1);
        if native {
            let stats = lua.jit_stats();
            assert!(stats.native_upvalue_writes >= 2);
            assert!(stats.native_table_reads > 0);
            assert!(stats.native_allocations >= 4000);
        }
    }
    Ok(())
}

#[test]
fn reentrant_callbacks_observe_materialized_native_heap_and_upvalues() -> Result<(), ExternError> {
    let mut lua = state(true);
    lua.enter(|ctx| {
        let nested = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let function: Function = stack.consume(ctx)?;
            let executor = Executor::start(ctx, function, ());
            while !executor.step(ctx, &mut Fuel::with(1000)).unwrap() {}
            let result = executor.take_result::<i64>(ctx).unwrap()?;
            stack.replace(ctx, result);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("nested", nested);
    });
    let executor=source(&mut lua,b"local t={} local n=40 local f=function() t.answer=n+2 return t.answer end local result=nested(f) return result")?;
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    let stats = lua.jit_stats();
    assert!(stats.native_table_writes > 0);
    assert!(stats.native_upvalue_reads > 0);
    Ok(())
}

#[test]
fn native_close_handlers_preserve_error_unwinding_and_capture_mutation() -> Result<(), ExternError>
{
    let script=b"local closed=0 local t={} local ok=pcall(function() local c <close> = setmetatable({n=42},{__close=function(self,err) t[1]=self.n closed=closed+1 end}) t[2]=10 error('expected') end) assert(not ok) return closed*100+t[1]+t[2]";
    for native in [false, true] {
        let mut lua = state(native);
        let executor = source(&mut lua, script)?;
        assert_eq!(lua.execute::<i64>(&executor)?, 152);
        if native {
            let stats = lua.jit_stats();
            assert!(stats.native_table_writes >= 2);
            assert!(stats.native_upvalue_writes >= 1);
        }
    }
    Ok(())
}

#[cfg(feature = "async")]
#[test]
fn native_heap_state_survives_foreign_await_and_resumption() -> Result<(), ExternError> {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let mut lua = state(true);
    lua.enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |ctx, _, _| {
            Ok(CallbackReturn::Sequence(luna::async_sequence(
                &ctx,
                |_, mut sequence| async move {
                    let result = sequence.await_future(std::future::ready(42i64)).await;
                    sequence.enter(|ctx, _, _, mut stack| stack.replace(ctx, result));
                    Ok(luna::SequenceReturn::Return)
                },
            )))
        });
        ctx.set_global("wait", callback);
    });
    let executor = source(
        &mut lua,
        b"local t={before=40} local result=wait() t.after=result+2 return t.before+t.after",
    )?;
    let result = {
        let mut future = std::pin::pin!(lua.execute_async::<i64>(&executor));
        let mut context = Context::from_waker(Waker::noop());
        let mut result = None;
        for _ in 0..1000 {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                result = Some(value);
                break;
            }
        }
        result.expect("ready foreign future did not complete")?
    };
    assert_eq!(result, 84);
    let stats = lua.jit_stats();
    assert!(stats.native_table_writes >= 2);
    assert!(stats.native_table_reads >= 2);
    Ok(())
}
