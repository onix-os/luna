//! `local x <close>` — the handler has to run on *every* way out of the block.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<String, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<String>(&executor)
}

/// A tracker whose `__close` appends to a log, so the tests can assert ordering.
const PRELUDE: &str = r#"
    local log = {}
    local function res(name)
        return setmetatable({}, { __close = function(_, err)
            -- Errors now carry a "chunk:line:" prefix; record only the message after it.
            local text = err and tostring(err):gsub("^.*:%d+: ", "") or nil
            log[#log + 1] = name .. (text and ("!" .. text) or "")
        end })
    end
    local function result() return table.concat(log, ",") end
"#;

#[test]
fn closes_on_normal_block_exit() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            do local a <close> = res("a") end
            log[#log + 1] = "after"
            return result()
        "#
        ))?,
        "a,after"
    );
    Ok(())
}

/// Reverse declaration order, as Lua specifies.
#[test]
fn closes_in_reverse_declaration_order() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            do
                local a <close> = res("a")
                local b <close> = res("b")
                local c <close> = res("c")
            end
            return result()
        "#
        ))?,
        "c,b,a"
    );
    Ok(())
}

#[test]
fn closes_on_break() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            for i = 1, 3 do
                local a <close> = res("i" .. i)
                if i == 2 then break end
            end
            return result()
        "#
        ))?,
        "i1,i2"
    );
    Ok(())
}

#[test]
fn closes_on_goto_out_of_the_block() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            do
                local a <close> = res("a")
                goto done
            end
            ::done::
            log[#log + 1] = "after"
            return result()
        "#
        ))?,
        "a,after"
    );
    Ok(())
}

#[test]
fn closes_on_return_from_a_function() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            local function f()
                local a <close> = res("a")
                return "returned"
            end
            local v = f()
            log[#log + 1] = v
            return result()
        "#
        ))?,
        "a,returned"
    );
    Ok(())
}

/// The case cleanup exists for: an error unwinding past the variable.
#[test]
fn closes_when_an_error_unwinds_past_it() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            local ok, err = pcall(function()
                local a <close> = res("a")
                error("boom")
            end)
            log[#log + 1] = tostring(ok)
            return result()
        "#
        ))?,
        "a!boom,false"
    );
    Ok(())
}

/// Unwinding through several scopes runs every handler, innermost first.
#[test]
fn closes_every_scope_an_error_passes_through() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            pcall(function()
                local outer <close> = res("outer")
                do
                    local inner <close> = res("inner")
                    error("boom")
                end
            end)
            return result()
        "#
        ))?,
        "inner!boom,outer!boom"
    );
    Ok(())
}

/// `false` and `nil` are allowed, so a conditional resource needs no special case.
#[test]
fn false_and_nil_are_allowed_and_skipped() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            do
                local a <close> = false
                local b <close> = nil
                local c <close> = res("c")
            end
            return result()
        "#
        ))?,
        "c"
    );
    Ok(())
}

/// A value with no `__close` is rejected where it is declared.
#[test]
fn a_value_without_close_is_rejected() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            r#"
            local ok = pcall(function()
                local a <close> = {}
                return 1
            end)
            return tostring(ok)
        "#
        )?,
        "false"
    );
    Ok(())
}

/// An error raised *by* a handler replaces the one being carried, and the rest still run.
#[test]
fn a_handler_that_errors_still_lets_the_others_run() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            local ok, err = pcall(function()
                local a <close> = res("a")
                local b <close> = setmetatable({{}}, {{ __close = function() error("from-handler") end }})
                error("original")
            end)
            log[#log + 1] = tostring(ok)
            return result()
        "#
        ))?,
        "a!from-handler,false"
    );
    Ok(())
}

/// A `<close>` variable in a loop body closes once per iteration.
#[test]
fn closes_each_loop_iteration() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"{PRELUDE}
            for i = 1, 3 do
                local a <close> = res("i" .. i)
            end
            return result()
        "#
        ))?,
        "i1,i2,i3"
    );
    Ok(())
}
