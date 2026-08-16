//! `__gc` handlers.
//!
//! A handler is Lua, and Lua cannot be called from inside a collection. So the collector resurrects
//! a dead object that has one and queues it; the handler runs afterwards, driven by `Lua::finish`.
//!
//! Note the ordinary Lua caveat these tests work around: a value is only collectable once nothing
//! references it, and a local's *stack slot* can keep it alive after the local goes out of scope.
//! Every test below churns the stack before collecting, exactly as one has to in PUC-Rio.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<i64, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<i64>(&executor)
}

const CHURN: &str = "for i = 1, 2000 do local t = { i, i, i } end";

#[test]
fn a_table_handler_runs_on_collection() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local ran = 0
            local function make()
                local t = setmetatable({{}}, {{ __gc = function() ran = ran + 1 end }})
                return #tostring(t)
            end
            make()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return ran
        "#
        ))?,
        1
    );
    Ok(())
}

/// Exactly once, even across several collections.
#[test]
fn a_handler_runs_only_once() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local ran = 0
            local function make()
                local t = setmetatable({{}}, {{ __gc = function() ran = ran + 1 end }})
                return #tostring(t)
            end
            make()
            {CHURN}
            for _ = 1, 5 do collectgarbage("collect") end
            return ran
        "#
        ))?,
        1
    );
    Ok(())
}

/// A live object is not finalized, however often the collector runs.
#[test]
fn a_reachable_object_is_not_finalized() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local ran = 0
            local kept = setmetatable({{}}, {{ __gc = function() ran = ran + 1 end }})
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return ran + (kept ~= nil and 0 or 100)
        "#
        ))?,
        0
    );
    Ok(())
}

/// The handler receives the object it is finalizing.
#[test]
fn the_handler_receives_its_object() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local seen = 0
            local function make()
                local t = setmetatable({{ marker = 7 }}, {{
                    __gc = function(self) seen = self.marker end
                }})
                return #tostring(t)
            end
            make()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return seen
        "#
        ))?,
        7
    );
    Ok(())
}

/// A handler that raises must not stop the others from running.
#[test]
fn an_erroring_handler_does_not_stop_the_sweep() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local ran = 0
            local function make()
                local a = setmetatable({{}}, {{ __gc = function() error("boom") end }})
                local b = setmetatable({{}}, {{ __gc = function() ran = ran + 1 end }})
                return #tostring(a) + #tostring(b)
            end
            make()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return ran
        "#
        ))?,
        1
    );
    Ok(())
}

/// Several objects each get their handler.
#[test]
fn every_dead_object_is_finalized() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local ran = 0
            local function make()
                local n = 0
                for i = 1, 5 do
                    local t = setmetatable({{}}, {{ __gc = function() ran = ran + 1 end }})
                    n = n + #tostring(t)
                end
                return n
            end
            make()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return ran
        "#
        ))?,
        5
    );
    Ok(())
}

/// A table without `__gc` never enters the registry, so nothing changes for the common case.
#[test]
fn tables_without_a_handler_are_unaffected() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local plain = setmetatable({{}}, {{ __index = {{}} }})
            {CHURN}
            collectgarbage("collect")
            return plain ~= nil and 0 or 1
        "#
        ))?,
        0
    );
    Ok(())
}

/// A host can attach a destructor without going through Lua: `__gc` is looked up as a value and
/// called like any other function, so a Rust `Callback` works there.
#[test]
fn a_rust_callback_can_be_the_handler() -> Result<(), ExternError> {
    use std::cell::Cell;
    use std::rc::Rc;

    use luna::{Callback, CallbackReturn, Table, Value};

    let ran = Rc::new(Cell::new(0));
    let seen = ran.clone();

    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let make = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let mt = Table::new(&ctx);
            let counter = seen.clone();
            mt.set_field(
                ctx,
                "__gc",
                Callback::from_fn(&ctx, move |_, _, _| {
                    counter.set(counter.get() + 1);
                    Ok(CallbackReturn::Return)
                }),
            );
            let t = Table::new(&ctx);
            t.set_metatable(ctx, Some(mt));
            // Returns something else, so the table itself is not kept alive by the result.
            stack.replace(ctx, Value::Boolean(true));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("make_native", make);
    });

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            format!(
                r#"
                make_native()
                {CHURN}
                collectgarbage("collect")
                collectgarbage("collect")
                return 0
            "#
            )
            .as_bytes(),
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<i64>(&executor)?;

    assert_eq!(ran.get(), 1, "the Rust handler should have run once");
    Ok(())
}
