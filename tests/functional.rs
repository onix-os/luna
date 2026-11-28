//! A functional state must be pure: no ambient inputs, no shared mutation, no hidden state in
//! closures, and the same traversal order every run.

use luna::{
    compiler::CompileErrorKind, functional, Callback, CallbackReturn, Closure, CompilerError,
    Context, Error, Executor, ExternError, Fuel, Lua, RuntimeError, StashedExecutor, Value,
};

/// The README example, verbatim apart from the assertions.
#[test]
fn the_readme_example_runs() -> Result<(), ExternError> {
    let mut lua = Lua::functional();
    let (ex, env) = lua.try_enter(|ctx| {
        let env = functional::environment(ctx);
        let source = &b"name = 'luna' return { name = name }"[..];
        let closure = Closure::load_with_env(ctx, None, source, env)?;
        Ok((
            ctx.stash(Executor::start(ctx, closure.into(), ())),
            ctx.stash(env),
        ))
    })?;
    lua.finish(&ex).map_err(RuntimeError::new)?;
    lua.try_enter(|ctx| {
        let config: Value = ctx.fetch(&ex).take_result(ctx)??;
        functional::freeze(ctx, config);
        functional::freeze(ctx, ctx.fetch(&env).into());
        let Value::Table(table) = config else {
            panic!("config is a table");
        };
        assert!(table.is_readonly() && table.set(ctx, "name", "x").is_err());
        Ok(())
    })?;
    Ok(())
}

/// A chunk environment with a host `freeze` callback, the way a registering host would offer one.
fn start<'gc>(ctx: Context<'gc>, source: &str) -> Result<StashedExecutor, Error<'gc>> {
    let env = functional::environment(ctx);
    env.set(
        ctx,
        "freeze",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value = stack.get(0);
            functional::freeze(ctx, value);
            stack.replace(ctx, value);
            Ok(CallbackReturn::Return)
        }),
    )?;
    let closure = Closure::load_with_env(ctx, None, source.as_bytes(), env)?;
    Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
}

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = common::functional();
    let executor = lua.try_enter(|ctx| start(ctx, source))?;
    lua.execute::<bool>(&executor)
}

fn compile_error(source: &str) -> Option<CompileErrorKind> {
    let mut lua = common::functional();
    lua.enter(|ctx| match Closure::load(ctx, None, source.as_bytes()) {
        Err(CompilerError::Compilation(error)) => Some(error.kind),
        _ => None,
    })
}

#[test]
fn the_library_has_nothing_impure() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return io == nil and os == nil and debug == nil and package == nil
            and coroutine == nil and require == nil and print == nil
            and collectgarbage == nil and dofile == nil and loadfile == nil
            and math.random == nil and math.randomseed == nil
            and string.dump == nil and table.interceptall == nil
            and type(string.format) == "function" and type(table.concat) == "function"
    "#
    )?);
    Ok(())
}

#[test]
fn shared_globals_and_libraries_are_frozen() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return not pcall(rawset, _G, "leak", 1)
            and not pcall(function() string.upper = nil end)
            and not pcall(function() getmetatable("").__index = {} end)
            and string.upper("ok") == "OK"
    "#
    )?);
    Ok(())
}

#[test]
fn a_chunk_may_mutate_what_it_creates() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local squares = {}
        for i = 1, 5 do squares[#squares + 1] = i * i end
        table.insert(squares, 36)
        local total = 0
        for _, v in ipairs(squares) do total = total + v end
        chunk_global = total
        return total == 91 and chunk_global == 91
    "#
    )?);
    Ok(())
}

#[test]
fn assignment_to_a_captured_variable_does_not_compile() {
    assert!(matches!(
        compile_error("local n = 0 local function tick() n = n + 1 end"),
        Some(CompileErrorKind::AssignToCaptured)
    ));
    assert!(matches!(
        compile_error("local f local function g() function f() end end"),
        Some(CompileErrorKind::AssignToCaptured)
    ));
    assert!(matches!(
        compile_error("_ENV = {}"),
        Some(CompileErrorKind::AssignToCaptured)
    ));
}

/// A captured local must be effectively final, as for Java lambdas: otherwise the enclosing code
/// could rebind it after a closure was frozen, pointing the frozen closure at a fresh mutable value.
#[test]
fn a_captured_local_cannot_be_reassigned_anywhere() {
    for source in [
        "local t = {} local function f() return t end t = {}",
        "local n = 0 n = n + 1 local function read() return n end",
        "local x = 0 for i = 1, 3 do x = i local f = function() return x end end",
        "local function g(a) a = a + 1 return function() return a end end",
        "local f f = function() return f end",
    ] {
        assert!(
            matches!(
                compile_error(source),
                Some(CompileErrorKind::AssignToCaptured)
            ),
            "{source}"
        );
    }
    for source in [
        "local n = 0 n = n + 1 return n",
        "local n = 1 local function read() return n end return read()",
        "local function f(n) if n == 0 then return 0 end return f(n - 1) end return f(3)",
        "local fs = {} for i = 1, 3 do fs[i] = function() return i end end return fs",
        "local t = {} local function add(v) t[#t + 1] = v end add(1) return t",
        "local x = 1 do local x = 2 x = 3 end local function f() return x end return f()",
    ] {
        assert!(compile_error(source).is_none(), "{source}");
    }
}

#[test]
fn load_compiles_in_functional_mode_and_refuses_binary() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local fine = load("local x = 1 x = x + 1 return x")
        local captured, why = load("local n = 0 return function() n = n + 1 end")
        local binary = pcall(load, "\27Luna\0", "chunk", "b")
        return fine() == 2 and captured == nil and not binary
            and why:find("line 1: cannot assign to a captured variable", 1, true) ~= nil
    "#
    )?);
    let mut lua = common::functional();
    lua.enter(|ctx| {
        assert!(matches!(
            Closure::load(ctx, None, b"\x1bLuna\x00"),
            Err(CompilerError::FunctionalBinary)
        ));
    });
    Ok(())
}

#[test]
fn freezing_is_deep() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local cfg = {
            nested = { list = { 1, 2 } },
            object = setmetatable({}, { __index = { z = 1 } }),
        }
        freeze(cfg)
        return not pcall(function() cfg.nested.list[1] = 9 end)
            and not pcall(rawset, cfg.nested, "x", 1)
            and not pcall(table.insert, cfg.nested.list, 3)
            and not pcall(setmetatable, cfg.object, nil)
            and not pcall(function() getmetatable(cfg.object).__index.z = 2 end)
            and cfg.nested.list[2] == 2 and cfg.object.z == 1
    "#
    )?);
    Ok(())
}

#[test]
fn a_frozen_closure_cannot_change_what_it_captured() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local state = { n = 0 }
        local function bump() state.n = state.n + 1 return state.n end
        local before = bump()
        freeze(bump)
        return before == 1 and not pcall(bump) and state.n == 1
    "#
    )?);
    Ok(())
}

#[test]
fn pairs_visits_keys_in_canonical_order() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local a = {}
        a.zeta = 1 a[3] = true a.alpha = 2 a[1.5] = 0 a[true] = "b" a[-2] = 0 a[false] = 1
        local b = { [false] = 1, [true] = "b", [-2] = 0, alpha = 2, [3] = true, [1.5] = 0, zeta = 1 }
        local function order(t)
            local out = {}
            for k in pairs(t) do out[#out + 1] = tostring(k) end
            return table.concat(out, ",")
        end
        return order(a) == order(b)
            and order(a) == "false,true,-2,1.5,3,alpha,zeta"
            and order({ 10, 20, 30, x = 1, [0] = 0 }) == "0,1,2,3,x"
    "#
    )?);
    Ok(())
}

#[test]
fn next_follows_the_same_order() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = { b = 1, a = 2, [1] = "x" }
        local k1 = next(t)
        local k2 = next(t, k1)
        local k3 = next(t, k2)
        return k1 == 1 and k2 == "a" and k3 == "b" and next(t, k3) == nil and next({}) == nil
    "#
    )?);
    Ok(())
}

#[test]
fn reference_keys_cannot_be_iterated() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = { [{}] = 1 }
        return t[next] == nil and not pcall(pairs, t) and not pcall(next, t)
    "#
    )?);
    Ok(())
}

#[test]
fn reference_values_print_without_addresses() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local named = setmetatable({}, { __name = "Point" })
        return tostring({}) == "table" and tostring(tostring) == "function"
            and tostring(named) == "Point"
            and not pcall(string.format, "%p", {})
            and string.format("%s", {}) == "table"
    "#
    )?);
    Ok(())
}

#[test]
fn collector_dependent_metatables_are_refused() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return not pcall(setmetatable, {}, { __gc = function() end })
            and not pcall(setmetatable, {}, { __mode = "k" })
            and getmetatable(setmetatable({}, { __index = {} })) ~= nil
    "#
    )?);
    Ok(())
}

#[test]
fn chunk_globals_stay_in_their_environment() -> Result<(), ExternError> {
    let mut lua = common::functional();
    let (executor, env) = lua.try_enter(|ctx| {
        let env = functional::environment(ctx);
        let closure = Closure::load_with_env(ctx, None, b"answer = 42 return answer", env)?;
        Ok((
            ctx.stash(Executor::start(ctx, closure.into(), ())),
            ctx.stash(env),
        ))
    })?;
    assert_eq!(lua.execute::<i64>(&executor)?, 42);
    lua.enter(|ctx| {
        let env = ctx.fetch(&env);
        functional::freeze(ctx, env.into());
        assert!(ctx.globals().get_value(ctx, "answer").is_nil());
        assert!(env.set(ctx, "answer", 1).is_err());
        assert!(matches!(env.get_value(ctx, "answer"), Value::Integer(42)));
    });
    Ok(())
}

#[test]
fn ordinary_states_are_unchanged() -> Result<(), ExternError> {
    let mut lua = common::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            br#"
            local n = 0
            local function tick() n = n + 1 return n end
            tick()
            return tostring({}):find("^table: 0x") ~= nil and tick() == 2
        "#,
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}

fn eval_with_files(
    files: &'static [(&'static str, &'static str)],
    source: &str,
) -> (Result<bool, ExternError>, usize) {
    use std::{cell::Cell, rc::Rc};

    let loads = Rc::new(Cell::new(0));
    let counter = loads.clone();
    let mut lua = common::functional();
    let result = lua
        .try_enter(|ctx| {
            let env = functional::import_environment(ctx, move |name| {
                counter.set(counter.get() + 1);
                files
                    .iter()
                    .find(|(file, _)| *file == name)
                    .map(|(_, text)| text.as_bytes().to_vec())
                    .ok_or_else(|| "no such file".to_owned())
            });
            let closure = Closure::load_with_env(ctx, None, source.as_bytes(), env)?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .and_then(|executor| lua.execute::<bool>(&executor));
    (result, loads.get())
}

#[test]
fn import_evaluates_once_and_freezes_the_result() {
    let files = &[
        (
            "lib",
            "helper = 2 return { double = function(x) return x * helper end, list = { 1 } }",
        ),
        ("none", "return nil"),
    ];
    let (result, loads) = eval_with_files(
        files,
        r#"
        local a, b = import("lib"), import("lib")
        local none1, none2 = import("none"), import("none")
        return a == b and a.double(21) == 42 and helper == nil
            and not pcall(function() a.list[2] = 2 end)
            and not pcall(table.insert, a.list, 2)
            and none1 == nil and none2 == nil
    "#,
    );
    assert!(result.unwrap());
    assert_eq!(loads, 2);
}

#[test]
fn imported_chunks_can_import_and_cycles_are_refused() {
    let files = &[
        ("outer", "return import('inner') + 1"),
        ("inner", "return 41"),
        ("a", "return import('b')"),
        ("b", "return import('a')"),
        ("broken", "local x = 1 local function f() x = 2 end"),
    ];
    let (result, _) = eval_with_files(
        files,
        r#"
        local cycle_ok, cycle = pcall(import, "a")
        local missing_ok, missing = pcall(import, "missing")
        local broken_ok, broken = pcall(import, "broken")
        local again_ok, again = pcall(import, "broken")
        return import("outer") == 42
            and not cycle_ok and tostring(cycle):find("import cycle through 'a'", 1, true) ~= nil
            and not missing_ok and tostring(missing):find("no such file", 1, true) ~= nil
            and not broken_ok and tostring(broken):find("captured variable", 1, true) ~= nil
            and not again_ok and tostring(again):find("cycle", 1, true) == nil
            and not pcall(import, 7)
    "#,
    );
    assert!(result.unwrap());
}

#[test]
fn a_failed_import_can_be_retried() {
    let files = &[("fails", "error('boom')")];
    let (result, loads) = eval_with_files(
        files,
        r#"
        local first_ok, first = pcall(import, "fails")
        local second_ok, second = pcall(import, "fails")
        return not first_ok and not second_ok
            and tostring(first):find("boom", 1, true) ~= nil
            and tostring(second):find("boom", 1, true) ~= nil
    "#,
    );
    assert!(result.unwrap());
    assert_eq!(loads, 2);
}

/// `libm`'s `exp(1)` is one ulp above glibc's `2.7182818284590451`: the same on every target.
#[test]
fn math_uses_the_portable_library() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return string.format("%.17g", math.sin(1)) == "0.8414709848078965"
            and string.format("%.17g", math.exp(1)) == "2.7182818284590455"
            and math.log(8, 2) == 3 and math.log(1) == 0
            and math.atan(1, 1) * 4 == math.pi and math.atan(0) == 0
            and math.cos(0) == 1 and math.tan(0) == 0
            and math.asin(1) * 2 == math.pi and math.acos(1) == 0
    "#
    )?);
    Ok(())
}

/// x86 produces a negative NaN for `0/0` and ARM a positive one, so a sign-aware `-nan` would
/// make the same config print differently on different machines.
#[test]
fn every_nan_prints_the_same() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local a, b = 0/0, -(0/0)
        return tostring(a) == "nan" and tostring(b) == "nan"
            and ("" .. a) == "nan" and (b .. "") == "nan"
            and table.concat({ a, b }, ",") == "nan,nan"
            and string.format("%g %f %e %s", a, b, a, b) == "nan nan nan nan"
            and string.format("%q", a) == string.format("%q", b)
            and string.format("%a", a) == string.format("%a", b)
            and string.pack("d", a) == string.pack("d", b)
            and string.pack("f", a) == string.pack("f", b)
    "#
    )?);
    let mut lua = common::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return tostring(0/0) ~= tostring(-(0/0))")?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}

#[test]
fn impure_libraries_cannot_be_loaded_into_a_functional_state() {
    for load in [
        Lua::load_core as fn(&mut Lua),
        Lua::load_io,
        Lua::load_os,
        Lua::load_debug,
        Lua::load_package,
    ] {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut lua = Lua::functional();
            load(&mut lua);
        }))
        .expect_err("loading an impure library must panic");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or_default();
        assert!(message.contains("functional state"), "{message}");
    }
}

/// A configuration that never finishes is stopped by the host's fuel budget, and one that
/// allocates without bound by the memory ceiling, so evaluation cannot hang or exhaust the host.
#[test]
fn runaway_configs_are_stopped_by_fuel_and_memory() -> Result<(), ExternError> {
    let mut lua = common::functional();
    let executor = lua.try_enter(|ctx| start(ctx, "local n = 0 while true do n = n + 1 end"))?;
    let finished = lua.enter(|ctx| {
        let executor = ctx.fetch(&executor);
        (0..64).any(|_| executor.step(ctx, &mut Fuel::with(4096)).unwrap())
    });
    assert!(!finished);

    let mut lua = common::functional();
    let before = lua.total_memory();
    lua.set_memory_limit(Some(before + 4 * 1024 * 1024));
    let executor = lua.try_enter(|ctx| {
        start(
            ctx,
            "local t = {} for i = 1, 100000000 do t[i] = { i } end return true",
        )
    })?;
    lua.finish(&executor).map_err(RuntimeError::new)?;
    assert!(lua.total_memory() < before + 64 * 1024 * 1024);
    Ok(())
}

/// With the JIT compiling every prototype up front, native code must keep every functional
/// guarantee: frozen tables refuse native writes, including a freeze in the middle of a hot
/// loop, and iteration order and results stay those of the interpreter.
#[cfg(feature = "jit")]
#[test]
fn native_code_keeps_the_functional_guarantees() {
    use luna::{JitConfig, JitMode};

    let source = r#"
        local t = {}
        for i = 1, 300 do t[i] = i * 2 end
        local sum = 0
        for i = 1, 300 do sum = sum + t[i] end
        freeze(t)
        local before = pcall(function() for i = 1, 300 do t[i] = -i end end)
        local u = {}
        local during = pcall(function()
            for i = 1, 300 do
                u[i] = i
                if i == 150 then freeze(u) end
            end
        end)
        local keys = {}
        for k in pairs({ zeta = 1, alpha = 2, [3] = 3, [1] = 1 }) do keys[#keys + 1] = tostring(k) end
        return sum == 90300 and not before and t[1] == 2 and t[300] == 600
            and not during and u[150] == 150 and u[151] == nil
            and table.concat(keys, ",") == "1,3,alpha,zeta"
            and tostring(t) == "table" and tostring(0/0) == "nan"
    "#;
    let mut lua = Lua::functional();
    if !lua.jit_capabilities().supported_target {
        return;
    }
    let config = JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    };
    lua.set_jit_config(config).unwrap();
    let executor = lua.try_enter(|ctx| start(ctx, source)).unwrap();
    lua.prepare_jit().unwrap();
    assert!(lua.execute::<bool>(&executor).unwrap());
    let stats = lua.jit_stats();
    assert!(
        stats.native_entries > 0 && stats.native_instructions > 0,
        "{stats:?}"
    );
}

mod common;

/// A frozen userdata keeps its behaviour: its metatable is frozen with it.
#[test]
fn freezing_reaches_userdata_metatables() -> Result<(), ExternError> {
    let mut lua = common::functional();
    let executor = lua.try_enter(|ctx| {
        let env = functional::environment(ctx);
        let object = luna::UserData::new_static(&ctx, ());
        let metatable = luna::Table::new(&ctx);
        metatable.set(ctx, "__index", luna::Table::new(&ctx))?;
        object.set_metatable(ctx, Some(metatable));
        env.set(ctx, "object", object)?;
        let closure = Closure::load_with_env(
            ctx,
            None,
            b"return pcall(function() getmetatable(object).__index.x = 1 end)",
            env,
        )?;
        functional::freeze(ctx, object.into());
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert!(!lua.execute::<bool>(&executor)?);
    Ok(())
}

/// Builds a config with hundreds of keys inserted in a scrambled order, then serialises it with
/// `pairs`. Each process seeds table hashing differently, so any leak of hash layout would show.
const DETERMINISM_PROBE: &str = r#"
    local config = { nested = {}, list = {} }
    for i = 1, 400 do
        local k = (i * 7919) % 400
        config["key" .. k] = k * 1.5
        config[k] = (k % 3 == 0) and 0/0 or -k
        config.nested[#config.nested + 1] = { id = k, tag = string.format("%05.1f", k / 7) }
    end
    for i = 1, 50 do config.list[i] = (i * 31) % 17 end
    table.sort(config.list)
    local function encode(value)
        if type(value) ~= "table" then return tostring(value) end
        local parts = {}
        for k, v in pairs(value) do parts[#parts + 1] = tostring(k) .. "=" .. encode(v) end
        return "{" .. table.concat(parts, ",") .. "}"
    end
    return encode(config) .. "|" .. tostring(#config.list) .. "|" .. tostring(math.sin(0.5))
"#;

fn determinism_output() -> String {
    let mut lua = Lua::functional();
    let executor = lua.try_enter(|ctx| start(ctx, DETERMINISM_PROBE)).unwrap();
    lua.execute::<String>(&executor).unwrap()
}

/// Prints the probe's output when a parent process asks for it, and does nothing otherwise.
#[test]
fn determinism_child() {
    if std::env::var_os("LUNA_FUNCTIONAL_DETERMINISM").is_some() {
        println!("OUTPUT:{}", determinism_output());
    }
}

#[test]
fn separate_processes_produce_identical_output() {
    let run = || {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "determinism_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LUNA_FUNCTIONAL_DETERMINISM", "1")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.split_once("OUTPUT:").map(|(_, rest)| rest.to_owned()))
            .expect("child printed its output")
    };
    let local = determinism_output();
    assert!(local.len() > 10_000);
    assert_eq!(run(), local);
    assert_eq!(run(), local);
}

/// The same probe compiled to native code up front gives byte-identical output.
#[cfg(feature = "jit")]
#[test]
fn native_code_produces_identical_output() {
    use luna::{JitConfig, JitMode};

    let mut lua = Lua::functional();
    if !lua.jit_capabilities().supported_target {
        return;
    }
    let config = JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    };
    lua.set_jit_config(config).unwrap();
    let executor = lua.try_enter(|ctx| start(ctx, DETERMINISM_PROBE)).unwrap();
    lua.prepare_jit().unwrap();
    let native = lua.execute::<String>(&executor).unwrap();
    assert!(lua.jit_stats().native_instructions > 0);
    assert_eq!(native, determinism_output());
}
