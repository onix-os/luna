use std::pin::Pin;

use ottavino_gc_arena::Collect;

use crate::{
    meta_ops, BoxSequence, Callback, CallbackReturn, Closure, Context, Error, Execution, IntoValue,
    Sequence, SequencePoll, Stack, String, Table, Value,
};

/// Stores a module's return value in `package.loaded` once its loader has run.
///
/// A loader is Lua, so it cannot be called from inside the `require` callback; it is returned as a
/// `CallbackReturn::Call` and this sequence picks up the result.
#[derive(Collect)]
#[collect(no_drop)]
struct FinishRequire<'gc> {
    name: String<'gc>,
    package: Table<'gc>,
}

impl<'gc> Sequence<'gc> for FinishRequire<'gc> {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        // "If the loader returns nil, require records true", so that a module with no value of its
        // own is still only loaded once.
        let value = match stack.get(0) {
            Value::Nil => Value::Boolean(true),
            v => v,
        };

        let loaded: Table = self.package.get(ctx, "loaded")?;
        loaded.set(ctx, self.name, value)?;

        stack.replace(ctx, value);
        Ok(SequencePoll::Return)
    }
}

/// Turns `foo.bar` into a path by substituting for each `?` in a `path` template.
fn candidate_paths(path_template: &str, module: &str) -> Vec<std::string::String> {
    let as_path = module.replace('.', "/");
    path_template
        .split(';')
        .filter(|t| !t.is_empty())
        .map(|t| t.replace('?', &as_path))
        .collect()
}

/// Loads the `package` library and the `require` global.
///
/// **There is no C loader.** `package.cpath`, `package.loadlib` and the C searcher do not exist and
/// will not: luna is pure Rust. What remains — `preload`, `loaded`, `path` and the Lua file
/// searcher — is the whole of `require` as a Lua program experiences it.
pub fn load_package<'gc>(ctx: Context<'gc>) {
    let package = Table::new(&ctx);

    let loaded = Table::new(&ctx);
    let preload = Table::new(&ctx);
    package.set_field(ctx, "loaded", loaded);
    package.set_field(ctx, "preload", preload);
    package.set_field(ctx, "path", ctx.intern(b"./?.lua;./?/init.lua"));

    ctx.set_global("package", package);

    ctx.set_global(
        "require",
        Callback::from_fn_with(&ctx, package, |package, ctx, _, mut stack| {
            let name: String = stack.consume(ctx)?;
            let package = *package;

            let loaded: Table = package.get(ctx, "loaded")?;

            // Already loaded: hand back the cached value without running anything.
            let cached = loaded.get_value(ctx, Value::String(name));
            if !cached.is_nil() {
                stack.replace(ctx, cached);
                return Ok(CallbackReturn::Return);
            }

            let finish = || {
                BoxSequence::new(
                    &ctx,
                    FinishRequire {
                        name,
                        package,
                    },
                )
            };

            // A host-registered loader wins over the filesystem, which is how an embedder ships
            // its own modules without putting them on disk.
            let preload: Table = package.get(ctx, "preload")?;
            let preloader = preload.get_value(ctx, Value::String(name));
            if !preloader.is_nil() {
                let function = meta_ops::call(ctx, preloader)?;
                stack.replace(ctx, name);
                return Ok(CallbackReturn::Call {
                    function,
                    then: Some(finish()),
                });
            }

            // Then the Lua file searcher.
            let path_template: String =
                package.get(ctx, "path")?;
            let module = name.display_lossy().to_string();
            let mut tried = Vec::new();

            for candidate in candidate_paths(&path_template.display_lossy().to_string(), &module) {
                match crate::io::buffered_read(match std::fs::File::open(&candidate) {
                    Ok(f) => f,
                    Err(_) => {
                        tried.push(candidate);
                        continue;
                    }
                }) {
                    Ok(mut reader) => {
                        let mut source = Vec::new();
                        std::io::Read::read_to_end(&mut reader, &mut source)
                            .map_err(|e| e.to_string().into_value(ctx))?;
                        let closure = Closure::load(ctx, Some(&candidate), &source)
                            .map_err(|e| e.to_string().into_value(ctx))?;
                        stack.replace(ctx, (name, ctx.intern(candidate.as_bytes())));
                        return Ok(CallbackReturn::Call {
                            function: closure.into(),
                            then: Some(finish()),
                        });
                    }
                    Err(_) => tried.push(candidate),
                }
            }

            Err(format!(
                "module '{module}' not found:\n\tno field package.preload['{module}']\n\tno file {}",
                tried.join("\n\tno file ")
            )
            .into_value(ctx)
            .into())
        }),
    );
}
