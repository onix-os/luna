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

/// Growth is where a weak-key table used to die: the rehash asked every key for its live form,
/// which a weak key holding an object cannot give. Four keys were enough to reach it.
#[test]
fn a_weak_key_table_grows_past_its_first_bucket() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            r#"
            local t = setmetatable({}, { __mode = "k" })
            local keep = {}
            for i = 1, 100 do
                local k = {}
                keep[i] = k
                t[k] = i
            end
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for k, v in pairs(t) do
                if t[keep[v]] ~= v then return -1 end
                n = n + 1
            end
            return n
        "#
        )?,
        100
    );
    Ok(())
}

/// The same growth, with nothing holding the keys: every entry must go.
#[test]
fn a_hundred_unheld_weak_keys_are_all_reclaimed() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local function fill()
                for i = 1, 100 do t[{{}}] = i end
            end
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

/// An object key in a weak table is never an array index, so growing the array must step over it
/// rather than demand a live key for it.
#[test]
fn an_object_key_survives_array_growth() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            r#"
            local t = setmetatable({}, { __mode = "k" })
            local k = {}
            t[k] = "obj"
            for i = 1, 10 do t[i] = i end
            local n = 0
            for _ in pairs(t) do n = n + 1 end
            return (t[k] == "obj" and t[7] == 7 and n == 11) and 1 or 0
        "#
        )?,
        1
    );
    Ok(())
}

/// `table.insert` reaches the same growth path through `length`.
#[test]
fn table_insert_into_a_weak_key_table() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            r#"
            local t = setmetatable({}, { __mode = "k" })
            local k = {}
            t[k] = "obj"
            table.insert(t, 1)
            table.insert(t, 2)
            return (t[k] == "obj" and t[1] == 1 and t[2] == 2) and 1 or 0
        "#
        )?,
        1
    );
    Ok(())
}

/// Growing the array must not read a weak slot as nil and throw the value away.
#[test]
fn growth_does_not_discard_a_live_weak_value() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            r#"
            local w = setmetatable({}, { __mode = "v" })
            local keep = {}
            w[2] = keep
            if w[2] ~= keep then return -1 end
            table.insert(w, keep)
            return (w[2] == keep and w[1] == keep) and 1 or 0
        "#
        )?,
        1
    );
    Ok(())
}

/// `__mode = "v"` applies to integer keys too — a cache built with `t[#t + 1] = obj` must release.
#[test]
fn integer_keyed_weak_values_are_released() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local cache = setmetatable({{}}, {{ __mode = "v" }})
            local function fill()
                for i = 1, 100 do cache[#cache + 1] = {{ i }} end
            end
            fill()
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for _ in pairs(cache) do n = n + 1 end
            return n
        "#
        ))?,
        0
    );
    Ok(())
}

/// The same table with its values held keeps every one of them.
#[test]
fn integer_keyed_weak_values_that_are_held_survive() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local cache = setmetatable({{}}, {{ __mode = "v" }})
            local keep = {{}}
            for i = 1, 100 do
                local v = {{ i }}
                keep[i] = v
                cache[#cache + 1] = v
            end
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            local n = 0
            for i, v in pairs(cache) do
                if v ~= keep[i] then return -1 end
                n = n + 1
            end
            return n
        "#
        ))?,
        100
    );
    Ok(())
}

/// Overwriting an entry must not promote its key to a strong one.
#[test]
fn overwriting_a_weak_key_leaves_it_weak() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local function fill()
                for i = 1, 100 do
                    local k = {{}}
                    t[k] = 1
                    t[k] = 2
                    t[k] = 3
                end
            end
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

/// Strings are values in Lua, so equal strings are one key however the table holds its keys —
/// and, being values, they are never removed from a weak table.
#[test]
fn equal_strings_are_one_weak_key() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local a = string.rep("x", 50)
            local b = string.rep("x", 50)
            t[a] = 1
            if t[b] ~= 1 then return -1 end
            t[b] = 2
            if t[a] ~= 2 then return -2 end
            local n = 0
            for _ in pairs(t) do n = n + 1 end
            if n ~= 1 then return -3 end
            a, b = nil, nil
            {CHURN}
            collectgarbage("collect")
            collectgarbage("collect")
            return t[string.rep("x", 50)] == 2 and 1 or 0
        "#
        ))?,
        1
    );
    Ok(())
}

/// Removing a key and putting it back under an equal-but-distinct string must reuse the entry.
/// A second bucket for the same key stalls `next` forever: it always resolves to the first one.
#[test]
fn re_adding_an_equal_string_key_leaves_one_entry() -> Result<(), ExternError> {
    assert_eq!(
        eval(
            r#"
            local a = string.rep("x", 50)
            local b = string.rep("x", 50)
            local t = {}
            t[a] = 1
            t[a] = nil
            t[b] = 2
            t[a] = 3
            local n = 0
            for k, v in pairs(t) do n = n + 1 end
            return (n == 1 and t[a] == 3 and t[b] == 3) and 1 or 0
        "#
        )?,
        1
    );
    Ok(())
}

/// The ephemeron leak: a value pointing back at its own key must not be kept alive by the roots
/// the *previous* cycle rooted it with.
#[test]
fn a_back_reference_does_not_survive_repeated_collection() -> Result<(), ExternError> {
    assert_eq!(
        eval(&format!(
            r#"
            local t = setmetatable({{}}, {{ __mode = "k" }})
            local k = {{}}
            local v = {{}}
            v.back = k
            t[k] = v
            collectgarbage("collect")
            k, v = nil, nil
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
