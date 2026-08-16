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

/// Weak keys are documented as not implemented — a `__mode` of `"k"` leaves keys strong. This test
/// exists so that stays a decision rather than drifting into a silent surprise; see
/// COMPATIBILITY.md, "`__mode`". If weak keys are ever implemented with ephemeron semantics, this
/// test should be rewritten, not deleted.
#[test]
fn weak_keys_are_not_implemented_and_keep_their_entries() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local function fill() t[{{ id = 1 }}] = "metadata" end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for _ in pairs(t) do n = n + 1 end
            return n
        "#
        ))?,
        1
    );
    Ok(())
}

/// `"kv"` degrades to weak-valued rather than erroring, which is the half luna can do correctly.
#[test]
fn mode_kv_behaves_as_weak_valued() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "kv" }})
            local function fill() t.entry = {{ payload = true }} end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return t.entry == nil and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}
