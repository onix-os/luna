//! `debug.sethook`: line and count hooks.

use luna::{Closure, Executor, ExternError, Lua};

fn eval<T: for<'gc> luna::FromMultiValue<'gc> + 'static>(source: &str) -> Result<T, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<T>(&executor)
}

#[test]
fn a_line_hook_sees_each_line() -> Result<(), ExternError> {
    assert_eq!(
        eval::<i64>(
            r#"
            local seen = 0
            debug.sethook(function(event, line)
                if event == "line" then seen = seen + 1 end
            end, "l")
            local a = 1
            local b = 2
            local c = a + b
            debug.sethook()
            return seen
        "#
        )? > 0,
        true
    );
    Ok(())
}

/// The line number is real, not a placeholder.
#[test]
fn a_line_hook_reports_the_line_it_is_about_to_run() -> Result<(), ExternError> {
    // The assignment is on line 4 of the chunk; the hook must see that number.
    assert_eq!(
        eval::<bool>(
            "local hit = false\ndebug.sethook(function(e, l) if l == 4 then hit = true end end, \"l\")\nlocal a = 1\nlocal b = 2\ndebug.sethook()\nreturn hit"
        )?,
        true
    );
    Ok(())
}

/// Roughly every `count` instructions — the exact number depends on how the loop compiles, so the
/// test pins the order of magnitude rather than an exact count.
#[test]
fn a_count_hook_fires_on_a_schedule() -> Result<(), ExternError> {
    let fired = eval::<i64>(
        r#"
        local n = 0
        debug.sethook(function() n = n + 1 end, "", 100)
        local s = 0
        for i = 1, 2000 do s = s + i end
        debug.sethook()
        return n
    "#,
    )?;
    assert!(
        (10..200).contains(&fired),
        "expected tens of firings, got {fired}"
    );
    Ok(())
}

/// A hook runs Lua, which would trigger the hook again. Suppression is by frame depth, so it ends
/// exactly when the hook's own frames do — a naive flag would either never re-arm or recurse.
#[test]
fn a_hook_does_not_trigger_itself() -> Result<(), ExternError> {
    let depth = eval::<i64>(
        r#"
        local n = 0
        debug.sethook(function()
            n = n + 1
            local t = {}
            for i = 1, 10 do t[i] = i * 2 end
        end, "l")
        local a = 1
        local b = 2
        debug.sethook()
        return n
    "#,
    )?;
    assert!(depth < 50, "hook recursed: {depth} firings");
    Ok(())
}

#[test]
fn sethook_with_no_arguments_clears_it() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            debug.sethook(function() end, "l")
            debug.sethook()
            local hook = debug.gethook()
            return hook == nil
        "#
        )?,
        true
    );
    Ok(())
}

#[test]
fn gethook_reports_what_was_installed() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            local f = function() end
            debug.sethook(f, "l")
            local hook, mask = debug.gethook()
            debug.sethook()
            return hook == f and mask == "l"
        "#
        )?,
        true
    );
    Ok(())
}

/// Call and return hooks are not implemented, and `sethook` says so rather than accepting a mask
/// it will never honour — the same rule applied to weak keys.
#[test]
fn unimplemented_masks_are_rejected_not_ignored() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            local ok_call = pcall(debug.sethook, function() end, "c")
            local ok_ret = pcall(debug.sethook, function() end, "r")
            local ok_empty = pcall(debug.sethook, function() end, "")
            return not ok_call and not ok_ret and not ok_empty
        "#
        )?,
        true
    );
    Ok(())
}

/// A hook is a normal Lua function, so an error in it propagates like any other — and is caught by
/// an enclosing `pcall`.
///
/// The hook errors *once*: one that errors unconditionally would be caught here, re-arm, and then
/// fire again outside the `pcall`, which is correct but tests something else.
#[test]
fn an_erroring_hook_is_catchable() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            local fired = 0
            local ok = pcall(function()
                debug.sethook(function()
                    fired = fired + 1
                    if fired == 1 then error("from the hook") end
                end, "l")
                local a = 1
                local b = 2
            end)
            debug.sethook()
            return not ok
        "#
        )?,
        true
    );
    Ok(())
}

/// `debug.getlocal` names and reads the locals live at a level.
#[test]
fn getlocal_reports_locals_in_declaration_order() -> Result<(), ExternError> {
    assert_eq!(
        eval::<String>(
            r#"
            local function probe()
                local alpha = 10
                local beta = "two"
                local names = {}
                for i = 1, 3 do
                    local n = debug.getlocal(1, i)
                    if n == nil then break end
                    names[#names + 1] = n
                end
                return table.concat(names, ",")
            end
            return probe()
        "#
        )?,
        "alpha,beta,names"
    );
    Ok(())
}

#[test]
fn getlocal_reads_the_value() -> Result<(), ExternError> {
    assert_eq!(
        eval::<i64>(
            r#"
            local function probe()
                local answer = 42
                local _, v = debug.getlocal(1, 1)
                return v
            end
            return probe()
        "#
        )?,
        42
    );
    Ok(())
}

/// Parameters are locals, and are the ones most often asked for.
#[test]
fn getlocal_sees_parameters() -> Result<(), ExternError> {
    assert_eq!(
        eval::<String>(
            r#"
            local function withargs(first, second)
                return debug.getlocal(1, 1) .. "," .. debug.getlocal(1, 2)
            end
            return withargs(1, 2)
        "#
        )?,
        "first,second"
    );
    Ok(())
}

/// `setlocal` writes through to the variable itself, not a copy.
#[test]
fn setlocal_changes_the_variable() -> Result<(), ExternError> {
    assert_eq!(
        eval::<i64>(
            r#"
            local function probe()
                local n = 1
                debug.setlocal(1, 1, 99)
                return n
            end
            return probe()
        "#
        )?,
        99
    );
    Ok(())
}

#[test]
fn getlocal_out_of_range_is_nil() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            local function probe()
                local a = 1
                return debug.getlocal(1, 99) == nil and debug.getlocal(99, 1) == nil
            end
            return probe()
        "#
        )?,
        true
    );
    Ok(())
}
