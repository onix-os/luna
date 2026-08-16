//! `#[derive(FromValue, IntoValue)]`.

#![cfg(feature = "derive")]

use luna::{Closure, Executor, ExternError, FromValue, IntoValue, Lua, Table, Value};

#[derive(FromValue, IntoValue, Debug, PartialEq)]
struct Point {
    x: i64,
    y: i64,
}

#[derive(FromValue, IntoValue, Debug, PartialEq)]
struct Config {
    name: String,
    retries: i64,
    verbose: bool,
}

#[derive(FromValue, IntoValue, Debug, PartialEq)]
enum Level {
    Debug,
    Info,
    Warn,
}

#[test]
fn a_struct_becomes_a_table_keyed_by_field_name() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let value = Point { x: 3, y: 4 }.into_value(ctx);
        let Value::Table(table) = value else {
            panic!("expected a table, got {value:?}");
        };
        assert_eq!(table.get::<_, i64>(ctx, "x").unwrap(), 3);
        assert_eq!(table.get::<_, i64>(ctx, "y").unwrap(), 4);
    });
}

#[test]
fn a_table_becomes_a_struct() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let table = Table::new(&ctx);
        table.set_field(ctx, "x", 7);
        table.set_field(ctx, "y", 9);
        let point = Point::from_value(ctx, table.into()).unwrap();
        assert_eq!(point, Point { x: 7, y: 9 });
    });
}

#[test]
fn the_round_trip_is_lossless() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let original = Config {
            name: "luna".to_owned(),
            retries: 3,
            verbose: true,
        };
        let value = Config {
            name: original.name.clone(),
            retries: original.retries,
            verbose: original.verbose,
        }
        .into_value(ctx);
        assert_eq!(Config::from_value(ctx, value).unwrap(), original);
    });
}

/// A missing field fails rather than defaulting: silently producing a zero would be worse than an
/// error the host can see.
#[test]
fn a_missing_field_is_an_error() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let table = Table::new(&ctx);
        table.set_field(ctx, "x", 1);
        assert!(Point::from_value(ctx, table.into()).is_err());
    });
}

#[test]
fn a_wrong_type_is_an_error() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        assert!(Point::from_value(ctx, Value::Integer(5)).is_err());

        let table = Table::new(&ctx);
        table.set_field(ctx, "x", "not a number");
        table.set_field(ctx, "y", 2);
        assert!(Point::from_value(ctx, table.into()).is_err());
    });
}

#[test]
fn a_fieldless_enum_is_its_variant_name() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let value = Level::Warn.into_value(ctx);
        let Value::String(name) = value else {
            panic!("expected a string, got {value:?}");
        };
        assert_eq!(name.to_str().unwrap(), "Warn");

        assert_eq!(
            Level::from_value(ctx, ctx.intern(b"Info").into()).unwrap(),
            Level::Info
        );
        assert!(Level::from_value(ctx, ctx.intern(b"Nope").into()).is_err());
    });
}

/// The point of the derive: values cross into a script without the host writing conversions.
#[test]
fn derived_values_cross_into_lua() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        ctx.set_global("point", Point { x: 2, y: 5 });
        ctx.set_global("level", Level::Debug);
        let closure = Closure::load(
            ctx,
            Some("probe"),
            b"return point.x * point.y .. ':' .. level",
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.execute::<String>(&executor)?, "10:Debug");
    Ok(())
}

/// And back out again, through the same `FromMultiValue` path every callback uses.
#[test]
fn derived_values_come_back_from_lua() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), b"return { x = 11, y = 13 }")?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.execute::<Point>(&executor)?, Point { x: 11, y: 13 });
    Ok(())
}
