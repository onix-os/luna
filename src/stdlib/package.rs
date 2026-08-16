use std::pin::Pin;

use ottavino_gc_arena::Collect;

use crate::{
    meta_ops, BoxSequence, Callback, CallbackReturn, Closure, Context, Error, Execution, IntoValue,
    Sequence, SequencePoll, Stack, String, Table, Value,
};

/// Walks `package.searchers`, calling each until one hands back a loader.
///
/// The searcher list is a real hook, as in PUC-Rio: a host or a script can insert its own entry and
/// `require` will consult it. That is why this is a sequence — every searcher, and then the loader
/// it returns, is Lua code that has to run through the executor.
#[derive(Collect)]
#[collect(no_drop)]
struct SearchModule<'gc> {
    name: String<'gc>,
    package: Table<'gc>,
    searchers: Table<'gc>,
    /// Which searcher was called last, 1-based; 0 before the first.
    index: i64,
    /// What each searcher said when it declined, for the "not found" message.
    reasons: std::vec::Vec<u8>,
    /// False while searchers are running, true once a loader has been called.
    loading: bool,
}

impl<'gc> SearchModule<'gc> {
    /// Calls searcher `index + 1`, or reports that nothing could load the module.
    fn advance(
        &mut self,
        ctx: Context<'gc>,
        stack: &mut Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        self.index += 1;
        let searcher = self.searchers.get_value(ctx, Value::Integer(self.index));
        if searcher.is_nil() {
            let module = self.name.display_lossy().to_string();
            let reasons = std::string::String::from_utf8_lossy(&self.reasons).into_owned();
            return Err(format!("module '{module}' not found:{reasons}")
                .into_value(ctx)
                .into());
        }

        let function = meta_ops::call(ctx, searcher)?;
        stack.replace(ctx, self.name);
        Ok(SequencePoll::Call {
            function,
            bottom: 0,
        })
    }
}

impl<'gc> Sequence<'gc> for SearchModule<'gc> {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        let this = self.get_mut();

        if this.loading {
            // "If the loader returns nil, require records true", so that a module with no value of
            // its own is still only loaded once.
            let value = match stack.get(0) {
                Value::Nil => Value::Boolean(true),
                v => v,
            };
            let loaded: Table = this.package.get(ctx, "loaded")?;
            loaded.set(ctx, this.name, value)?;
            stack.replace(ctx, value);
            return Ok(SequencePoll::Return);
        }

        // A searcher returns a loader plus an optional extra value, a string saying why it
        // declined, or nothing at all.
        match stack.get(0) {
            Value::Function(_) | Value::Table(_) | Value::UserData(_) => {
                let function = meta_ops::call(ctx, stack.get(0))?;
                let extra = stack.get(1);
                stack.replace(ctx, (this.name, extra));
                this.loading = true;
                Ok(SequencePoll::Call {
                    function,
                    bottom: 0,
                })
            }
            Value::String(reason) => {
                this.reasons.push(b'\n');
                this.reasons.push(b'\t');
                this.reasons.extend_from_slice(reason.as_bytes());
                stack.clear();
                this.advance(ctx, &mut stack)
            }
            _ => {
                stack.clear();
                this.advance(ctx, &mut stack)
            }
        }
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

    // The five lines PUC-Rio documents: directory separator, path separator, template mark, the
    // executable-directory mark and the substitution point in a C loader name. The last two exist
    // only so that code parsing this string finds what it expects; luna has no C loader.
    package.set_field(
        ctx,
        "config",
        ctx.intern(
            if cfg!(windows) {
                "\\\n;\n?\n!\n-\n"
            } else {
                "/\n;\n?\n!\n-\n"
            }
            .as_bytes(),
        ),
    );

    // `require` walks this list, so replacing or inserting an entry changes what it consults. A
    // searcher takes the module name and returns either a loader (plus an optional second value
    // handed to it) or a string saying why it declined. PUC-Rio's third and fourth searchers are
    // the C loader and the all-in-one loader, neither of which exists here.
    let searchers = Table::new(&ctx);
    searchers
        .set(
            ctx,
            1,
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let name: String = stack.consume(ctx)?;
                let package: Table = ctx.get_global("package")?;
                let preload: Table = package.get(ctx, "preload")?;
                match preload.get_value(ctx, name) {
                    Value::Nil => {
                        let module = name.display_lossy().to_string();
                        stack.replace(
                            ctx,
                            ctx.intern(format!("no field package.preload['{module}']").as_bytes()),
                        );
                    }
                    loader => stack.replace(ctx, (loader, ":preload:")),
                }
                Ok(CallbackReturn::Return)
            }),
        )
        .unwrap();
    searchers
        .set(
            ctx,
            2,
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let name: String = stack.consume(ctx)?;
                let package: Table = ctx.get_global("package")?;
                let template: String = package.get(ctx, "path")?;
                let mut tried = Vec::new();
                for candidate in candidate_paths(
                    &template.display_lossy().to_string(),
                    &name.display_lossy().to_string(),
                ) {
                    let Ok(file) = std::fs::File::open(&candidate) else {
                        tried.push(format!("no file {candidate}"));
                        continue;
                    };
                    let mut source = Vec::new();
                    match crate::io::buffered_read(file)
                        .and_then(|mut r| std::io::Read::read_to_end(&mut r, &mut source))
                    {
                        Ok(_) => {
                            let closure = Closure::load(ctx, Some(&candidate), &source)
                                .map_err(|e| e.to_string().into_value(ctx))?;
                            // The second value is the "extra" argument `require` passes the
                            // loader, which for a file searcher is where it was found.
                            stack.replace(ctx, (closure, ctx.intern(candidate.as_bytes())));
                            return Ok(CallbackReturn::Return);
                        }
                        Err(_) => tried.push(format!("no file {candidate}")),
                    }
                }
                stack.replace(ctx, ctx.intern(tried.join("\n\t").as_bytes()));
                Ok(CallbackReturn::Return)
            }),
        )
        .unwrap();
    package.set_field(ctx, "searchers", searchers);

    package.set_field(
        ctx,
        "searchpath",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (name, path, sep, rep): (String, String, Option<String>, Option<String>) =
                stack.consume(ctx)?;
            // `sep` is replaced by `rep` before substitution — the hook that lets `a.b` become
            // `a/b`, and the reason the default `rep` is the directory separator.
            let sep = sep
                .map(|s| s.display_lossy().to_string())
                .unwrap_or_else(|| ".".to_owned());
            let rep = rep
                .map(|s| s.display_lossy().to_string())
                .unwrap_or_else(|| if cfg!(windows) { "\\" } else { "/" }.to_owned());
            let name = name.display_lossy().to_string();
            let name = if sep.is_empty() {
                name
            } else {
                name.replace(&sep, &rep)
            };

            let mut tried = std::vec::Vec::new();
            for candidate in path.display_lossy().to_string().split(';') {
                if candidate.is_empty() {
                    continue;
                }
                let candidate = candidate.replace('?', &name);
                if std::fs::metadata(&candidate).is_ok() {
                    stack.replace(ctx, ctx.intern(candidate.as_bytes()));
                    return Ok(CallbackReturn::Return);
                }
                tried.push(format!("\n\tno file '{candidate}'"));
            }
            stack.replace(ctx, (Value::Nil, ctx.intern(tried.concat().as_bytes())));
            Ok(CallbackReturn::Return)
        }),
    );

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

            // Every searcher in `package.searchers`, in order, until one produces a loader.
            let searchers: Table = package.get(ctx, "searchers")?;
            let mut search = SearchModule {
                name,
                package,
                searchers,
                index: 0,
                reasons: std::vec::Vec::new(),
                loading: false,
            };
            let first = search.advance(ctx, &mut stack)?;
            let SequencePoll::Call { function, .. } = first else {
                unreachable!("`advance` either calls a searcher or raises")
            };
            Ok(CallbackReturn::Call {
                function,
                then: Some(BoxSequence::new(&ctx, search)),
            })
        }),
    );
}
