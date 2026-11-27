//! `import(name)`: evaluating other chunks once, freezing their results and caching them.

use std::pin::Pin;

use ottavino_gc_arena::Collect;

use crate::{
    BoxSequence, Callback, CallbackReturn, Closure, CompilerError, Context, Error, Execution,
    IntoValue, Sequence, SequencePoll, Stack, Table, Value,
};

use super::{environment_over, freeze, message, Data, DataError};

/// An [`environment`](super::environment) whose chunks can call `import(name)`.
///
/// `loader` maps a name to source text and is the only way the chunks reach other files, so it
/// decides which names exist and how they resolve. Each name is evaluated once, in its own
/// environment that can import further. Its first result and environment are frozen, and later
/// imports return that same value. Importing a name that is still being evaluated is an error.
/// An error raised inside an import gains a line naming the import and where it was called. A name
/// ending in `.json` or `.toml` is parsed as data instead of run.
pub fn import_environment<'gc>(
    ctx: Context<'gc>,
    loader: impl Fn(&str) -> Result<Vec<u8>, String> + 'static,
) -> Table<'gc> {
    import_environment_with(ctx, loader, Table::new(&ctx))
}

/// [`import_environment`] with extra globals shared by the root chunk and every import.
///
/// The fields of `globals` are copied into the shared scope and frozen with it.
pub fn import_environment_with<'gc>(
    ctx: Context<'gc>,
    loader: impl Fn(&str) -> Result<Vec<u8>, String> + 'static,
    globals: Table<'gc>,
) -> Table<'gc> {
    environment_over(ctx, import_scope(ctx, loader, globals))
}

/// The frozen scope every chunk of an import environment reads its globals through.
pub(crate) fn import_scope<'gc>(
    ctx: Context<'gc>,
    loader: impl Fn(&str) -> Result<Vec<u8>, String> + 'static,
    globals: Table<'gc>,
) -> Table<'gc> {
    let scope = Table::new(&ctx);
    for (key, value) in globals.iter(ctx) {
        scope.set_raw(&ctx, key, value).unwrap();
    }
    let imports = Imports {
        scope,
        cache: Table::new(&ctx),
        loading: Table::new(&ctx),
        chain: Table::new(&ctx),
    };
    let import = Callback::from_fn_with(&ctx, imports, move |imports, ctx, exec, mut stack| {
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
        let depth = imports.chain.length(&ctx);
        let site = match exec.upper_lua_frame() {
            Some(frame) => format!(
                "{}:{}",
                frame.chunk_name.display_lossy(),
                frame.current_line
            ),
            None => match imports.chain.get_raw(&ctx, Value::Integer(depth)) {
                Value::String(importer) => format!("{} (in a tail call)", importer.display_lossy()),
                _ => "the root chunk (in a tail call)".to_owned(),
            },
        };
        if !imports.loading.get_raw(&ctx, key).is_nil() {
            return Err(message(
                ctx,
                format!("import cycle through '{text}', imported from {site}"),
            ));
        }
        let failed =
            |detail: String| message(ctx, format!("cannot import '{text}' from {site}: {detail}"));
        let source = loader(text).map_err(failed)?;
        if let Some(parse) = data_format(text) {
            let value = std::str::from_utf8(&source)
                .map_err(|_| "the file is not UTF-8".to_owned())
                .and_then(|source| parse(source).map_err(|error| error.to_string()))
                .and_then(|data| data.to_value(ctx).map_err(|error| error.to_string()))
                .map_err(failed)?;
            freeze(ctx, value);
            let entry = Table::new(&ctx);
            entry.set_raw(&ctx, Value::Integer(1), value)?;
            imports.cache.set_raw(&ctx, key, entry.into())?;
            stack.replace(ctx, value);
            return Ok(CallbackReturn::Return);
        }
        let env = environment_over(ctx, imports.scope);
        let closure = Closure::load_with_env(ctx, Some(text), &source, env).map_err(|error| {
            failed(match error {
                CompilerError::Parsing(error) => error.to_string(),
                CompilerError::Compilation(error) => error.to_string(),
                error => error.to_string(),
            })
        })?;
        imports.loading.set_raw(&ctx, key, Value::Boolean(true))?;
        imports
            .chain
            .set_raw(&ctx, Value::Integer(depth + 1), key)?;
        stack.clear();
        Ok(CallbackReturn::Call {
            function: closure.into(),
            then: Some(BoxSequence::new(
                &ctx,
                Imported {
                    imports: *imports,
                    key,
                    env,
                    trace: ctx
                        .intern(format!("\n  in '{text}', imported from {site}").as_bytes())
                        .into(),
                },
            )),
        })
    });
    scope.set_field(ctx, "import", import);
    let metatable = Table::new(&ctx);
    metatable.set_field(ctx, "__index", ctx.globals());
    scope.set_metatable(ctx, Some(metatable));
    freeze(ctx, scope.into());
    scope
}

type Parse = fn(&str) -> Result<Data, DataError>;

/// Names ending in `.json` or `.toml` are data files, imported as frozen data.
fn data_format(name: &str) -> Option<Parse> {
    if name.ends_with(".json") {
        Some(Data::from_json)
    } else if name.ends_with(".toml") {
        Some(Data::from_toml)
    } else {
        None
    }
}

#[derive(Collect, Clone, Copy)]
#[collect(no_drop)]
struct Imports<'gc> {
    scope: Table<'gc>,
    cache: Table<'gc>,
    loading: Table<'gc>,
    chain: Table<'gc>,
}

impl<'gc> Imports<'gc> {
    /// Leave the import at the top of the chain.
    fn finish(&self, ctx: Context<'gc>, key: Value<'gc>) -> Result<(), Error<'gc>> {
        self.loading.set_raw(&ctx, key, Value::Nil)?;
        let depth = self.chain.length(&ctx);
        self.chain
            .set_raw(&ctx, Value::Integer(depth), Value::Nil)?;
        Ok(())
    }
}

#[derive(Collect)]
#[collect(no_drop)]
struct Imported<'gc> {
    imports: Imports<'gc>,
    key: Value<'gc>,
    env: Table<'gc>,
    trace: Value<'gc>,
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
        self.imports.finish(ctx, self.key)?;
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
        self.imports.finish(ctx, self.key)?;
        let Value::String(trace) = self.trace else {
            return Err(error);
        };
        let text = match &error {
            Error::Lua(lua) => match lua.0 {
                Value::String(text) => text.as_bytes().to_vec(),
                _ => return Err(error),
            },
            Error::Runtime(runtime) => format!("{runtime:#}").into_bytes(),
        };
        let mut traced = text;
        traced.extend_from_slice(trace.as_bytes());
        Err(Value::String(ctx.intern(&traced)).into())
    }
}
