//! The `lib` global of functional states: it never changes its arguments, so it works on frozen
//! data, and it gives the merging, list, map, equality, rewriting and schema tools configs need.

use luna::{functional, Callback, CallbackReturn, Closure, Executor, ExternError};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = common::functional();
    let executor = lua.try_enter(|ctx| {
        let env = functional::environment(ctx);
        env.set(
            ctx,
            "freeze",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let value = stack.get(0);
                functional::freeze(ctx, value);
                stack.replace(ctx, value);
                Ok(CallbackReturn::Return)
            }),
        )?;
        let closure = Closure::load_with_env(ctx, None, source.as_bytes(), env)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

#[test]
fn merging_works_on_frozen_data_without_changing_it() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local base = freeze({ name = "a", server = { port = 80, host = "h" }, tags = { 1, 2 } })
        local shallow = lib.merge(base, { server = { port = 81 } }, { extra = true })
        local deep = lib.deep_merge(base, { server = { port = 81 }, tags = { 3 } })
        return shallow.server.port == 81 and shallow.server.host == nil and shallow.extra
            and deep.server.port == 81 and deep.server.host == "h" and deep.name == "a"
            and lib.equal(deep.tags, { 3 })
            and base.server.port == 80 and table.isfrozen(base)
            and not table.isfrozen(deep)
    "#
    )?);
    Ok(())
}

#[test]
fn list_helpers() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local xs = freeze({ 3, 1, 2, 3 })
        local function even(x) return x % 2 == 0 end
        return lib.equal(lib.map(xs, function(x, i) return x * i end), { 3, 2, 6, 12 })
            and lib.equal(lib.filter(xs, function(x) return x > 1 end), { 3, 2, 3 })
            and lib.fold(xs, 0, function(acc, x) return acc + x end) == 9
            and lib.equal(lib.concat({ 1 }, {}, { 2, 3 }), { 1, 2, 3 })
            and lib.equal(lib.flatten({ { 1 }, { 2, 3 } }), { 1, 2, 3 })
            and lib.equal(lib.range(2, 4), { 2, 3, 4 }) and next(lib.range(3, 2)) == nil
            and lib.find(xs, even) == 2 and select(2, lib.find(xs, even)) == 3
            and lib.any(xs, even) and not lib.all(xs, even)
            and lib.contains({ { 1 } }, { 1 }) and not lib.contains(xs, 9)
            and lib.equal(lib.unique(xs), { 3, 1, 2 })
            and lib.equal(lib.sort(xs), { 1, 2, 3, 3 }) and xs[1] == 3
            and lib.equal(lib.group_by(xs, even), { [true] = { 2 }, [false] = { 3, 1, 3 } })
            and lib.is_list(xs) and not lib.is_list({}) and not lib.is_list({ a = 1 })
    "#
    )?);
    Ok(())
}

#[test]
fn map_helpers_follow_canonical_order() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local m = freeze({ b = 2, a = 1, c = 3 })
        return lib.equal(lib.keys(m), { "a", "b", "c" })
            and lib.equal(lib.values(m), { 1, 2, 3 })
            and lib.equal(lib.map_values(m, function(v, k) return k .. v end),
                { a = "a1", b = "b2", c = "c3" })
            and lib.equal(lib.filter_values(m, function(v) return v ~= 2 end), { a = 1, c = 3 })
            and lib.equal(lib.from_pairs({ { "x", 1 }, { "y", 2 } }), { x = 1, y = 2 })
    "#
    )?);
    Ok(())
}

#[test]
fn equality_and_copies_are_structural() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local a = { 1, { x = "y" }, n = { m = {} } }
        local cyclic = {} cyclic.self = cyclic
        local other = {} other.self = other
        local copy = lib.copy(freeze({ list = { 1, 2 }, meta = setmetatable({}, { __index = { z = 1 } }) }))
        copy.list[3] = 3
        return lib.equal(a, { 1, { x = "y" }, n = { m = {} } })
            and not lib.equal(a, { 1, { x = "z" }, n = { m = {} } })
            and not lib.equal({ 1 }, { 1, 2 }) and not lib.equal({ 1, 2 }, { 1 })
            and lib.equal(cyclic, other)
            and copy.list[3] == 3 and copy.meta.z == 1 and not table.isfrozen(copy)
    "#
    )?);
    Ok(())
}

/// `rewrite` is Guix's input rewriting: replace one dependency everywhere, rebuild what depends
/// on it, and share everything else.
#[test]
fn rewriting_replaces_by_identity_and_shares_the_rest() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local openssl = { name = "openssl", version = "3.0" }
        local zlib = { name = "zlib" }
        local curl = { name = "curl", deps = { openssl, zlib } }
        local git = { name = "git", deps = { curl, zlib } }
        local pkgs = freeze({ openssl = openssl, zlib = zlib, curl = curl, git = git })
        local patched = { name = "openssl", version = "3.4" }
        local new = lib.rewrite(pkgs, { [openssl] = patched })
        local by_function = lib.rewrite(pkgs, function(node)
            if node.name == "zlib" then return { name = "zlib-ng" } end
        end)
        return new.openssl == patched and new.curl.deps[1] == patched
            and new.git.deps[1] == new.curl and new.git.deps[1] ~= curl
            and new.zlib == zlib and new.curl.deps[2] == zlib
            and pkgs.curl.deps[1] == openssl
            and lib.rewrite(pkgs, {}) == pkgs
            and by_function.git.deps[2].name == "zlib-ng"
            and by_function.git.deps[2] == by_function.curl.deps[2]
    "#
    )?);
    Ok(())
}

#[test]
fn schemas_normalise_and_explain() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = lib.types
        local service = t.record({
            name = t.string,
            port = t.default(t.integer, 80),
            weight = t.optional(t.number),
            mode = t.enum({ "dev", "prod" }),
            hosts = t.list(t.one_of(t.string, t.record({ host = t.string }))),
            env = t.default(t.map(t.string), {}),
            check = t.optional(t.func),
            id = t.custom("a positive integer", function(v) return math.type(v) == "integer" and v > 0 end),
        }, nil)
        local ok = lib.check({ name = "a", mode = "dev", hosts = { "x", { host = "y" } }, id = 1 }, service)
        local function message(value, schema)
            local good, err = pcall(lib.check, value, schema, "svc")
            return not good and err
        end
        local open = t.record({ a = t.any }, { open = true })
        return ok.port == 80 and ok.weight == nil and lib.equal(ok.env, {})
            and ok.hosts[2].host == "y"
            and message({ name = 1 }, t.record({ name = t.string })) == "svc.name: expected a string, got number 1"
            and message({ }, t.record({ name = t.string })) == "svc.name: missing required string"
            and message({ name = "a", b = 1 }, t.record({ name = t.string })) == "svc.b: unknown field"
            and message({ "x", 2 }, t.list(t.string)) == "svc[2]: expected a string, got number 2"
            and message({ ["my key"] = 1 }, t.map(t.string)) == 'svc["my key"]: expected a string, got number 1'
            and message("stage", t.enum({ "dev", "prod" })) == 'svc: expected one of "dev", "prod", got string "stage"'
            and message(0, t.custom("a positive integer", function(v) return v > 0 end)) == "svc: expected a positive integer, got number 0"
            and message(true, t.one_of(t.string, t.integer)) == "svc: expected string or integer, got boolean true"
            and lib.check({ a = 1, b = 2 }, open).b == 2
    "#
    )?);
    Ok(())
}

mod common;
