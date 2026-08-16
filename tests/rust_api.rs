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
