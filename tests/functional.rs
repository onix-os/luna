//! A functional state must be pure: no ambient inputs, no shared mutation, no hidden state in
//! closures, and the same traversal order every run.

use luna::{
    compiler::CompileErrorKind, functional, Callback, CallbackReturn, Closure, CompilerError,
    Executor, ExternError, Lua, RuntimeError, Value,
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

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::functional();
    let executor = lua.try_enter(|ctx| {
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
    })?;
    lua.execute::<bool>(&executor)
}

fn compile_error(source: &str) -> Option<CompileErrorKind> {
    let mut lua = Lua::functional();
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
    assert!(compile_error("local n = 0 n = n + 1 local function read() return n end").is_none());
}

#[test]
fn load_compiles_in_functional_mode_and_refuses_binary() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local fine = load("local x = 1 x = x + 1 return x")
        local captured = load("local n = 0 return function() n = n + 1 end")
        local binary = pcall(load, "\27Luna\0", "chunk", "b")
        return fine() == 2 and captured == nil and not binary
    "#
    )?);
    let mut lua = Lua::functional();
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
    let mut lua = Lua::functional();
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
    let mut lua = Lua::core();
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
    let mut lua = Lua::functional();
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
