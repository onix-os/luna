//! The Rust helpers the functional `lib` is built on: data encodings, deep freezing and closure
//! identity and chunk names. `lib.lua` receives them as its chunk argument and keeps them private.

use ottavino_gc_arena::Gc;

use crate::{Callback, CallbackReturn, Context, Function, Table, Value};

use super::{freeze, message, Data, DataError};

fn data_message<'gc>(ctx: Context<'gc>, error: DataError) -> crate::Error<'gc> {
    message(ctx, format!("value{}: {}", error.path, error.message))
}

pub(super) fn natives<'gc>(ctx: Context<'gc>) -> Table<'gc> {
    let natives = Table::new(&ctx);
    natives.set_field(
        ctx,
        "encode",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (value, format, pretty): (Value, crate::String, Option<bool>) =
                stack.consume(ctx)?;
            let data = Data::from_value(ctx, value).map_err(|error| data_message(ctx, error))?;
            let text = match format.as_bytes() {
                b"json" => data.to_json(pretty.unwrap_or(false)),
                b"toml" => data.to_toml(),
                b"yaml" => data.to_yaml(),
                b"ini" => data.to_ini(),
                _ => return Err(message(ctx, "unknown encoding".to_owned())),
            }
            .map_err(|error| data_message(ctx, error))?;
            stack.replace(ctx, ctx.intern(text.as_bytes()));
            Ok(CallbackReturn::Return)
        }),
    );
    natives.set_field(
        ctx,
        "decode",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (text, format): (crate::String, crate::String) = stack.consume(ctx)?;
            let text = std::str::from_utf8(text.as_bytes())
                .map_err(|_| message(ctx, "the text is not UTF-8".to_owned()))?;
            let data = match format.as_bytes() {
                b"json" => Data::from_json(text),
                b"toml" => Data::from_toml(text),
                _ => return Err(message(ctx, "unknown encoding".to_owned())),
            }
            .map_err(|error| message(ctx, error.to_string()))?;
            let value = data
                .to_value(ctx)
                .map_err(|error| message(ctx, error.to_string()))?;
            stack.replace(ctx, value);
            Ok(CallbackReturn::Return)
        }),
    );
    natives.set_field(
        ctx,
        "freeze",
        Callback::from_fn(&ctx, |ctx, _, stack| {
            freeze(ctx, stack.get(0));
            Ok(CallbackReturn::Return)
        }),
    );
    natives.set_field(
        ctx,
        "same_code",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let same = match (stack.get(0), stack.get(1)) {
                (Value::Function(Function::Closure(a)), Value::Function(Function::Closure(b))) => {
                    Gc::as_ptr(a.prototype()) == Gc::as_ptr(b.prototype())
                }
                _ => false,
            };
            stack.replace(ctx, same);
            Ok(CallbackReturn::Return)
        }),
    );
    natives.set_field(
        ctx,
        "captures",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let captures = Table::new(&ctx);
            if let Value::Function(Function::Closure(closure)) = stack.get(0) {
                for (index, upvalue) in closure.upvalues().iter().enumerate() {
                    let value = match upvalue.get().get() {
                        crate::closure::UpValueState::Closed(value) => value,
                        crate::closure::UpValueState::Open(open) => open.get(&ctx),
                    };
                    captures.set(ctx, index as i64 + 1, value)?;
                }
            }
            stack.replace(ctx, captures);
            Ok(CallbackReturn::Return)
        }),
    );
    natives.set_field(
        ctx,
        "source",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let source = match stack.get(0) {
                Value::Function(Function::Closure(closure)) => {
                    Value::String(closure.prototype().chunk_name)
                }
                _ => Value::Nil,
            };
            stack.replace(ctx, source);
            Ok(CallbackReturn::Return)
        }),
    );
    natives
}
