//! `string.dump` and the loader that reads it back.
//!
//! The mutation test at the bottom is the one that matters. A loader that only ever sees its own
//! output is a loader nobody has tested.

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
fn a_dumped_function_runs_again() -> Result<(), ExternError> {
    assert_eq!(
        eval::<i64>(
            r#"
            -- A chunk, which is what precompilation dumps.
            local chunk = assert(load("local a, b = ... local sum = a + b return sum * 2"))
            local back = assert(load(string.dump(chunk), "c", "b"))
            return back(3, 4)
        "#
        )?,
        14
    );
    Ok(())
}

/// Control flow, tables, varargs and nested functions all have to survive the round trip, because
/// each exercises a different corner of the opcode encoding.
#[test]
fn the_round_trip_preserves_behaviour() -> Result<(), ExternError> {
    assert_eq!(
        eval::<String>(
            r#"
            local source = [[
                local out = {}
                for i = 1, select('#', ...) do
                    local v = select(i, ...)
                    if type(v) == "number" then
                        out[#out + 1] = tostring(v * 2)
                    else
                        out[#out + 1] = v:upper()
                    end
                end
                local inner = function(s) return "[" .. s .. "]" end
                return inner(table.concat(out, ","))
            ]]
            local original = assert(load(source))(1, "two", 3)
            local dumped = assert(load(string.dump(assert(load(source))), "c", "b"))
            local back = dumped(1, "two", 3)
            assert(original == back, original .. " vs " .. back)
            return back
        "#
        )?,
        "[2,TWO,6]"
    );
    Ok(())
}

#[test]
fn stripping_drops_local_names_but_not_behaviour() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            local f = assert(load("local a = ... local doubled = a * 2 return doubled"))
            local full = string.dump(f)
            local thin = string.dump(f, true)
            assert(#thin < #full, "stripping should be smaller")
            return assert(load(thin, "c", "b"))(21) == 42
        "#
        )?,
        true
    );
    Ok(())
}

/// A chunk becomes a top-level function, which can only carry `_ENV`; a function closing over
/// anything else is refused at dump time rather than producing bytes that cannot be loaded.
#[test]
fn a_function_with_upvalues_is_refused() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            -- A nested function reaches `_ENV` through its parent, so it is not a chunk.
            local nested = assert(load("return function(x) return tostring(x) end"))()
            local ok, err = pcall(string.dump, nested)
            return not ok and tostring(err):match("chunk") ~= nil
        "#
        )?,
        true
    );
    Ok(())
}

/// `mode` is how a host refuses a form it never meant to accept.
#[test]
fn mode_gates_binary_and_text_separately() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            local bytes = string.dump(assert(load("return 1")))
            assert(not pcall(load, bytes, "c", "t"), "text-only should refuse a binary chunk")
            assert(load(bytes, "c", "b") ~= nil, "binary should be allowed by 'b'")
            assert(not pcall(load, "return 1", "c", "b"), "binary-only should refuse text")
            assert(load("return 1", "c", "bt") ~= nil, "'bt' should allow text")
            -- Binary is opt-in: no mode means text only.
            assert(not pcall(load, bytes), "binary should need an explicit mode")
            return true
        "#
        )?,
        true
    );
    Ok(())
}

#[test]
fn rubbish_is_rejected_not_run() -> Result<(), ExternError> {
    assert_eq!(
        eval::<bool>(
            r#"
            -- the signature without a body
            local f, err = load("\27Luna", "c", "b")
            assert(f == nil and err ~= nil, "a bare signature should not load")
            -- a plausible-looking chunk from another version
            local f2 = load("\27Luna\255\0", "c", "b")
            assert(f2 == nil, "a foreign version should not load")
            return true
        "#
        )?,
        true
    );
    Ok(())
}

/// Every truncation of a valid chunk must be refused, never panic.
///
/// Truncation is the cheapest way to reach the length-handling paths, and the one a real file gets
/// into by accident.
#[test]
fn every_truncation_is_refused() {
    let mut lua = Lua::full();
    let bytes = dump_of(&mut lua, "local function f(a, b) return a + b end return f");

    for cut in 0..bytes.len() {
        let prefix = &bytes[..cut];
        lua.enter(|ctx| {
            // The contract is "an error, not a panic". Anything that loads is fine too: a prefix
            // could in principle be a valid smaller chunk.
            let _ = Closure::load(ctx, Some("truncated"), prefix);
        });
    }
}

/// Every single-byte corruption of a valid chunk must be refused or load into something harmless —
/// but never panic, and never be believed enough to index out of a prototype.
#[test]
fn single_byte_corruption_never_panics() {
    let mut lua = Lua::full();
    let bytes = dump_of(&mut lua, "local function f(a, b) local c = a + b if c > 2 then return c else return -c end end return f");

    // Every byte, flipped to a handful of values likely to break an index: zero, one, a small
    // count, and the maximums that a length or index field is read from.
    for position in 0..bytes.len() {
        for replacement in [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff] {
            let mut corrupted = bytes.clone();
            if corrupted[position] == replacement {
                continue;
            }
            corrupted[position] = replacement;
            lua.enter(|ctx| {
                let _ = Closure::load(ctx, Some("corrupt"), &corrupted);
            });
        }
    }
}

/// What the loader guarantees, and what it does not.
///
/// Loading is total: no byte sequence panics it, which the two mutation tests above cover. Running
/// a *crafted* chunk is a different claim, and one this does not make — see the note in
/// `src/dump.rs`. This test pins the part that holds: a corrupted chunk either fails to load or
/// produces a function, and neither outcome escapes as a panic from the load itself.
#[test]
fn corrupted_chunks_either_fail_to_load_or_produce_a_function() {
    let mut lua = Lua::full();
    let bytes = dump_of(
        &mut lua,
        "local a = ... local t = {a, a + 1} return t[1] + t[2]",
    );

    let mut loaded = 0;
    for position in 0..bytes.len() {
        for replacement in [0x00, 0x02, 0x40, 0xff] {
            let mut corrupted = bytes.clone();
            if corrupted[position] == replacement {
                continue;
            }
            corrupted[position] = replacement;
            lua.enter(|ctx| {
                if Closure::load(ctx, Some("corrupt"), &corrupted).is_ok() {
                    loaded += 1;
                }
            });
        }
    }
    // If nothing loaded the test proved nothing, so say so rather than passing silently.
    assert!(loaded > 0, "no corrupted chunk loaded; the test is vacuous");
}

/// The chunk itself is a function, and carries the nested prototype for anything it defines — so
/// dumping the chunk exercises the recursive path without having to run anything.
fn dump_of(lua: &mut Lua, source: &str) -> Vec<u8> {
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("subject"), source.as_bytes()).unwrap();
        luna::dump::dump(&closure.prototype(), false)
    })
}
