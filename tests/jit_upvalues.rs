#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{
    Callback, CallbackReturn, Closure, Executor, ExternError, Fuel, Function, JitConfig, JitMode,
    Lua, StashedExecutor,
};

fn state(native: bool) -> Lua {
    let mut lua = Lua::core();
    lua.load_debug();
    if native {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
    }
    lua
}

fn source(lua: &mut Lua, script: &[u8]) -> Result<StashedExecutor, ExternError> {
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("scalar-upvalues"), script)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.prepare_jit().unwrap();
    Ok(executor)
}

#[test]
fn current_frame_aliases_read_scratch_write_through_and_decline_stale_tables(
) -> Result<(), ExternError> {
    let cases = [
        ("0", "x=41 alias=alias+1 return x", 42, 1, false),
        ("0", "alias={answer=42} return x.answer", 42, 1, false),
        ("{answer=42}", "x=false return alias.answer", -1, 0, false),
        (
            "{answer=0}",
            "x=false alias.answer=42 return 0",
            -1,
            0,
            false,
        ),
        ("0", "x=41 alias=alias+1 return read", 42, 1, false),
        ("0", "x=41 alias=alias+1 return read()", 42, 1, false),
        (
            "0",
            "x=41 alias=alias+1 coroutine.yield(x) x=alias+1 alias=x+1 return x",
            44,
            2,
            true,
        ),
    ];
    for (initial, body, expected, writes, yielding) in cases {
        let call = if yielding {
            "local co=coroutine.create(f) local ok,result=coroutine.resume(co) assert(ok and result==42) ok,result=coroutine.resume(co)"
        } else {
            "local ok,result=pcall(f)"
        };
        let script = format!(
            r#"
            local alias=0
            local f
            f=function()
                local x={initial}
                local read=function() return x end
                local joined=false
                for i=1,10 do
                    local name,value=debug.getupvalue(f,i)
                    if name and value==0 then
                        debug.upvaluejoin(f,i,read,1)
                        joined=true
                        break
                    end
                end
                assert(joined)
                {body}
            end
            {call}
            if ok and type(result)=='function' then return result() end
            return ok and result or -1
        "#
        );
        let mut reference = state(false);
        let mut native = state(true);
        let left = source(&mut reference, script.as_bytes())?;
        let right = source(&mut native, script.as_bytes())?;
        let mut finished = false;
        for _ in 0..1000 {
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
            let wanted = step(&mut reference, &left);
            assert_eq!(step(&mut native, &right), wanted, "{body}");
            reference.gc_collect();
            native.gc_collect();
            if wanted.0 {
                finished = true;
                break;
            }
        }
        assert!(finished, "{body}");
        assert_eq!(reference.execute::<i64>(&left)?, expected, "{body}");
        assert_eq!(native.execute::<i64>(&right)?, expected, "{body}");
        let stats = native.jit_stats();
        assert!(stats.native_instructions > 0);
        if writes > 0 {
            assert_eq!(stats.native_upvalue_writes, writes, "{body}");
        } else {
            assert!(stats.helper_declines > 0, "{body}");
        }
    }
    Ok(())
}

#[test]
fn closed_and_open_cells_materialize_between_slices_and_gc() -> Result<(), ExternError> {
    let scripts: &[&[u8]] = &[
        b"local n=0 local function add(x) n=n+x return n end local result for i=1,100 do result=add(i) end return result+n",
        b"local function make() local n=0 return function(x) n=n+x return n end end local add=make() local result for i=1,100 do result=add(i) end return result",
    ];
    for (index, script) in scripts.iter().enumerate() {
        let mut reference = state(false);
        let mut native = state(true);
        let left = source(&mut reference, script)?;
        let right = source(&mut native, script)?;
        let mut finished = false;
        for _ in 0..1000 {
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
            let expected = step(&mut reference, &left);
            assert_eq!(step(&mut native, &right), expected);
            reference.gc_collect();
            native.gc_collect();
            if expected.0 {
                finished = true;
                break;
            }
        }
        assert!(finished);
        let expected = if index == 0 { 10100 } else { 5050 };
        assert_eq!(reference.execute::<i64>(&left)?, expected);
        assert_eq!(native.execute::<i64>(&right)?, expected);
        let stats = native.jit_stats();
        assert_eq!(stats.native_upvalue_reads, 200);
        assert_eq!(stats.native_upvalue_writes, 100);
        assert!(stats.native_instructions >= 300);
    }
    Ok(())
}

#[test]
fn dense_cells_charge_each_operation_and_flush_each_slice() -> Result<(), ExternError> {
    let script = b"local n=0 local function f(x) for i=1,200 do n=n+x end return n end return f(1)";
    let mut reference = state(false);
    let mut native = state(true);
    let left = source(&mut reference, script)?;
    let right = source(&mut native, script)?;
    let mut finished = false;
    for _ in 0..100 {
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
        assert_eq!(step(&mut native, &right), expected);
        let after = native.jit_stats();
        assert!(after.native_upvalue_reads - before.native_upvalue_reads <= 64);
        assert!(after.native_upvalue_writes - before.native_upvalue_writes <= 64);
        reference.gc_collect();
        native.gc_collect();
        if expected.0 {
            finished = true;
            break;
        }
    }
    assert!(finished);
    assert_eq!(reference.execute::<i64>(&left)?, 200);
    assert_eq!(native.execute::<i64>(&right)?, 200);
    let stats = native.jit_stats();
    assert_eq!(stats.native_upvalue_reads, 201);
    assert_eq!(stats.native_upvalue_writes, 200);
    Ok(())
}

#[test]
fn aliases_foreign_stacks_and_reference_values_keep_helper_semantics() -> Result<(), ExternError> {
    let scripts: &[(&[u8], i64)] = &[
        (b"local a,b=0,100 local function f(x) a=a+x b=b+1 return a+b end debug.upvaluejoin(f,2,f,1) local result for i=1,10 do result=f(i) end return result",130),
        (b"local function make() local a,b=0,100 return function(x) a=a+x b=b+1 return a+b end end local f=make() debug.upvaluejoin(f,2,f,1) local result for i=1,10 do result=f(i) end return result",130),
        (b"local co=coroutine.create(function() local n=0 local function f(x) n=n+x return n end coroutine.yield(f) return n end) local ok,f=coroutine.resume(co) assert(ok) local result for i=1,100 do result=f(i) end local ok,n=coroutine.resume(co) assert(ok) return result+n",10100),
        (b"local n=0 local function f(x) n=x return n end local t={} local got=f(t) return got==t and 42 or 0",42),
        (b"local n={} local function f() return n end return f()==n and 42 or 0",42),
    ];
    for &(script, expected) in scripts {
        for native in [false, true] {
            let mut lua = state(native);
            let executor = source(&mut lua, script)?;
            assert_eq!(lua.execute::<i64>(&executor)?, expected);
            if native {
                let stats = lua.jit_stats();
                assert!(stats.native_upvalue_reads > 0);
                assert!(stats.helper_instructions > 0);
            }
        }
    }
    Ok(())
}

#[test]
fn writes_commit_before_guard_and_debug_rebinding() -> Result<(), ExternError> {
    let scripts: &[(&[u8], i64)] = &[
        (b"local n=0 local function f(x) n=n+x return n end assert(f(1)==1) debug.setupvalue(f,1,20) assert(f(21)==41) return n",41),
        (b"local n=0 local function f(x) n=x return n+1 end local ok=pcall(f,false) assert(not ok) assert(n==false) assert(f(41)==42) return n",41),
        (b"local n=0 local function f(x) n=x return n end assert(f(nil)==nil) assert(n==nil) assert(f(false)==false) assert(n==false) assert(f(1.5)==1.5) assert(n==1.5) return f(42)",42),
    ];
    for &(script, expected) in scripts {
        for native in [false, true] {
            let mut lua = state(native);
            let executor = source(&mut lua, script)?;
            assert_eq!(lua.execute::<i64>(&executor)?, expected);
            if native {
                assert!(lua.jit_stats().native_upvalue_writes >= 2);
            }
        }
    }
    Ok(())
}

#[test]
fn rust_reentry_observes_shared_open_and_closed_cell_writes() -> Result<(), ExternError> {
    let scripts: &[&[u8]] = &[
        b"local n=0 local function set(x) n=x end local function read() return n end for i=1,50 do set(i) assert(observe(read)==i) end return n",
        b"local function make() local n=0 return function(x) n=x end,function() return n end end local set,read=make() for i=1,50 do set(i) assert(observe(read)==i) end return observe(read)",
        b"local n=0 local function change() n=n+1 return n end for i=1,50 do assert(observe(change)==i) end return n",
        b"local function make() local n=0 return function() n=n+1 return n end end local change=make() local result for i=1,50 do result=observe(change) assert(result==i) end return result",
    ];
    for script in scripts {
        for native in [false, true] {
            let mut lua = state(native);
            lua.enter(|ctx| {
                ctx.set_global(
                    "observe",
                    Callback::from_fn(&ctx, |ctx, _, mut stack| {
                        let function: Function = stack.consume(ctx)?;
                        let executor = Executor::start(ctx, function, ());
                        while !executor.step(ctx, &mut Fuel::with(64)).unwrap() {}
                        let value = executor.take_result::<i64>(ctx).unwrap()?;
                        stack.replace(ctx, value);
                        Ok(CallbackReturn::Return)
                    }),
                );
            });
            let executor = source(&mut lua, script)?;
            assert_eq!(lua.execute::<i64>(&executor)?, 50);
            if native {
                let stats = lua.jit_stats();
                assert_eq!(stats.native_upvalue_writes, 50);
                assert!(stats.native_upvalue_reads >= 50);
            }
        }
    }
    Ok(())
}

#[test]
fn eight_and_nine_distinct_cells_preserve_all_reads_and_writes() -> Result<(), ExternError> {
    for count in [8, 9] {
        let names: Vec<_> = (0..count).map(|i| format!("n{i}")).collect();
        let bindings = names
            .iter()
            .map(|name| format!("local {name}=0 "))
            .collect::<String>();
        let writes = names
            .iter()
            .map(|name| format!("{name}={name}+x "))
            .collect::<String>();
        let sum = names.join("+");
        let script = format!("{bindings} local function add(x) {writes} return {sum} end local result for i=1,100 do result=add(1) end return result");
        for native in [false, true] {
            let mut lua = state(native);
            let executor = source(&mut lua, script.as_bytes())?;
            assert_eq!(lua.execute::<i64>(&executor)?, count * 100);
            if native {
                let stats = lua.jit_stats();
                assert_eq!(stats.native_upvalue_reads, count as u64 * 200);
                assert_eq!(stats.native_upvalue_writes, count as u64 * 100);
            }
        }
    }
    Ok(())
}
