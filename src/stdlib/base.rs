use std::pin::Pin;

use ottavino_gc_arena::Collect;

use crate::{
    meta_ops::{self, MetaResult},
    table::NextValue,
    BoxSequence, Callback, CallbackReturn, Closure, Context, Error, Execution, IntoValue,
    MetaMethod, Sequence, SequencePoll, Stack, String, Table, TypeError, Value, Variadic,
};

pub fn load_base<'gc>(ctx: Context<'gc>) {
    ctx.set_global(
        "tonumber",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            use crate::compiler::string_utils::{read_neg, trim_whitespace};

            fn extract_number_data(bytes: &[u8]) -> (&[u8], bool) {
                let bytes = trim_whitespace(bytes);
                let (is_neg, bytes) = read_neg(bytes);
                (bytes, is_neg)
            }

            if stack.is_empty() {
                Err("Missing argument(s) to tonumber".into_value(ctx))?
            } else if stack.len() == 1 || stack.get(1).is_nil() {
                let prenumber = stack.consume::<Value>(ctx)?;
                stack.replace(ctx, prenumber.to_numeric().unwrap_or(Value::Nil));
            } else {
                let (value, base) = stack.consume::<(Value, i64)>(ctx)?;
                // Avoid implicitly converting value to a string
                let s = match value {
                    Value::String(s) => s,
                    _ => {
                        return Err(TypeError {
                            expected: "string",
                            found: value.type_name(),
                        }
                        .into())
                    }
                };
                if !(2..=36).contains(&base) {
                    Err("base out of range".into_value(ctx))?;
                }
                let (bytes, is_neg) = extract_number_data(s.as_bytes());
                let result = bytes
                    .iter()
                    .map(|b| {
                        if b.is_ascii_digit() {
                            Some((*b - b'0') as i64)
                        } else if b.is_ascii_lowercase() {
                            Some((*b - b'a') as i64 + 10)
                        } else if b.is_ascii_uppercase() {
                            Some((*b - b'A') as i64 + 10)
                        } else {
                            None
                        }
                    })
                    .try_fold(0i64, |acc, v| match v {
                        Some(v) if v < base => Some(acc.wrapping_mul(base).wrapping_add(v)),
                        _ => None,
                    })
                    .map(|v| if is_neg { v.wrapping_neg() } else { v });
                stack.replace(ctx, result.map(Value::Integer).unwrap_or(Value::Nil));
            }

            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global(
        "tostring",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            if stack.is_empty() {
                Err("Bad argument to tostring".into_value(ctx).into())
            } else {
                match meta_ops::tostring(ctx, stack.get(0))? {
                    MetaResult::Value(v) => {
                        stack[0] = v;
                        stack.drain(1..);
                        Ok(CallbackReturn::Return)
                    }
                    MetaResult::Call(call) => {
                        stack.replace(ctx, Variadic(call.args));
                        Ok(CallbackReturn::Call {
                            function: call.function,
                            then: Some(BoxSequence::new(&ctx, CheckToString)),
                        })
                    }
                }
            }
        }),
    );

    ctx.set_global(
        "error",
        Callback::from_fn(&ctx, |_, _, stack| Err(stack.get(0).into())),
    );

    ctx.set_global(
        "assert",
        Callback::from_fn(&ctx, |ctx, _, stack| {
            if stack.get(0).to_bool() {
                Ok(CallbackReturn::Return)
            } else if stack.get(1).is_nil() {
                Err("assertion failed!".into_value(ctx).into())
            } else {
                Err(stack.get(1).into())
            }
        }),
    );

    ctx.set_global(
        "pcall",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let function = meta_ops::call(ctx, stack.get(0))?;
            stack.pop_front();
            Ok(CallbackReturn::Call {
                function,
                then: Some(BoxSequence::new(&ctx, PCall)),
            })
        }),
    );

    // Reads a file the way `luaL_loadfile` does, BOM and shebang included — `crate::io` has done
    // that since before anything called it.
    fn read_chunk_file(path: &str) -> Result<Vec<u8>, std::io::Error> {
        let mut reader = crate::io::buffered_read(std::fs::File::open(path)?)?;
        let mut source = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut source)?;
        Ok(source)
    }

    ctx.set_global(
        "loadfile",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (path, _mode, env): (String, Option<Value>, Option<Value>) = stack.consume(ctx)?;
            let path = path.display_lossy().to_string();

            let env = match env {
                None | Some(Value::Nil) => ctx.globals(),
                Some(Value::Table(t)) => t,
                Some(_) => {
                    return Err("bad argument #3 to 'loadfile' (table expected)"
                        .into_value(ctx)
                        .into())
                }
            };

            match read_chunk_file(&path) {
                Ok(source) => match Closure::load_with_env(ctx, Some(&path), &source, env) {
                    Ok(closure) => stack.replace(ctx, closure),
                    Err(err) => {
                        let msg = ctx.intern(err.to_string().as_bytes());
                        stack.replace(ctx, (Value::Nil, msg));
                    }
                },
                Err(err) => {
                    let msg = ctx.intern(format!("cannot open {path}: {err}").as_bytes());
                    stack.replace(ctx, (Value::Nil, msg));
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    // Unlike `loadfile`, a failure here is raised rather than returned, and the chunk is called
    // with whatever extra arguments were passed.
    ctx.set_global(
        "dofile",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let path: String = stack.from_front(ctx)?;
            let path = path.display_lossy().to_string();

            let source = read_chunk_file(&path)
                .map_err(|err| format!("cannot open {path}: {err}").into_value(ctx))?;
            let closure = Closure::load(ctx, Some(&path), &source)
                .map_err(|err| err.to_string().into_value(ctx))?;

            Ok(CallbackReturn::Call {
                function: closure.into(),
                then: None,
            })
        }),
    );

    ctx.set_global(
        "xpcall",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let function = meta_ops::call(ctx, stack.get(0))?;
            let handler = meta_ops::call(ctx, stack.get(1))?;
            stack.pop_front();
            stack.pop_front();
            Ok(CallbackReturn::Call {
                function,
                then: Some(BoxSequence::new(
                    &ctx,
                    XPCall {
                        handler,
                        handled: false,
                    },
                )),
            })
        }),
    );

    ctx.set_global(
        "type",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            if stack.is_empty() {
                Err("Missing argument to type".into_value(ctx).into())
            } else {
                stack.replace(ctx, stack.get(0).type_name());
                Ok(CallbackReturn::Return)
            }
        }),
    );

    ctx.set_global(
        "select",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ind = stack.get(0);
            if let Some(n) = ind.to_integer() {
                if n >= 1 {
                    let last = (n as usize).min(stack.len());
                    stack.drain(0..last);
                    return Ok(CallbackReturn::Return);
                } else if n < 0 {
                    let inverse_index = n.unsigned_abs() as usize;
                    let len = stack.len();
                    if inverse_index < len {
                        stack.drain(0..len - inverse_index);
                        return Ok(CallbackReturn::Return);
                    }
                }
            }

            if matches!(ind, Value::String(s) if s == b"#") {
                stack.replace(ctx, stack.len() as i64 - 1);
                return Ok(CallbackReturn::Return);
            }

            Err("Bad argument to 'select'".into_value(ctx).into())
        }),
    );

    ctx.set_global(
        "rawget",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (table, key): (Table, Value) = stack.consume(ctx)?;
            stack.replace(ctx, table.get_value(ctx, key));
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global(
        "rawlen",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let table: Table = stack.consume(ctx)?;
            stack.replace(ctx, table.length());
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global(
        "rawset",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (table, key, value): (Table, Value, Value) = stack.consume(ctx)?;
            table.set(ctx, key, value)?;
            stack.replace(ctx, table);
            Ok(CallbackReturn::Return)
        }),
    );

    // The globals table under its conventional name. `_ENV` already works; this is the name that
    // scripts and probes reach for.
    ctx.set_global("_G", ctx.globals());

    // Raw identity, without consulting `__eq`.
    ctx.set_global(
        "rawequal",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (a, b): (Value, Value) = stack.consume(ctx)?;
            // `meta_ops::equal` only reaches for `__eq` once raw identity has already failed, so a
            // deferred call is itself the answer: not raw-equal.
            let equal = match meta_ops::equal(ctx, a, b)? {
                MetaResult::Value(v) => v.to_bool(),
                MetaResult::Call(_) => false,
            };
            stack.replace(ctx, equal);
            Ok(CallbackReturn::Return)
        }),
    );

    // Writes to stderr, like PUC-Rio's default warning handler. A host that wants its own sink
    // replaces this global, the same way it would replace `print`.
    ctx.set_global(
        "warn",
        Callback::from_fn(&ctx, |_ctx, _, mut stack| {
            let mut message = std::string::String::new();
            for i in 0..stack.len() {
                match stack.get(i) {
                    Value::String(s) => message.push_str(&s.display_lossy().to_string()),
                    other => {
                        return Err(TypeError {
                            expected: "string",
                            found: other.type_name(),
                        }
                        .into())
                    }
                }
            }
            // A message starting with "@" is a control message in PUC-Rio, not output.
            if !message.starts_with('@') {
                eprintln!("Lua warning: {message}");
            }
            stack.clear();
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global(
        "getmetatable",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            if let Value::Table(t) = stack.get(0) {
                stack.replace(ctx, t.metatable());
                Ok(CallbackReturn::Return)
            } else {
                Err("'getmetatable' can only be used on table types"
                    .into_value(ctx)
                    .into())
            }
        }),
    );

    ctx.set_global(
        "setmetatable",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (t, mt): (Table, Option<Table>) = stack.consume(ctx)?;
            t.set_metatable(&ctx, mt);
            stack.replace(ctx, t);
            Ok(CallbackReturn::Return)
        }),
    );

    fn next<'gc>(
        ctx: Context<'gc>,
        table: Table<'gc>,
        index: Value<'gc>,
    ) -> Result<(Value<'gc>, Value<'gc>), Value<'gc>> {
        match table.next(index) {
            NextValue::Found { key, value } => Ok((key, value)),
            NextValue::Last => Ok((Value::Nil, Value::Nil)),
            NextValue::NotFound => Err("invalid table key".into_value(ctx)),
        }
    }

    let next = Callback::from_fn(&ctx, |ctx, _, mut stack| {
        let (table, index): (Table, Value) = stack.consume(ctx)?;
        stack.replace(ctx, next(ctx, table, index)?);
        Ok(CallbackReturn::Return)
    });

    ctx.set_global("next", next);

    ctx.set_global(
        "pairs",
        Callback::from_fn_with(&ctx, next, move |next, ctx, _, mut stack| {
            let table = stack.get(0);
            if let Some(mt) = match table {
                Value::Table(t) => t.metatable(),
                Value::UserData(u) => u.metatable(),
                _ => None,
            } {
                /// Simply matches PUC-Rio behavior of returning the first 3 elements of the __pairs metacall
                #[derive(Collect)]
                #[collect(require_static)]
                struct PairsReturn;

                impl<'gc> Sequence<'gc> for PairsReturn {
                    fn poll(
                        self: Pin<&mut Self>,
                        _ctx: Context<'gc>,
                        _exec: Execution<'gc, '_>,
                        mut stack: Stack<'gc, '_>,
                    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                        if stack.len() > 3 {
                            stack.drain(3..);
                        }
                        Ok(SequencePoll::Return)
                    }
                }

                let pairs = mt.get_value(ctx, MetaMethod::Pairs);
                if !pairs.is_nil() {
                    let function = meta_ops::call(ctx, pairs)?;
                    stack.replace(ctx, (table, Value::Nil));
                    return Ok(CallbackReturn::Call {
                        function,
                        then: Some(BoxSequence::new(&ctx, PairsReturn)),
                    });
                }
            }

            stack.replace(ctx, (*next, table));
            Ok(CallbackReturn::Return)
        }),
    );

    let inext = Callback::from_fn(&ctx, |ctx, _, mut stack| {
        let (table, index): (Value, Option<i64>) = stack.consume(ctx)?;
        let next_index = index.unwrap_or(0).wrapping_add(1);
        Ok(match meta_ops::index(ctx, table, next_index.into())? {
            MetaResult::Value(v) => {
                if !v.is_nil() {
                    stack.extend([next_index.into(), v]);
                }
                CallbackReturn::Return
            }
            MetaResult::Call(call) => {
                #[derive(Collect)]
                #[collect(require_static)]
                struct INext(i64);

                impl<'gc> Sequence<'gc> for INext {
                    fn poll(
                        self: Pin<&mut Self>,
                        _ctx: Context<'gc>,
                        _exec: Execution<'gc, '_>,
                        mut stack: Stack<'gc, '_>,
                    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                        if !stack.get(0).is_nil() {
                            stack.push_front(self.0.into());
                        }
                        Ok(SequencePoll::Return)
                    }
                }

                stack.extend(call.args);
                CallbackReturn::Call {
                    function: call.function,
                    then: Some(BoxSequence::new(&ctx, INext(next_index))),
                }
            }
        })
    });

    ctx.set_global(
        "ipairs",
        Callback::from_fn_with(&ctx, inext, move |inext, ctx, _, mut stack| {
            stack.into_front(ctx, *inext);
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global(
        "collectgarbage",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            // Everything but "count" is a *request*: acting on the collector needs `&mut Lua`,
            // which no callback has, so the host carries it out when the slice ends.
            match stack.consume::<Option<String>>(ctx)? {
                Some(arg) if arg == "count" => {
                    stack.into_back(ctx, ctx.metrics().total_allocation() as f64 / 1024.0);
                }
                Some(arg) if arg == "collect" => {
                    ctx.request_gc(crate::GcRequest::Collect);
                    stack.into_back(ctx, 0);
                }
                Some(arg) if arg == "step" => {
                    ctx.request_gc(crate::GcRequest::Step);
                    stack.into_back(ctx, true);
                }
                Some(arg) if arg == "stop" => {
                    ctx.request_gc(crate::GcRequest::Stop);
                    stack.into_back(ctx, 0);
                }
                Some(arg) if arg == "restart" => {
                    ctx.request_gc(crate::GcRequest::Restart);
                    stack.into_back(ctx, 0);
                }
                Some(arg) if arg == "isrunning" => {
                    // The request has not been carried out yet, so answer from the arena: a
                    // stopped collector is one that is never owed anything.
                    stack.into_back(ctx, ctx.metrics().allocation_debt() > 0.0);
                }
                Some(_) => {
                    return Err("bad argument to 'collectgarbage'".into_value(ctx).into());
                }
                None => {
                    ctx.request_gc(crate::GcRequest::Collect);
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global("_VERSION", "luna");

    ctx.set_global(
        "load",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (chunk, chunk_name, mode, env) =
                stack.consume::<(Value, Option<Value>, Option<Value>, Option<Value>)>(ctx)?;

            let chunk = match chunk {
                Value::String(s) => s,
                // A reader function is the other form PUC-Rio accepts. Refusing it by name beats
                // accepting it and compiling something the caller did not write.
                Value::Function(_) => {
                    return Err(
                        "bad argument #1 to 'load' (reader functions are not supported)"
                            .into_value(ctx)
                            .into(),
                    );
                }
                _ => {
                    return Err("bad argument #1 to 'load' (string expected)"
                        .into_value(ctx)
                        .into());
                }
            };

            let chunk_name = match chunk_name {
                None | Some(Value::Nil) => None,
                Some(Value::String(s)) => Some(s.display_lossy().to_string()),
                Some(_) => {
                    return Err("bad argument #2 to 'load' (string expected)"
                        .into_value(ctx)
                        .into());
                }
            };

            // There is no bytecode loader, so "b" is refused rather than silently treated as
            // source. "t" and "bt" both mean "text is acceptable", which is all luna can do.
            match mode {
                None | Some(Value::Nil) => {}
                Some(Value::String(s)) => {
                    let mode = s.display_lossy().to_string();
                    if !mode.contains('t') {
                        return Err("bad argument #3 to 'load' (luna cannot load binary chunks)"
                            .into_value(ctx)
                            .into());
                    }
                }
                Some(_) => {
                    return Err("bad argument #3 to 'load' (string expected)"
                        .into_value(ctx)
                        .into());
                }
            }

            // The whole point of the fourth argument: a chunk loaded into a restricted table must
            // not be able to reach the real globals.
            let env = match env {
                None | Some(Value::Nil) => ctx.globals(),
                Some(Value::Table(t)) => t,
                Some(_) => {
                    return Err("bad argument #4 to 'load' (table expected)"
                        .into_value(ctx)
                        .into());
                }
            };

            match Closure::load_with_env(ctx, chunk_name.as_deref(), chunk.as_bytes(), env) {
                Ok(closure) => {
                    stack.replace(ctx, closure);
                    Ok(CallbackReturn::Return)
                }
                Err(e) => {
                    let err_str = ctx.intern(e.to_string().as_bytes());
                    stack.replace(ctx, (Value::Nil, err_str));
                    Ok(CallbackReturn::Return)
                }
            }
        }),
    );
}

#[derive(Collect)]
#[collect(require_static)]
struct CheckToString;

impl<'gc> Sequence<'gc> for CheckToString {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        match stack.get(0) {
            Value::String(_) => Ok(SequencePoll::Return),
            _ => Err("'__tostring' must return a string".into_value(ctx).into()),
        }
    }
}

/// `xpcall`'s message handler runs at the frame that intercepted the error, before unwinding
/// continues — which is the whole reason to prefer it over `pcall`.
#[derive(Collect)]
#[collect(no_drop)]
pub struct XPCall<'gc> {
    handler: crate::Function<'gc>,
    handled: bool,
}

impl<'gc> Sequence<'gc> for XPCall<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        // Reached either because the protected call returned, or because the handler we asked for
        // has just finished.
        stack.into_front(ctx, !self.handled);
        self.handled = false;
        Ok(SequencePoll::Return)
    }

    fn error(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        error: Error<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        self.handled = true;
        stack.replace(ctx, error.to_value(ctx));
        Ok(SequencePoll::Call {
            bottom: 0,
            function: self.handler,
        })
    }
}

#[derive(Collect)]
#[collect(require_static)]
pub struct PCall;

impl<'gc> Sequence<'gc> for PCall {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        stack.into_front(ctx, true);
        Ok(SequencePoll::Return)
    }

    fn error(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        error: Error<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        stack.replace(ctx, (false, error));
        Ok(SequencePoll::Return)
    }
}
