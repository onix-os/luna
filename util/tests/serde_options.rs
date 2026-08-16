//! The serde option surface, in all three directions: Rust → Lua, Lua → Rust, Lua → any format.

use luna::{Callback, CallbackReturn, Context, Lua, Table, Value};
use luna_util::serde::{
    from_value, from_value_with, to_value, to_value_with, DeOptions, SerOptions, SerializeValue,
    ValueOptions,
};

fn with_lua<R>(f: impl for<'gc> FnOnce(Context<'gc>) -> R) -> R {
    let mut lua = Lua::core();
    lua.enter(f)
}

/// `()` becomes the `unit` marker by default so a round trip can tell it from `nil`; the option
/// turns it into a plain `nil` for a Lua side that has no use for a marker userdata.
#[test]
fn serialize_unit_chooses_between_the_marker_and_nil() {
    with_lua(|ctx| {
        let default = to_value(ctx, &()).unwrap();
        assert!(
            matches!(default, Value::UserData(_)),
            "expected the unit marker, got {default:?}"
        );

        let plain = to_value_with(ctx, &(), SerOptions::default().serialize_unit(false)).unwrap();
        assert!(plain.is_nil(), "expected nil, got {plain:?}");
    });
}

#[test]
fn serialize_none_chooses_between_the_marker_and_nil() {
    with_lua(|ctx| {
        let none: Option<i64> = None;
        assert!(to_value(ctx, &none).unwrap().is_nil());

        let marked = to_value_with(ctx, &none, SerOptions::default().serialize_none(true)).unwrap();
        assert!(matches!(marked, Value::UserData(_)));
    });
}

/// The depth guard is what stops a cyclic table taking the process down; the option only moves
/// where it trips.
#[test]
fn deserialize_max_depth_is_configurable() {
    with_lua(|ctx| {
        // Four levels of nesting: {a = {a = {a = {a = 1}}}}
        let mut inner = Table::new(&ctx);
        inner.set(ctx, "a", 1).unwrap();
        for _ in 0..3 {
            let outer = Table::new(&ctx);
            outer.set(ctx, "a", inner).unwrap();
            inner = outer;
        }

        // A generous limit reads it.
        assert!(from_value::<serde_json::Value>(ctx, inner.into()).is_ok());

        // A tight one refuses, and says why.
        let err = from_value_with::<serde_json::Value>(
            ctx,
            inner.into(),
            DeOptions::default().max_depth(2),
        )
        .unwrap_err();
        assert!(err.to_string().contains("nests deeper than 2"), "got {err}");
    });
}

/// A cycle still fails rather than hanging, whatever the limit is.
#[test]
fn a_cyclic_table_is_refused_not_followed() {
    with_lua(|ctx| {
        let table = Table::new(&ctx);
        table.set(ctx, "self", table).unwrap();

        let err = from_value::<serde_json::Value>(ctx, table.into()).unwrap_err();
        assert!(err.to_string().contains("cyclic"), "got {err}");
    });
}

/// Lua-side helpers in an otherwise plain table: deny by default, skip on request.
#[test]
fn deserialize_can_tolerate_unsupported_types() {
    with_lua(|ctx| {
        let table = Table::new(&ctx);
        table.set(ctx, "name", "luna").unwrap();
        table
            .set(
                ctx,
                "helper",
                Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
            )
            .unwrap();

        // By default the function is fatal.
        assert!(from_value::<serde_json::Value>(ctx, table.into()).is_err());

        // Tolerated, it deserializes as a unit — null in JSON's vocabulary.
        let value: serde_json::Value = from_value_with(
            ctx,
            table.into(),
            DeOptions::default().deny_unsupported_types(false),
        )
        .unwrap();
        assert_eq!(value["name"], "luna");
        assert!(value["helper"].is_null());
    });
}

/// Sorted keys are what a content hash or a golden file needs: the same *set* of entries produces
/// the same bytes however the table was assembled.
#[test]
fn sort_keys_makes_output_order_independent() {
    let forwards = with_lua(|ctx| {
        let t = Table::new(&ctx);
        for key in ["alpha", "beta", "gamma"] {
            t.set(ctx, key, 1).unwrap();
        }
        serde_json::to_string(&SerializeValue::with_options(
            ctx,
            t.into(),
            ValueOptions::default().sort_keys(true),
        ))
        .unwrap()
    });

    let backwards = with_lua(|ctx| {
        let t = Table::new(&ctx);
        for key in ["gamma", "beta", "alpha"] {
            t.set(ctx, key, 1).unwrap();
        }
        serde_json::to_string(&SerializeValue::with_options(
            ctx,
            t.into(),
            ValueOptions::default().sort_keys(true),
        ))
        .unwrap()
    });

    assert_eq!(forwards, backwards);
    assert_eq!(forwards, r#"{"alpha":1,"beta":1,"gamma":1}"#);
}

/// Without the option, insertion order is preserved — which is stable, but only for a table built
/// the same way twice.
#[test]
fn insertion_order_is_the_default() {
    let json = with_lua(|ctx| {
        let t = Table::new(&ctx);
        for key in ["gamma", "alpha", "beta"] {
            t.set(ctx, key, 1).unwrap();
        }
        serde_json::to_string(&SerializeValue::new(ctx, t.into())).unwrap()
    });
    assert_eq!(json, r#"{"gamma":1,"alpha":1,"beta":1}"#);
}

/// Mixed key types still get a total order, so sorting cannot panic or vary run to run.
#[test]
fn sorting_handles_mixed_key_types() {
    let json = with_lua(|ctx| {
        let t = Table::new(&ctx);
        t.set(ctx, "zebra", 1).unwrap();
        t.set(ctx, 2, 1).unwrap();
        t.set(ctx, "apple", 1).unwrap();
        t.set(ctx, 10, 1).unwrap();
        serde_json::to_string(&SerializeValue::with_options(
            ctx,
            t.into(),
            ValueOptions::default().sort_keys(true),
        ))
        .unwrap()
    });
    // Numbers before strings, and numerically rather than lexically: 2 before 10.
    assert_eq!(json, r#"{"2":1,"10":1,"apple":1,"zebra":1}"#);
}

/// Serializing *out* has the same choice about Lua-only values.
#[test]
fn serializing_out_can_skip_unsupported_types() {
    with_lua(|ctx| {
        let t = Table::new(&ctx);
        t.set(ctx, "name", "luna").unwrap();
        t.set(
            ctx,
            "helper",
            Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
        )
        .unwrap();

        // Denied by default.
        assert!(serde_json::to_string(&SerializeValue::new(ctx, t.into())).is_err());

        // Skipped on request, leaving the data behind.
        let json = serde_json::to_string(&SerializeValue::with_options(
            ctx,
            t.into(),
            ValueOptions::default().deny_unsupported_types(false),
        ))
        .unwrap();
        assert_eq!(json, r#"{"name":"luna"}"#);
    });
}

/// The outgoing depth limit is configurable too, and defaults to the same 128.
#[test]
fn serializing_out_respects_max_depth() {
    with_lua(|ctx| {
        let mut inner = Table::new(&ctx);
        inner.set(ctx, 1, 1).unwrap();
        for _ in 0..4 {
            let outer = Table::new(&ctx);
            outer.set(ctx, 1, inner).unwrap();
            inner = outer;
        }

        assert!(serde_json::to_string(&SerializeValue::new(ctx, inner.into())).is_ok());

        let err = serde_json::to_string(&SerializeValue::with_options(
            ctx,
            inner.into(),
            ValueOptions::default().max_depth(2),
        ))
        .unwrap_err();
        assert!(err.to_string().contains("deeper than 2"), "got {err}");
    });
}
