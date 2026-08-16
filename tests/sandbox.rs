//! Frozen tables and the memory ceiling: together, what makes the sandboxing claim true.

use luna::{Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

#[test]
fn a_frozen_table_refuses_writes() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = table.freeze({ a = 1 })
        local ok = pcall(function() t.b = 2 end)
        return ok == false and t.a == 1 and t.b == nil
    "#
    )?);
    Ok(())
}

/// The point of freezing: `rawset` must not be an escape hatch.
#[test]
fn rawset_cannot_bypass_a_freeze() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = table.freeze({})
        local ok = pcall(rawset, t, "x", 1)
        return ok == false and t.x == nil
    "#
    )?);
    Ok(())
}

#[test]
fn freezing_is_reportable() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local a, b = {}, table.freeze({})
        return table.isfrozen(a) == false and table.isfrozen(b) == true
    "#
    )?);
    Ok(())
}

/// The scenario that motivates it: shared globals, two scripts, one hostile.
#[test]
fn a_frozen_stdlib_table_survives_sabotage() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        table.freeze(string)
        local ok = pcall(function() string.format = function() return "sabotaged" end end)
        return ok == false and string.format("%d", 7) == "7"
    "#
    )?);
    Ok(())
}

/// Slice-granular, so the script is stopped rather than erroring at the allocation.
#[test]
fn a_runaway_allocation_is_stopped_by_the_memory_ceiling() {
    let mut lua = Lua::core();
    let before = lua.total_memory();
    lua.set_memory_limit(Some(before + 4 * 1024 * 1024));
    assert_eq!(lua.memory_limit(), Some(before + 4 * 1024 * 1024));

    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                &br#"
                    local t = {}
                    for i = 1, 100000000 do t[i] = i end
                    return true
                "#[..],
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    // Returns rather than running the machine out of memory.
    lua.finish(&executor).unwrap();
    assert!(lua.total_memory() < before + 64 * 1024 * 1024);
}

#[test]
fn no_limit_by_default() {
    let lua = Lua::core();
    assert_eq!(lua.memory_limit(), None);
}

/// `__newindex` firing on every store, not just absent keys.
///
/// The case it exists for: a namespace where the *destination* of a write depends on the value
/// being written. With stock Lua semantics a name assigned a table and then a string stays put,
/// because the key now exists and the metamethod stops firing.
#[test]
fn intercept_all_writes_fires_newindex_every_time() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local elsewhere = {}
        local t = {}
        setmetatable(t, { __newindex = function(tbl, k, v)
            if type(v) == "string" then
                elsewhere[k] = v
            else
                rawset(tbl, k, v)
            end
        end })
        table.interceptall(t)

        t.x = "a string"   -- goes elsewhere
        t.x = {}           -- lands in t
        t.x = "back again" -- must return elsewhere, not stay in t

        return elsewhere.x == "back again"
    "#
    )?);
    Ok(())
}

/// Without it, Lua's normal rule applies: a present key stops the metamethod firing.
#[test]
fn without_it_a_present_key_stops_newindex() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local seen = 0
        local t = setmetatable({}, { __newindex = function(tbl, k, v)
            seen = seen + 1
            rawset(tbl, k, v)
        end })
        t.x = 1
        t.x = 2
        return seen == 1 and t.x == 2
    "#
    )?);
    Ok(())
}

/// Hitting the ceiling collects before giving up: a script that has merely produced garbage
/// should not be killed for it.
#[test]
fn the_ceiling_collects_before_stopping() {
    let mut lua = Lua::core();
    let baseline = lua.total_memory();
    lua.set_memory_limit(Some(baseline + 2 * 1024 * 1024));

    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                // Allocates far more than the ceiling in total, but holds none of it — so with a
                // collection in the loop it completes rather than being stopped.
                &br#"
                    local sum = 0
                    for i = 1, 60000 do
                        local t = { i, i, i, i, i, i, i, i }
                        sum = sum + t[1]
                    end
                    return sum
                "#[..],
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();

    let total = lua.execute::<i64>(&executor);
    assert_eq!(
        total.ok(),
        Some(1_800_030_000),
        "garbage alone must not trip the ceiling"
    );
}
