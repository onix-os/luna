use super::*;
use crate::{Lua, UserData};

const METHODS: [MetaMethod; 15] = [
    MetaMethod::Add,
    MetaMethod::Sub,
    MetaMethod::Mul,
    MetaMethod::Div,
    MetaMethod::IDiv,
    MetaMethod::Mod,
    MetaMethod::Pow,
    MetaMethod::BAnd,
    MetaMethod::BOr,
    MetaMethod::BXor,
    MetaMethod::Shl,
    MetaMethod::Shr,
    MetaMethod::Lt,
    MetaMethod::Le,
    MetaMethod::Concat,
];

fn same_value<'gc>(left: Value<'gc>, right: Value<'gc>) {
    match (left, right) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::String(a), Value::String(b)) => assert_eq!(a, b),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        _ => panic!("different values: {left:?}, {right:?}"),
    }
}

fn compare<'gc>(ctx: Context<'gc>, lhs: Value<'gc>, rhs: Value<'gc>, method: MetaMethod) {
    let object = |value| matches!(value, Value::Table(_) | Value::UserData(_));
    let expected = if object(lhs) || object(rhs) {
        let selected = if object(lhs) {
            get_metamethod(ctx, lhs, method)
        } else {
            None
        }
        .or_else(|| {
            if object(rhs) {
                get_metamethod(ctx, rhs, method)
            } else {
                None
            }
        });
        match selected {
            Some(value) => call(ctx, value)
                .map(|function| {
                    MetaResult::Call(MetaCall {
                        function,
                        args: [lhs, rhs],
                    })
                })
                .map_err(|error| MetaOperatorError::Call(method, error)),
            None => Err(MetaOperatorError::Binary(
                method,
                lhs.type_name(),
                rhs.type_name(),
            )),
        }
    } else {
        Ok(MetaResult::Value(Value::Integer(71)))
    };
    let actual = meta_metaop(ctx, lhs, rhs, method, |_, _, _| Some(Value::Integer(71)));
    match (actual, expected) {
        (Ok(MetaResult::Call(actual)), Ok(MetaResult::Call(expected))) => {
            assert_eq!(actual.function, expected.function);
            same_value(actual.args[0], lhs);
            same_value(actual.args[1], rhs);
        }
        (Ok(MetaResult::Value(actual)), Ok(MetaResult::Value(expected))) => {
            same_value(actual, expected)
        }
        (Err(actual), Err(expected)) => assert_eq!(actual.to_string(), expected.to_string()),
        (actual, expected) => panic!("dispatch mismatch: {actual:?}, {expected:?}"),
    }
}

#[test]
fn binary_dispatch_preserves_selection_errors_and_argument_identity() {
    Lua::empty().enter(|ctx| {
        let left = Callback::from_fn(&ctx, |_, _, _| {
            panic!("metamethod executed during selection")
        });
        let right = Callback::from_fn(&ctx, |_, _, _| {
            panic!("metamethod executed during selection")
        });
        for method in METHODS {
            let mut values = vec![
                Value::Nil,
                Value::Integer(3),
                Value::Number(-0.0),
                ctx.intern(b"3").into(),
            ];
            for selected in [Value::Nil, left.into(), right.into(), Value::Integer(9)] {
                let metatable = Table::new(&ctx);
                metatable
                    .set_raw(&ctx, method.into_value(ctx), selected)
                    .unwrap();
                let table = Table::new(&ctx);
                table.set_metatable(ctx, Some(metatable));
                let userdata = UserData::new_static(&ctx, ());
                userdata.set_metatable(ctx, Some(metatable));
                values.extend([Value::Table(table), Value::UserData(userdata)]);
            }
            for &lhs in &values {
                for &rhs in &values {
                    compare(ctx, lhs, rhs, method);
                }
            }
        }
    });
}

#[test]
fn binary_dispatch_observes_live_metatable_replacement() {
    Lua::empty().enter(|ctx| {
        let left = Table::new(&ctx);
        let right = Table::new(&ctx);
        let left_meta = Table::new(&ctx);
        let right_meta = Table::new(&ctx);
        left.set_metatable(ctx, Some(left_meta));
        right.set_metatable(ctx, Some(right_meta));
        let callback = Callback::from_fn(&ctx, |_, _, _| {
            panic!("metamethod executed during selection")
        });
        for method in METHODS {
            right_meta
                .set_raw(&ctx, method.into_value(ctx), callback.into())
                .unwrap();
            for value in [
                Value::Nil,
                callback.into(),
                Value::Boolean(false),
                Value::Integer(0),
            ] {
                left_meta
                    .set_raw(&ctx, method.into_value(ctx), value)
                    .unwrap();
                compare(ctx, left.into(), right.into(), method);
                compare(ctx, right.into(), left.into(), method);
            }
            left.set_metatable(ctx, None);
            compare(ctx, left.into(), right.into(), method);
            left.set_metatable(ctx, Some(left_meta));
        }
    });
}
