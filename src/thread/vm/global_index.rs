use super::*;

pub(super) fn constant<'gc>(
    ctx: Context<'gc>,
    table: Value<'gc>,
    key: Constant<String<'gc>>,
) -> Result<MetaResult<'gc, 2>, meta_ops::MetaOperatorError> {
    if let (Value::Table(table), Constant::String(key)) = (table, key) {
        let value = table.get_prepared_string(ctx, key, crate::table::PreparedStringHash::new(key));
        if !value.is_nil() {
            return Ok(MetaResult::Value(value));
        }
    }
    meta_ops::index(ctx, table, key.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same<'gc>(left: Value<'gc>, right: Value<'gc>) {
        match (left, right) {
            (Value::Nil, Value::Nil) => {}
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (Value::String(a), Value::String(b)) => assert!(ottavino_gc_arena::Gc::ptr_eq(
                a.into_inner(),
                b.into_inner()
            )),
            (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
            (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
            (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
            (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
            (a, b) => panic!("different values: {a:?} / {b:?}"),
        }
    }

    fn compare<'gc>(ctx: Context<'gc>, table: Value<'gc>, key: Constant<String<'gc>>) {
        match (
            meta_ops::index(ctx, table, key.into()),
            constant(ctx, table, key),
        ) {
            (Ok(MetaResult::Value(a)), Ok(MetaResult::Value(b))) => same(a, b),
            (Ok(MetaResult::Call(a)), Ok(MetaResult::Call(b))) => {
                same(a.function.into(), b.function.into());
                for (a, b) in a.args.into_iter().zip(b.args) {
                    same(a, b);
                }
            }
            (Err(a), Err(b)) => assert_eq!(format!("{a:?}"), format!("{b:?}")),
            (a, b) => panic!("different outcomes: {a:?} / {b:?}"),
        }
    }

    #[test]
    fn constant_globals_match_generic_values_calls_and_errors() {
        crate::Lua::full().enter(|ctx| {
            let key = ctx.intern(b"target");
            let equal = String::from_static(&ctx, b"target");
            let table = Table::new(&ctx);
            let meta = Table::new(&ctx);
            let fallback = Table::new(&ctx);
            fallback.set_field(ctx, "target", 93);
            let function =
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return));
            for index in [Value::Nil, fallback.into(), function.into(), 17.into()] {
                meta.set_field(ctx, "__index", index);
                for metatable in [None, Some(meta)] {
                    table.set_metatable(ctx, metatable);
                    for value in [
                        Value::Nil,
                        false.into(),
                        31.into(),
                        Value::Number(-0.0),
                        Value::Number(f64::NAN),
                        key.into(),
                        fallback.into(),
                        function.into(),
                    ] {
                        table.set_raw(&ctx, key.into(), value).unwrap();
                        for key in [
                            Constant::String(key),
                            Constant::String(equal),
                            Constant::String(ctx.intern(b"absent")),
                            Constant::Nil,
                            Constant::Boolean(false),
                            Constant::Integer(1),
                            Constant::Number(-0.0),
                        ] {
                            compare(ctx, table.into(), key);
                        }
                    }
                }
            }
            for table in [
                Value::Nil,
                false.into(),
                19.into(),
                key.into(),
                function.into(),
            ] {
                compare(ctx, table, Constant::String(key));
            }
        });
    }

    #[test]
    fn constant_globals_observe_mutations_weak_modes_and_cycles() {
        crate::Lua::full().enter(|ctx| {
            let key = Constant::String(ctx.intern(b"target"));
            let table = Table::new(&ctx);
            let meta = Table::new(&ctx);
            for mode in ["", "k", "v", "kv"] {
                meta.set_field(ctx, "__mode", ctx.intern(mode.as_bytes()));
                table.set_metatable(ctx, Some(meta));
                for value in [Value::Nil, 47.into(), false.into(), Table::new(&ctx).into()] {
                    table.set_field(ctx, "target", value);
                    compare(ctx, table.into(), key);
                }
            }
            table.set_field(ctx, "target", Value::Nil);
            meta.set_field(ctx, "__index", table);
            compare(ctx, table.into(), key);
            table.set_field(ctx, "target", -9);
            compare(ctx, table.into(), key);
            meta.set_field(ctx, "__index", Value::Nil);
            compare(ctx, table.into(), key);
        });
    }
}
