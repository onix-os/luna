//! Pure, deterministic states for configuration code.
//!
//! A state from [`crate::Lua::functional`] behaves like Starlark: a chunk may build and mutate the
//! tables it creates, and [`freeze`] makes everything it produced read-only once it finishes.
//! Nothing in the library reads the clock, the environment or the filesystem, and table
//! iteration follows a canonical key order instead of hash layout.

use std::{cmp::Ordering, collections::HashSet, pin::Pin};

use ottavino_gc_arena::{lock::RefLock, Collect, Gc, Rootable};

use crate::{
    closure::UpValueState, constant::cmp_int_float, stdlib::StringMetatable, BoxSequence, Callback,
    CallbackReturn, Closure, CompilerError, Context, Error, Execution, Function, IntoValue,
    MetaMethod, Sequence, SequencePoll, Stack, Table, Value,
};

/// Make `value` and everything reachable from it read-only.
///
/// Tables, the metatables of tables and userdata, and the values captured by closures are
/// frozen. Callbacks, userdata contents and threads are opaque to the runtime and left unchanged.
/// Captured locals are effectively final in a functional state, so freezing a closure while its
/// chunk is still running is as sound as freezing it afterwards.
pub fn freeze<'gc>(ctx: Context<'gc>, value: Value<'gc>) {
    let mut seen = HashSet::new();
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Table(table) => {
                if !seen.insert(Gc::as_ptr(table.into_inner()) as usize) {
                    continue;
                }
                table.set_readonly(&ctx, true);
                if let Some(metatable) = table.metatable() {
                    pending.push(metatable.into());
                }
                for (key, value) in table.iter(ctx) {
                    pending.push(key);
                    pending.push(value);
                }
            }
            Value::UserData(userdata) => {
                if let Some(metatable) = userdata.metatable() {
                    pending.push(metatable.into());
                }
            }
            Value::Function(Function::Closure(closure)) => {
                if !seen.insert(Gc::as_ptr(closure.into_inner()) as usize) {
                    continue;
                }
                for upvalue in closure.upvalues() {
                    pending.push(match upvalue.get().get() {
                        UpValueState::Closed(value) => value,
                        UpValueState::Open(open) => open.get(&ctx),
                    });
                }
            }
            _ => {}
        }
    }
}

/// A fresh environment for one chunk.
///
/// Reads fall through to the frozen globals and assignments land in the returned table, so a
/// chunk may define its own globals. Freeze the table with the chunk's results.
pub fn environment<'gc>(ctx: Context<'gc>) -> Table<'gc> {
    environment_over(ctx, ctx.globals())
}

fn environment_over<'gc>(ctx: Context<'gc>, parent: Table<'gc>) -> Table<'gc> {
    let metatable = Table::new(&ctx);
    metatable.set_field(ctx, "__index", parent);
    metatable.set_readonly(&ctx, true);
    let env = Table::new(&ctx);
    env.set_metatable(ctx, Some(metatable));
    env
}

/// An [`environment`] whose chunks can call `import(name)`.
///
/// `loader` maps a name to source text and is the only way the chunks reach other files, so it
/// decides which names exist and how they resolve. Each name is evaluated once, in its own
/// environment that can import further. Its first result and environment are frozen, and later
/// imports return that same value. Importing a name that is still being evaluated is an error.
pub fn import_environment<'gc>(
    ctx: Context<'gc>,
    loader: impl Fn(&str) -> Result<Vec<u8>, String> + 'static,
) -> Table<'gc> {
    let scope = Table::new(&ctx);
    let imports = Imports {
        scope,
        cache: Table::new(&ctx),
        loading: Table::new(&ctx),
    };
    let import = Callback::from_fn_with(&ctx, imports, move |imports, ctx, _, mut stack| {
        let name = match stack.get(0) {
            Value::String(name) => name,
            _ => {
                return Err("bad argument #1 to 'import' (string expected)"
                    .into_value(ctx)
                    .into())
            }
        };
        let key = Value::String(name);
        if let Value::Table(entry) = imports.cache.get_raw(&ctx, key) {
            stack.replace(ctx, entry.get_raw(&ctx, Value::Integer(1)));
            return Ok(CallbackReturn::Return);
        }
        let text = std::str::from_utf8(name.as_bytes())
            .map_err(|_| message(ctx, "import name must be UTF-8".to_owned()))?;
        if !imports.loading.get_raw(&ctx, key).is_nil() {
            return Err(message(ctx, format!("import cycle through '{text}'")));
        }
        let source = loader(text)
            .map_err(|error| message(ctx, format!("cannot import '{text}': {error}")))?;
        let env = environment_over(ctx, imports.scope);
        let closure = Closure::load_with_env(ctx, Some(text), &source, env).map_err(|error| {
            let detail = match error {
                CompilerError::Parsing(error) => error.to_string(),
                CompilerError::Compilation(error) => error.to_string(),
                error => error.to_string(),
            };
            message(ctx, format!("cannot import '{text}': {detail}"))
        })?;
        imports.loading.set_raw(&ctx, key, Value::Boolean(true))?;
        stack.clear();
        Ok(CallbackReturn::Call {
            function: closure.into(),
            then: Some(BoxSequence::new(
                &ctx,
                Imported {
                    imports: *imports,
                    key,
                    env,
                },
            )),
        })
    });
    scope.set_field(ctx, "import", import);
    let metatable = Table::new(&ctx);
    metatable.set_field(ctx, "__index", ctx.globals());
    scope.set_metatable(ctx, Some(metatable));
    freeze(ctx, scope.into());
    environment_over(ctx, scope)
}

#[derive(Collect, Clone, Copy)]
#[collect(no_drop)]
struct Imports<'gc> {
    scope: Table<'gc>,
    cache: Table<'gc>,
    loading: Table<'gc>,
}

#[derive(Collect)]
#[collect(no_drop)]
struct Imported<'gc> {
    imports: Imports<'gc>,
    key: Value<'gc>,
    env: Table<'gc>,
}

impl<'gc> Sequence<'gc> for Imported<'gc> {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        let value = stack.get(0);
        freeze(ctx, value);
        freeze(ctx, self.env.into());
        let entry = Table::new(&ctx);
        entry.set_raw(&ctx, Value::Integer(1), value)?;
        self.imports.loading.set_raw(&ctx, self.key, Value::Nil)?;
        self.imports.cache.set_raw(&ctx, self.key, entry.into())?;
        stack.replace(ctx, value);
        Ok(SequencePoll::Return)
    }

    fn error(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        error: Error<'gc>,
        _stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        self.imports.loading.set_raw(&ctx, self.key, Value::Nil)?;
        Err(error)
    }
}

fn message<'gc>(ctx: Context<'gc>, text: String) -> Error<'gc> {
    Value::String(ctx.intern(text.as_bytes())).into()
}

/// The canonical order of table keys: booleans, then numbers by value, then strings by bytes.
pub fn compare_keys<'gc>(a: Value<'gc>, b: Value<'gc>) -> Ordering {
    match (a, b) {
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(&b),
        (Value::Integer(a), Value::Integer(b)) => a.cmp(&b),
        (Value::Number(a), Value::Number(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        (Value::Integer(a), Value::Number(b)) => cmp_int_float(a, b).unwrap_or(Ordering::Equal),
        (Value::Number(a), Value::Integer(b)) => cmp_int_float(b, a)
            .map(Ordering::reverse)
            .unwrap_or(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.as_bytes().cmp(b.as_bytes()),
        (a, b) => rank(a).cmp(&rank(b)),
    }
}

fn rank(key: Value<'_>) -> Option<u8> {
    match key {
        Value::Boolean(_) => Some(0),
        Value::Integer(_) | Value::Number(_) => Some(1),
        Value::String(_) => Some(2),
        _ => None,
    }
}

fn unordered<'gc>(ctx: Context<'gc>, key: Value<'gc>) -> Error<'gc> {
    message(
        ctx,
        format!(
            "cannot iterate a table with {} keys in functional mode",
            key.type_name()
        ),
    )
}

fn sorted_keys<'gc>(ctx: Context<'gc>, table: Table<'gc>) -> Result<Vec<Value<'gc>>, Error<'gc>> {
    let mut keys = Vec::new();
    for (key, _) in table.iter(ctx) {
        if rank(key).is_none() {
            return Err(unordered(ctx, key));
        }
        keys.push(key);
    }
    keys.sort_by(|a, b| compare_keys(*a, *b));
    Ok(keys)
}

fn successor<'gc>(
    ctx: Context<'gc>,
    table: Table<'gc>,
    after: Value<'gc>,
) -> Result<(Value<'gc>, Value<'gc>), Error<'gc>> {
    if !after.is_nil() && rank(after).is_none() {
        return Err(unordered(ctx, after));
    }
    let mut best: Option<(Value<'gc>, Value<'gc>)> = None;
    for (key, value) in table.iter(ctx) {
        if rank(key).is_none() {
            return Err(unordered(ctx, key));
        }
        if !after.is_nil() && compare_keys(key, after) != Ordering::Greater {
            continue;
        }
        if best.map_or(true, |(found, _)| {
            compare_keys(key, found) == Ordering::Less
        }) {
            best = Some((key, value));
        }
    }
    Ok(best.unwrap_or((Value::Nil, Value::Nil)))
}

#[derive(Collect)]
#[collect(no_drop)]
struct Cursor<'gc> {
    table: Table<'gc>,
    keys: Vec<Value<'gc>>,
    position: usize,
}

fn global<'gc>(ctx: Context<'gc>, name: &'static str) -> Function<'gc> {
    match ctx.globals().get_value(ctx, name) {
        Value::Function(function) => function,
        _ => panic!("functional install requires the base library"),
    }
}

fn library<'gc>(ctx: Context<'gc>, name: &'static str) -> Option<Table<'gc>> {
    match ctx.globals().get_value(ctx, name) {
        Value::Table(table) => Some(table),
        _ => None,
    }
}

fn remove<'gc>(ctx: Context<'gc>, table: Table<'gc>, name: &'static str) {
    table
        .set_raw(&ctx, ctx.intern(name.as_bytes()).into(), Value::Nil)
        .unwrap();
}

/// Replace the impure and order-dependent parts of the loaded library, then freeze it.
pub(crate) fn install<'gc>(ctx: Context<'gc>) {
    let globals = ctx.globals();
    for name in ["collectgarbage", "dofile", "loadfile"] {
        remove(ctx, globals, name);
    }
    if let Some(math) = library(ctx, "math") {
        remove(ctx, math, "random");
        remove(ctx, math, "randomseed");
        crate::stdlib::load_portable_math(ctx, math);
    }
    if let Some(string) = library(ctx, "string") {
        remove(ctx, string, "dump");
    }
    if let Some(table) = library(ctx, "table") {
        remove(ctx, table, "interceptall");
    }

    ctx.set_global(
        "next",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (table, after): (Table, Value) = stack.consume(ctx)?;
            stack.replace(ctx, successor(ctx, table, after)?);
            Ok(CallbackReturn::Return)
        }),
    );

    let pairs = global(ctx, "pairs");
    ctx.set_global(
        "pairs",
        Callback::from_fn_with(&ctx, pairs, |pairs, ctx, _, mut stack| {
            let table = match stack.get(0) {
                Value::Table(table) => table,
                _ => {
                    return Ok(CallbackReturn::Call {
                        function: *pairs,
                        then: None,
                    })
                }
            };
            if table
                .metatable()
                .is_some_and(|mt| !mt.get_value(ctx, MetaMethod::Pairs).is_nil())
            {
                return Ok(CallbackReturn::Call {
                    function: *pairs,
                    then: None,
                });
            }
            let cursor = Gc::new(
                &ctx,
                RefLock::new(Cursor {
                    table,
                    keys: sorted_keys(ctx, table)?,
                    position: 0,
                }),
            );
            let iterate = Callback::from_fn_with(&ctx, cursor, |cursor, ctx, _, mut stack| {
                let mut cursor = cursor.borrow_mut(&ctx);
                while let Some(&key) = cursor.keys.get(cursor.position) {
                    cursor.position += 1;
                    let value = cursor.table.get_raw(&ctx, key);
                    if !value.is_nil() {
                        stack.replace(ctx, (key, value));
                        return Ok(CallbackReturn::Return);
                    }
                }
                stack.replace(ctx, Value::Nil);
                Ok(CallbackReturn::Return)
            });
            stack.replace(ctx, (iterate, table, Value::Nil));
            Ok(CallbackReturn::Return)
        }),
    );

    let set_metatable = global(ctx, "setmetatable");
    ctx.set_global(
        "setmetatable",
        Callback::from_fn_with(&ctx, set_metatable, |set_metatable, ctx, _, stack| {
            if let Value::Table(table) = stack.get(0) {
                if table.is_readonly() {
                    return Err("cannot change the metatable of a frozen table"
                        .into_value(ctx)
                        .into());
                }
            }
            if let Value::Table(metatable) = stack.get(1) {
                if !metatable.get_value(ctx, MetaMethod::Gc).is_nil()
                    || !metatable.get_value(ctx, MetaMethod::Mode).is_nil()
                {
                    return Err("__gc and __mode are not available in functional mode"
                        .into_value(ctx)
                        .into());
                }
            }
            Ok(CallbackReturn::Call {
                function: *set_metatable,
                then: None,
            })
        }),
    );

    let load = global(ctx, "load");
    ctx.set_global(
        "load",
        Callback::from_fn_with(&ctx, load, |load, ctx, _, mut stack| {
            if stack.len() < 3 {
                stack.resize(3);
            }
            stack.set(2, "t".into_value(ctx));
            Ok(CallbackReturn::Call {
                function: *load,
                then: None,
            })
        }),
    );

    freeze(ctx, globals.into());
    let strings = ctx.singleton::<Rootable![StringMetatable<'_>]>();
    freeze(ctx, strings.0.into());
}
