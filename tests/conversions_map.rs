//! Map and set conversions in both directions.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use luna::{Callback, CallbackReturn, Closure, Executor, ExternError, Lua};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = Lua::core();
    lua.try_enter(|ctx| {
        // Hands a map to Lua, and reads one back.
        let roundtrip = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let map: BTreeMap<String, i64> = stack.consume(ctx)?;
            let doubled: BTreeMap<String, i64> = map.into_iter().map(|(k, v)| (k, v * 2)).collect();
            stack.replace(ctx, doubled);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("roundtrip", roundtrip);

        let set_roundtrip = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let set: BTreeSet<i64> = stack.consume(ctx)?;
            stack.replace(ctx, set.len() as i64);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("set_size", set_roundtrip);

        let hash_map = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let map: HashMap<String, String> = stack.consume(ctx)?;
            stack.replace(ctx, map.len() as i64);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("hash_size", hash_map);

        let make_set = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let set: HashSet<i64> = [1, 2, 3].into_iter().collect();
            stack.replace(ctx, set);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("make_set", make_set);
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

#[test]
fn maps_round_trip() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local out = roundtrip({ a = 1, b = 2 })
        return out.a == 2 and out.b == 4
    "#
    )?);
    Ok(())
}

#[test]
fn sets_convert_from_lua_tables() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return set_size({ [1] = true, [2] = true, [3] = false }) == 2
    "#
    )?);
    Ok(())
}

#[test]
fn sets_convert_into_lua_tables() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local s = make_set()
        return s[1] == true and s[2] == true and s[3] == true and s[4] == nil
    "#
    )?);
    Ok(())
}

#[test]
fn hash_maps_work_too() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return hash_size({ x = "1", y = "2", z = "3" }) == 3
    "#
    )?);
    Ok(())
}
