//! A Lua value serializing out into a concrete serde format.

use luna::{Lua, Table, Value};
use luna_util::serde::SerializeValue;

fn json(build: impl for<'gc> FnOnce(luna::Context<'gc>) -> Value<'gc>) -> String {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let value = build(ctx);
        serde_json::to_string(&SerializeValue::new(ctx, value)).unwrap()
    })
}

#[test]
fn scalars_round_out() {
    assert_eq!(json(|_| Value::Nil), "null");
    assert_eq!(json(|_| Value::Boolean(true)), "true");
    assert_eq!(json(|_| Value::Integer(-7)), "-7");
    assert_eq!(json(|_| Value::Number(0.5)), "0.5");
    assert_eq!(json(|ctx| ctx.intern(b"hi").into()), r#""hi""#);
}

/// A `1..=n` table is an array; anything else is a map. This is the only real judgement call in
/// the impl, so it is the one worth pinning down.
#[test]
fn sequences_are_arrays_and_the_rest_are_maps() {
    assert_eq!(
        json(|ctx| {
            let t = Table::new(&ctx);
            for i in 1..=3 {
                t.set(ctx, i, i * 10).unwrap();
            }
            t.into()
        }),
        "[10,20,30]"
    );

    assert_eq!(
        json(|ctx| {
            let t = Table::new(&ctx);
            t.set(ctx, "name", "luna").unwrap();
            t.into()
        }),
        r#"{"name":"luna"}"#
    );

    // A gap makes it no longer a sequence, so it falls back to the map form.
    assert_eq!(
        json(|ctx| {
            let t = Table::new(&ctx);
            t.set(ctx, 1, "a").unwrap();
            t.set(ctx, 3, "c").unwrap();
            t.into()
        }),
        r#"{"1":"a","3":"c"}"#
    );
}

#[test]
fn nesting_is_preserved() {
    assert_eq!(
        json(|ctx| {
            let inner = Table::new(&ctx);
            inner.set(ctx, 1, 1).unwrap();
            inner.set(ctx, 2, 2).unwrap();
            let outer = Table::new(&ctx);
            outer.set(ctx, "xs", inner).unwrap();
            outer.into()
        }),
        r#"{"xs":[1,2]}"#
    );
}

/// The deserializer's cycle crash was a real bug; the serializer must not reintroduce it.
#[test]
fn a_cycle_errors_rather_than_recursing_forever() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let t = Table::new(&ctx);
        t.set(ctx, "self", t).unwrap();
        let err = serde_json::to_string(&SerializeValue::new(ctx, t.into())).unwrap_err();
        assert!(err.to_string().contains("cyclic"), "got {err}");
    });
}

#[test]
fn a_function_has_no_representation() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let f: Value =
            luna::Callback::from_fn(&ctx, |_, _, _| Ok(luna::CallbackReturn::Return)).into();
        let err = serde_json::to_string(&SerializeValue::new(ctx, f)).unwrap_err();
        assert!(err.to_string().contains("function"), "got {err}");
    });
}
