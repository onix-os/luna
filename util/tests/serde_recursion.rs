//! A cyclic table must error, not take the process down with it.
//!
//! `IgnoredAny` walks the whole structure without needing a concrete target type, which is exactly
//! the traversal that used to recurse forever.

use luna::{Closure, Executor, ExternError, Lua, Value};
use serde::de::IgnoredAny;

fn returned_value(
    source: &[u8],
    f: impl for<'gc> FnOnce(luna::Context<'gc>, Value<'gc>),
) -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.finish(&executor).unwrap();
    lua.try_enter(|ctx| {
        let value: Value = ctx.fetch(&executor).take_result::<Value>(ctx)??;
        f(ctx, value);
        Ok(())
    })
}

#[test]
fn a_self_referential_table_errors_instead_of_overflowing() -> Result<(), ExternError> {
    returned_value(
        &br#"
            local t = {}
            t.self = t
            return t
        "#[..],
        |ctx, value| {
            let err = luna_util::serde::from_value::<IgnoredAny>(ctx, value)
                .expect_err("a cyclic table must not deserialize");
            assert!(
                err.to_string().contains("nests deeper"),
                "unexpected error: {err}"
            );
        },
    )
}

/// Two tables pointing at each other is the same problem one step removed.
#[test]
fn mutually_referential_tables_error_too() -> Result<(), ExternError> {
    returned_value(
        &br#"
            local a, b = {}, {}
            a.other = b
            b.other = a
            return a
        "#[..],
        |ctx, value| {
            assert!(luna_util::serde::from_value::<IgnoredAny>(ctx, value).is_err());
        },
    )
}

/// Honest nesting well inside the limit still works.
#[test]
fn deep_but_finite_nesting_still_works() -> Result<(), ExternError> {
    returned_value(
        &br#"
            local root = {}
            local node = root
            for _ = 1, 40 do node.child = {} node = node.child end
            node.leaf = true
            return root
        "#[..],
        |ctx, value| {
            luna_util::serde::from_value::<IgnoredAny>(ctx, value)
                .expect("finite nesting must decode");
        },
    )
}
