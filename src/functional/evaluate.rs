//! One call from a configuration file to frozen data: imports confined to a root directory,
//! inputs, a recorded API, instruction and memory limits, schema checking, content hashes and a
//! cache that skips evaluation when nothing it read has changed.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt, fs,
    path::{Component, Path, PathBuf},
    rc::Rc,
};

use crate::{Closure, Context, Error, Executor, Fuel, Lua, StashedExecutor, Table, Value};

use super::{
    environment_over, freeze,
    import::import_scope,
    sha256::{hex, Sha256},
    Api, Data, DataError, Recorder,
};

const SLICE: i32 = 4096;
const FORMAT: &str = "luna-functional-1";

/// What a configuration is evaluated with.
#[derive(Debug, Clone)]
pub struct Options {
    /// The `inputs` global, an empty map by default. A root chunk that returns a function is called
    /// with it.
    pub inputs: Data,
    /// A registration-style API exposed under a global name, whose calls are recorded.
    pub api: Option<(String, Api)>,
    /// A file returning a `lib.types` schema the result is checked and normalised against.
    pub schema: Option<PathBuf>,
    /// A directory of cached results, keyed by content hash.
    pub cache: Option<PathBuf>,
    /// The most VM instructions the whole evaluation may run.
    pub fuel: u64,
    /// The most bytes the evaluation may allocate beyond a fresh functional state.
    pub memory: Option<usize>,
    /// The directory imports resolve in. Defaults to the configuration file's directory.
    pub root: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            inputs: Data::Map(Vec::new()),
            api: None,
            schema: None,
            cache: None,
            fuel: 1 << 32,
            memory: Some(1 << 30),
            root: None,
        }
    }
}

/// The frozen result of an evaluation and its identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluated {
    /// The value the configuration returned, normalised by the schema if there is one.
    pub value: Data,
    /// The recorded API calls as a list of `{ name, args }` maps.
    pub declarations: Data,
    /// Every file read, by name, with the SHA-256 of its contents.
    pub files: BTreeMap<String, String>,
    /// SHA-256 over the format version, every file read, the inputs, the API and the schema.
    pub input_hash: String,
    /// SHA-256 over the canonical encoding of the value and the declarations.
    pub output_hash: String,
    /// Whether this came from the cache.
    pub cached: bool,
}

/// Why an evaluation failed.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    /// A file could not be read.
    Io { path: PathBuf, message: String },
    /// The configuration or schema did not compile.
    Compile(String),
    /// Lua code raised an error.
    Lua(String),
    /// A value could not be converted to or from data.
    Data(DataError),
    /// The instruction budget ran out.
    Fuel(u64),
    /// The memory ceiling was exceeded.
    Memory(usize),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::Io { path, message } => write!(f, "{}: {message}", path.display()),
            EvalError::Compile(message) | EvalError::Lua(message) => write!(f, "{message}"),
            EvalError::Data(error) => write!(f, "{error}"),
            EvalError::Fuel(fuel) => {
                write!(f, "evaluation ran more than {fuel} instructions")
            }
            EvalError::Memory(bytes) => {
                write!(f, "evaluation allocated more than {bytes} bytes")
            }
        }
    }
}

impl std::error::Error for EvalError {}

impl From<DataError> for EvalError {
    fn from(error: DataError) -> Self {
        EvalError::Data(error)
    }
}

/// The SHA-256 of `bytes` in lowercase hexadecimal.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finish())
}

fn read(path: &Path) -> Result<Vec<u8>, EvalError> {
    fs::read(path).map_err(|error| EvalError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })
}

/// Resolve an import name inside `root`, refusing absolute paths and anything that leaves it.
fn resolve(root: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    if name.is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("import names are relative paths inside the root, without '.' or '..'".into());
    }
    let path = root.join(relative);
    let canonical = path.canonicalize().map_err(|error| error.to_string())?;
    if !canonical.starts_with(root) {
        return Err("the import resolves outside the root".into());
    }
    Ok(canonical)
}

fn lua_message<'gc>(error: &Error<'gc>) -> String {
    match error {
        Error::Lua(lua) => match lua.0 {
            Value::String(text) => text.display_lossy().to_string(),
            other => format!("a {} was raised as an error", other.type_name()),
        },
        Error::Runtime(runtime) => format!("{runtime:#}"),
    }
}

/// Run `executor` to completion within the remaining fuel and the memory ceiling.
fn run(
    lua: &mut Lua,
    executor: &StashedExecutor,
    fuel: &mut u64,
    budget: u64,
    limit: Option<usize>,
) -> Result<(), EvalError> {
    loop {
        if let Some(limit) = limit {
            if lua.accounted_memory() > limit {
                lua.gc_collect();
                lua.gc_collect();
                if lua.accounted_memory() > limit {
                    lua.enter(|ctx| ctx.fetch(executor).stop(&ctx));
                    return Err(EvalError::Memory(limit));
                }
            }
        }
        let mut slice = Fuel::with(SLICE);
        let finished = lua
            .enter(|ctx| ctx.fetch(executor).step(ctx, &mut slice))
            .map_err(|error| EvalError::Lua(error.to_string()))?;
        let used = (SLICE - slice.remaining()).max(0) as u64;
        if used > *fuel {
            lua.enter(|ctx| ctx.fetch(executor).stop(&ctx));
            return Err(EvalError::Fuel(budget));
        }
        *fuel -= used;
        if finished {
            return Ok(());
        }
    }
}

fn result<'gc>(ctx: Context<'gc>, executor: &StashedExecutor) -> Result<Value<'gc>, EvalError> {
    match ctx.fetch(executor).take_result::<Value>(ctx) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(EvalError::Lua(lua_message(&error))),
        Err(error) => Err(EvalError::Lua(error.to_string())),
    }
}

/// Evaluate the configuration at `path`.
///
/// The file runs in a [`Lua::functional`] state whose chunks see `inputs`, the API if any, and
/// `import(name)`, which reads `name` relative to the root and refuses to leave it. If the file
/// returns a function, it is called with the inputs, as a Nix file returning a function is. The
/// result is frozen, checked against the schema, and returned as data together with the
/// recorded declarations and the content hashes. With a cache directory, a previous result is
/// returned without evaluating when the file, the inputs, the API, the schema and every file it
/// imported are unchanged.
pub fn evaluate(path: impl AsRef<Path>, options: &Options) -> Result<Evaluated, EvalError> {
    let path = path.as_ref();
    let file = path.canonicalize().map_err(|error| EvalError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let root = match &options.root {
        Some(root) => root.canonicalize().map_err(|error| EvalError::Io {
            path: root.clone(),
            message: error.to_string(),
        })?,
        None => file.parent().unwrap_or(Path::new("/")).to_owned(),
    };
    let name = file
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .or_else(|| file.file_name().and_then(|name| name.to_str()))
        .unwrap_or("config")
        .to_owned();
    let source = read(&file)?;
    let schema = match &options.schema {
        Some(schema) => Some((schema.display().to_string(), read(schema)?)),
        None => None,
    };
    let api = options
        .api
        .as_ref()
        .map(|(name, api)| Data::List(vec![Data::from(name.as_str()), api.describe()]))
        .unwrap_or(Data::Nil);
    let key = Data::List(vec![
        Data::from(FORMAT),
        Data::from(env!("CARGO_PKG_VERSION")),
        Data::from(name.as_str()),
        Data::from(content_hash(&source)),
        options.inputs.clone(),
        api.clone(),
        schema
            .as_ref()
            .map(|(_, text)| Data::from(content_hash(text)))
            .unwrap_or(Data::Nil),
    ]);
    let key = content_hash(&key.canonical());

    if let Some(cache) = &options.cache {
        if let Some(hit) = cached(cache, &key, &root) {
            return Ok(hit);
        }
    }

    let imported = Rc::new(RefCell::new(BTreeMap::new()));
    let mut evaluated = run_config(
        &root,
        &name,
        &source,
        schema.as_ref(),
        options,
        imported.clone(),
    )?;
    let mut files = imported.borrow().clone();
    let imports = files.clone();
    files.insert(name.clone(), content_hash(&source));
    if let Some((schema_name, text)) = &schema {
        files.insert(format!("schema:{schema_name}"), content_hash(text));
    }
    let identity = Data::List(vec![
        Data::from(FORMAT),
        Data::from(env!("CARGO_PKG_VERSION")),
        Data::from(name.as_str()),
        hashes(&files)?,
        options.inputs.clone(),
        api,
    ]);
    evaluated.input_hash = content_hash(&identity.canonical());
    evaluated.files = files;

    if let Some(cache) = &options.cache {
        store(cache, &key, &imports, &evaluated);
    }
    Ok(evaluated)
}

fn hashes(files: &BTreeMap<String, String>) -> Result<Data, DataError> {
    Data::map(
        files
            .iter()
            .map(|(name, hash)| (Data::from(name.as_str()), Data::from(hash.as_str())))
            .collect(),
    )
}

fn run_config(
    root: &Path,
    name: &str,
    source: &[u8],
    schema: Option<&(String, Vec<u8>)>,
    options: &Options,
    imported: Rc<RefCell<BTreeMap<String, String>>>,
) -> Result<Evaluated, EvalError> {
    let mut lua = Lua::functional();
    let limit = options
        .memory
        .map(|memory| lua.accounted_memory().saturating_add(memory));
    let mut fuel = options.fuel;
    let loader_root = root.to_owned();
    let loader = move |import: &str| {
        let path = resolve(&loader_root, import)?;
        let text = fs::read(&path).map_err(|error| error.to_string())?;
        imported
            .borrow_mut()
            .insert(import.to_owned(), content_hash(&text));
        Ok(text)
    };

    let (executor, recorder, scope, inputs) = lua
        .try_enter(|ctx| {
            let globals = Table::new(&ctx);
            let inputs = options.inputs.to_value(ctx).map_err(data_error)?;
            freeze(ctx, inputs);
            globals.set(ctx, "inputs", inputs)?;
            let recorder = match &options.api {
                Some((global, api)) => {
                    let (table, recorder) = api.install(ctx).map_err(data_error)?;
                    globals.set(ctx, global.as_str(), table)?;
                    Some(recorder)
                }
                None => None,
            };
            let scope = import_scope(ctx, loader, globals);
            let env = environment_over(ctx, scope);
            let closure = Closure::load_with_env(ctx, Some(name), source, env)
                .map_err(|error| compile_error(&error))?;
            Ok((
                ctx.stash(Executor::start(ctx, closure.into(), ())),
                recorder.map(|recorder| ctx.stash(recorder.table())),
                ctx.stash(scope),
                ctx.stash(inputs),
            ))
        })
        .map_err(|error| extern_message(&error))?;

    run(&mut lua, &executor, &mut fuel, options.fuel, limit)?;
    let returned = lua
        .try_enter(|ctx| {
            Ok(match result(ctx, &executor).map_err(eval_to_lua)? {
                Value::Function(function) => {
                    let inputs = ctx.fetch(&inputs);
                    Err(ctx.stash(Executor::start(ctx, function, inputs)))
                }
                value => Ok(ctx.stash(value)),
            })
        })
        .map_err(|error| extern_message(&error))?;
    let mut value = match returned {
        Ok(value) => value,
        Err(call) => {
            run(&mut lua, &call, &mut fuel, options.fuel, limit)?;
            finished(&mut lua, &call)?
        }
    };

    if let Some((schema_name, schema_source)) = schema {
        let load = lua
            .try_enter(|ctx| {
                let env = environment_over(ctx, ctx.fetch(&scope));
                let chunk = format!("schema:{schema_name}");
                let closure = Closure::load_with_env(ctx, Some(&chunk), schema_source, env)
                    .map_err(|error| compile_error(&error))?;
                Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
            })
            .map_err(|error| extern_message(&error))?;
        run(&mut lua, &load, &mut fuel, options.fuel, limit)?;
        let check = lua
            .try_enter(|ctx| {
                let schema = result(ctx, &load).map_err(eval_to_lua)?;
                let lib: Table = ctx.globals().get(ctx, "lib")?;
                let check: crate::Function = lib.get(ctx, "check")?;
                let args = (ctx.fetch(&value), schema, "config");
                Ok(ctx.stash(Executor::start(ctx, check, args)))
            })
            .map_err(|error| extern_message(&error))?;
        run(&mut lua, &check, &mut fuel, options.fuel, limit)?;
        value = finished(&mut lua, &check)?;
    }

    lua.try_enter(|ctx| {
        let value = ctx.fetch(&value);
        freeze(ctx, value);
        let data = Data::from_value(ctx, value).map_err(|error| {
            data_error(DataError {
                path: format!("value{}", error.path),
                message: error.message,
            })
        })?;
        let declarations = match &recorder {
            Some(log) => Recorder::from_table(ctx.fetch(log))
                .to_data(ctx)
                .map_err(data_error)?,
            None => Data::List(Vec::new()),
        };
        let output = Data::map(vec![
            (Data::from("declarations"), declarations.clone()),
            (Data::from("value"), data.clone()),
        ])
        .map_err(data_error)?;
        Ok(Evaluated {
            value: data,
            declarations,
            files: BTreeMap::new(),
            input_hash: String::new(),
            output_hash: content_hash(&output.canonical()),
            cached: false,
        })
    })
    .map_err(|error| extern_message(&error))
}

fn finished(lua: &mut Lua, executor: &StashedExecutor) -> Result<crate::StashedValue, EvalError> {
    lua.try_enter(|ctx| {
        let value = result(ctx, executor).map_err(eval_to_lua)?;
        Ok(ctx.stash(value))
    })
    .map_err(|error| extern_message(&error))
}

fn data_error<'gc>(error: DataError) -> Error<'gc> {
    Error::Runtime(crate::RuntimeError::new(EvalError::Data(error)))
}

fn eval_to_lua<'gc>(error: EvalError) -> Error<'gc> {
    Error::Runtime(crate::RuntimeError::new(error))
}

fn compile_error<'gc>(error: &crate::CompilerError) -> Error<'gc> {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    Error::Runtime(crate::RuntimeError::new(EvalError::Compile(message)))
}

fn extern_message(error: &crate::ExternError) -> EvalError {
    if let crate::ExternError::Runtime(runtime) = error {
        if let Some(eval) = runtime.downcast::<EvalError>() {
            return eval.clone();
        }
    }
    let text = error.to_string();
    EvalError::Lua(text.strip_prefix("lua error: ").unwrap_or(&text).to_owned())
}

fn entry_path(cache: &Path, key: &str) -> PathBuf {
    cache.join(format!("{key}.luna"))
}

fn cached(cache: &Path, key: &str, root: &Path) -> Option<Evaluated> {
    let bytes = fs::read(entry_path(cache, key)).ok()?;
    let entry = Data::from_canonical(&bytes).ok()?;
    let Some(Data::Map(imports)) = entry.get("imports") else {
        return None;
    };
    for (name, hash) in imports {
        let path = resolve(root, name.as_str()?).ok()?;
        if content_hash(&fs::read(path).ok()?) != hash.as_str()? {
            return None;
        }
    }
    let Some(Data::Map(files)) = entry.get("files") else {
        return None;
    };
    Some(Evaluated {
        value: entry.get("value")?.clone(),
        declarations: entry.get("declarations")?.clone(),
        files: files
            .iter()
            .map(|(name, hash)| Some((name.as_str()?.to_owned(), hash.as_str()?.to_owned())))
            .collect::<Option<_>>()?,
        input_hash: entry.get("input_hash")?.as_str()?.to_owned(),
        output_hash: entry.get("output_hash")?.as_str()?.to_owned(),
        cached: true,
    })
}

fn store(cache: &Path, key: &str, imports: &BTreeMap<String, String>, evaluated: &Evaluated) {
    let entry = (|| {
        Data::map(vec![
            (Data::from("declarations"), evaluated.declarations.clone()),
            (Data::from("files"), hashes(&evaluated.files)?),
            (Data::from("imports"), hashes(imports)?),
            (
                Data::from("input_hash"),
                Data::from(evaluated.input_hash.as_str()),
            ),
            (
                Data::from("output_hash"),
                Data::from(evaluated.output_hash.as_str()),
            ),
            (Data::from("value"), evaluated.value.clone()),
        ])
    })();
    let Ok(entry) = entry else {
        return;
    };
    if fs::create_dir_all(cache).is_err() {
        return;
    }
    let path = entry_path(cache, key);
    let partial = path.with_extension(format!("{}.partial", std::process::id()));
    if fs::write(&partial, entry.canonical()).is_ok() && fs::rename(&partial, &path).is_err() {
        let _ = fs::remove_file(&partial);
    }
}
