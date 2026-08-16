//! Rust-side conversions and the prelude.

use std::collections::HashMap;

use luna::prelude::*;
use luna::{Closure, Either, ExternError};

fn run(source: &str, setup: impl FnOnce(Context<'_>)) -> Result<bool, ExternError> {
    let mut lua = Lua::core();
    lua.enter(setup);
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

/// A callback that legitimately accepts a string or a number can now say so in its signature.
#[test]
fn either_accepts_both_arms() -> Result<(), ExternError> {
    assert!(run(
        r#"return describe(7) == "int:7" and describe("hi") == "str:hi""#,
        |ctx| {
            let describe = Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let which: Either<i64, luna::String> = stack.consume(ctx)?;
                let text = match which {
                    Either::Left(n) => format!("int:{n}"),
                    Either::Right(s) => format!("str:{}", s.display_lossy()),
                };
                stack.replace(ctx, text);
                Ok(CallbackReturn::Return)
            });
            ctx.set_global("describe", describe);
        }
    )?);
    Ok(())
}

#[test]
fn wide_integers_and_runtime_strings_convert() -> Result<(), ExternError> {
    assert!(run(
        r#"
            local a, b, c, d = wide()
            return a == 5 and math.type(a) == "integer"
                and math.type(b) == "float"
                and c == "runtime" and d == "x"
        "#,
        |ctx| {
            let wide = Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let owned = String::from("runtime");
                stack.replace(
                    ctx,
                    (
                        5usize,
                        // Past i64, so it becomes a float rather than failing.
                        u64::MAX,
                        owned.as_str(),
                        'x',
                    ),
                );
                Ok(CallbackReturn::Return)
            });
            ctx.set_global("wide", wide);
        }
    )?);
    Ok(())
}

#[test]
fn usize_converts_back_from_lua() -> Result<(), ExternError> {
    assert!(run(r#"return takes_index(3) == 6"#, |ctx| {
        let takes = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let n: usize = stack.consume(ctx)?;
            stack.replace(ctx, n * 2);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("takes_index", takes);
    })?);
    Ok(())
}

/// The prelude renames the types that would otherwise shadow `std`.
#[test]
fn the_prelude_does_not_shadow_std() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        let t = LuaTable::new(&ctx);
        t.set(ctx, "answer", 42).unwrap();
        // `String` here is still `std::string::String`.
        let owned: String = String::from("not shadowed");
        assert_eq!(owned.len(), 12);
        assert_eq!(t.get::<_, i64>(ctx, "answer").unwrap(), 42);
    });
}

#[test]
fn maps_still_round_trip() -> Result<(), ExternError> {
    assert!(run(r#"return count({ a = 1, b = 2 }) == 2"#, |ctx| {
        let count = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let m: HashMap<String, i64> = stack.consume(ctx)?;
            stack.replace(ctx, m.len());
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("count", count);
    })?);
    Ok(())
}

/// A typed payload as a callback argument, instead of repeating the downcast by hand.
#[test]
fn user_ref_reads_a_payload_directly() -> Result<(), ExternError> {
    struct Rect {
        w: i64,
        h: i64,
    }

    assert!(run(r#"return area(make()) == 12"#, |ctx| {
        let make = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud = luna::UserData::new_static(&ctx, Rect { w: 3, h: 4 });
            stack.replace(ctx, ud);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("make", make);

        let area = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let rect: luna::UserRef<Rect> = stack.consume(ctx)?;
            stack.replace(ctx, rect.w * rect.h);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("area", area);
    })?);
    Ok(())
}

/// The wrong payload type is a type error rather than a panic.
#[test]
fn user_ref_rejects_the_wrong_type() -> Result<(), ExternError> {
    struct A;
    struct B;

    assert!(run(r#"return pcall(wants_b, make_a()) == false"#, |ctx| {
        let make_a = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            stack.replace(ctx, luna::UserData::new_static(&ctx, A));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("make_a", make_a);

        let wants_b = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let _: luna::UserRef<B> = stack.consume(ctx)?;
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("wants_b", wants_b);
    })?);
    Ok(())
}
