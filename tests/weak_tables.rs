//! `__mode = "v"`: values held weakly.
//!
//! Built by making the *slot* weak rather than by teaching the collector to skip tracing. The
//! difference matters: a `GcWeak` can only be read by upgrading, so a collected value answers
//! `None` and there is no window in which the table holds a pointer to freed memory. The
//! skip-tracing version would make that a discipline instead of a fact about the type.

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
fn a_weak_value_disappears_after_collection() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local cache = setmetatable({{}}, {{ __mode = "v" }})
            local function fill() cache.entry = {{ payload = true }} end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return cache.entry == nil and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}

/// A value still referenced elsewhere survives — otherwise the table would be useless.
#[test]
fn a_strongly_held_value_survives() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local cache = setmetatable({{}}, {{ __mode = "v" }})
            local kept = {{ payload = true }}
            cache.entry = kept
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return (cache.entry == kept) and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}

/// Iteration skips collected entries rather than yielding a hole.
#[test]
fn iteration_skips_collected_entries() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local cache = setmetatable({{}}, {{ __mode = "v" }})
            local kept = {{ id = "kept" }}
            cache.kept = kept
            local function fill() cache.gone = {{ id = "gone" }} end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for k, v in pairs(cache) do n = n + 1 end
            return n
        "#
        ))?,
        1
    );
    Ok(())
}

/// Non-collectable values are held as they are: there is nothing behind them to lose.
#[test]
fn primitives_in_a_weak_table_are_kept() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local cache = setmetatable({{}}, {{ __mode = "v" }})
            cache.n = 42
            cache.s = "text"
            cache.b = true
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return (cache.n == 42 and cache.s == "text" and cache.b == true) and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}

/// Entries present before `__mode` is set are weakened too.
#[test]
fn existing_entries_are_weakened() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local cache = {{}}
            local function fill() cache.entry = {{ payload = true }} end
            fill()
            setmetatable(cache, {{ __mode = "v" }})
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return cache.entry == nil and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}

/// A table without `__mode` is unaffected.
#[test]
fn a_strong_table_keeps_everything() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local strong = {{}}
            local function fill() strong.entry = {{ payload = true }} end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return strong.entry ~= nil and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}

/// A weak key with nothing else referring to it goes away.
#[test]
fn a_weak_key_disappears_with_its_object() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local function fill() t[{{}}] = "metadata" end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for _ in pairs(t) do n = n + 1 end
            return n
        "#
        ))?,
        0
    );
    Ok(())
}

/// A key held elsewhere keeps its entry — and its value, which is the half that needs the
/// finalizer pass to put back what weak storage took away.
#[test]
fn a_held_key_keeps_its_entry_and_value() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local keep = {{}}
            t[keep] = {{ payload = 7 }}
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return (t[keep] ~= nil and t[keep].payload == 7) and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}

/// The case that makes weak keys hard, and the reason for ephemeron marking: the value refers back
/// to its own key. Holding values strongly would make the value keep the key alive, so the entry
/// could never be collected — a leak precisely where weak keys are supposed to help.
#[test]
fn a_value_referring_to_its_own_key_is_still_collected() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local function cycle()
                local k = {{}}
                t[k] = {{ owner = k }}
            end
            cycle()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for _ in pairs(t) do n = n + 1 end
            return n
        "#
        ))?,
        0
    );
    Ok(())
}

/// Two entries whose values refer to each other's keys: neither is reachable from outside, so both
/// must go. A single marking pass without iteration would keep them.
#[test]
fn mutually_referring_entries_are_collected() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local function chain()
                local a, b = {{}}, {{}}
                t[a] = {{ b }}
                t[b] = {{ a }}
            end
            chain()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for _ in pairs(t) do n = n + 1 end
            return n
        "#
        ))?,
        0
    );
    Ok(())
}

/// A chain of ephemerons: the value of one entry is the key of the next. Anchoring the first must
/// keep the whole chain, which is what iterating to a fixed point buys.
#[test]
fn an_anchored_chain_survives_end_to_end() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local head = {{}}
            local function build()
                local a, b = {{}}, {{}}
                t[head] = a
                t[a] = b
                t[b] = "end"
            end
            build()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for _ in pairs(t) do n = n + 1 end
            return n
        "#
        ))?,
        3
    );
    Ok(())
}

/// `"kv"` is both: keys weak *and* values weak, so an entry goes when either side does.
#[test]
fn mode_kv_is_weak_on_both_sides() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "kv" }})
            local held = {{}}
            local function fill()
                t.entry = {{ payload = true }}   -- string key, value unreachable
                t[held] = {{ payload = true }}   -- key held, value unreachable
            end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            -- both values are gone even though one key is held
            return (t.entry == nil and t[held] == nil) and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}
