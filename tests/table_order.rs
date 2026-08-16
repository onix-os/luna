//! The map part of a table iterates in insertion order.
//!
//! Lua promises nothing here, but a per-process hash seed makes the same table iterate differently
//! between runs of the same binary, which is no use to anything building an ordered structure out
//! of a table.

use luna::{Closure, Executor, ExternError, Lua};

fn keys(source: &str) -> Result<String, ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<String>(&executor)
}

#[test]
fn map_keys_iterate_in_insertion_order() -> Result<(), ExternError> {
    let source = r#"
        local t = { who = "world", n = 2, extra = true, another = 1 }
        local out = {}
        for k in pairs(t) do out[#out + 1] = k end
        return table.concat(out, ",")
    "#;
    assert_eq!(keys(source)?, "who,n,extra,another");
    Ok(())
}

/// The order has to be the same every run, not merely self-consistent within one.
#[test]
fn the_order_is_stable_across_tables_and_runs() -> Result<(), ExternError> {
    let source = r#"
        local out = {}
        for i = 1, 5 do
            local t = {}
            t.alpha = 1
            t.beta = 2
            t.gamma = 3
            local seen = {}
            for k in pairs(t) do seen[#seen + 1] = k end
            out[#out + 1] = table.concat(seen, "")
        end
        return table.concat(out, ",")
    "#;
    assert_eq!(
        keys(source)?,
        "alphabetagamma,alphabetagamma,alphabetagamma,alphabetagamma,alphabetagamma"
    );
    Ok(())
}

/// The array part comes first, then the map part in insertion order.
#[test]
fn the_array_part_still_leads() -> Result<(), ExternError> {
    let source = r#"
        local t = { "a", "b" }
        t.zeta = 1
        t.eta = 2
        local out = {}
        for k in pairs(t) do out[#out + 1] = tostring(k) end
        return table.concat(out, ",")
    "#;
    assert_eq!(keys(source)?, "1,2,zeta,eta");
    Ok(())
}

/// Removing a key must not disturb the order of the keys around it.
#[test]
fn removal_leaves_the_rest_in_order() -> Result<(), ExternError> {
    let source = r#"
        local t = {}
        t.one = 1
        t.two = 2
        t.three = 3
        t.four = 4
        t.two = nil
        local out = {}
        for k in pairs(t) do out[#out + 1] = k end
        return table.concat(out, ",")
    "#;
    assert_eq!(keys(source)?, "one,three,four");
    Ok(())
}

/// Enough keys to force the map to grow and compact its order slots more than once.
#[test]
fn order_survives_growth() -> Result<(), ExternError> {
    let source = r#"
        local t = {}
        for i = 1, 200 do t["k" .. i] = i end
        local ok = true
        local expected = 1
        for k, v in pairs(t) do
            if v ~= expected then ok = false end
            expected = expected + 1
        end
        return ok and expected == 201 and "yes" or "no"
    "#;
    assert_eq!(keys(source)?, "yes");
    Ok(())
}

/// Growth after removals must not resurrect the removed keys or reorder the survivors.
#[test]
fn order_survives_growth_after_removal() -> Result<(), ExternError> {
    let source = r#"
        local t = {}
        for i = 1, 100 do t["k" .. i] = i end
        for i = 1, 100, 2 do t["k" .. i] = nil end
        for i = 101, 200 do t["k" .. i] = i end
        local out = {}
        for k, v in pairs(t) do out[#out + 1] = v end
        local ok = #out == 150
        for i = 2, #out do
            if out[i] <= out[i - 1] then ok = false end
        end
        return ok and "yes" or "no"
    "#;
    assert_eq!(keys(source)?, "yes");
    Ok(())
}
