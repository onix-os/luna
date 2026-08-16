use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};

use ottavino_gc_arena::{lock::Lock, Collect, Gc, Rootable};

use crate::{
    Callback, CallbackReturn, Context, IntoValue, Singleton, String, Table, UserData, Value,
    Variadic,
};

/// What a file handle wraps.
///
/// The standard streams are kept separate from real files so that closing `io.stdout` is a no-op
/// rather than something that takes stdout away from the process.
enum Handle {
    File(BufReader<std::fs::File>),
    /// A child process; the flag says whether luna writes to its stdin rather than reads stdout.
    Process(Box<std::process::Child>, bool),
    Stdout,
    Stderr,
    Stdin,
    Closed,
}

impl Handle {
    fn is_closed(&self) -> bool {
        matches!(self, Handle::Closed)
    }
}

struct FileHandle(RefCell<Handle>);

/// The streams `io.read`, `io.write` and `io.lines` use when given no file of their own.
///
/// A singleton rather than fields on the `io` table, so that `io.input(f)` redirects the existing
/// `io.read` instead of only affecting callers who go looking for the new handle.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct DefaultStreams<'gc> {
    input: Gc<'gc, Lock<UserData<'gc>>>,
    output: Gc<'gc, Lock<UserData<'gc>>>,
}

impl<'gc> DefaultStreams<'gc> {
    // Built in `load_io` and captured by the callbacks that need it, rather than made a `Singleton`:
    // creating one needs a file handle, which needs the metatable singleton, and the registry is
    // already borrowed while a singleton is being created.
    fn new(ctx: Context<'gc>, input: UserData<'gc>, output: UserData<'gc>) -> Self {
        DefaultStreams {
            input: Gc::new(&ctx, Lock::new(input)),
            output: Gc::new(&ctx, Lock::new(output)),
        }
    }
}

/// Resolve the argument of `io.input`/`io.output`: a handle passes through, a name is opened.
fn stream_argument<'gc>(
    ctx: Context<'gc>,
    value: Value<'gc>,
    write: bool,
) -> Result<UserData<'gc>, crate::Error<'gc>> {
    match value {
        Value::UserData(ud) => {
            handle_of(ctx, value)?;
            Ok(ud)
        }
        Value::String(name) => {
            let path = std::path::PathBuf::from(name.display_lossy().to_string());
            let opened = if write {
                std::fs::File::create(&path)
            } else {
                std::fs::File::open(&path)
            };
            match opened {
                Ok(f) => Ok(new_handle(ctx, Handle::File(BufReader::new(f)))),
                Err(err) => Err(format!("cannot open '{}': {err}", path.display())
                    .into_value(ctx)
                    .into()),
            }
        }
        other => Err(format!(
            "bad argument (expected file or name, got {})",
            other.type_name()
        )
        .into_value(ctx)
        .into()),
    }
}

/// The shared metatable for every file handle.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct FileMetatable<'gc>(pub Table<'gc>);

impl<'gc> Singleton<'gc> for FileMetatable<'gc> {
    fn create(ctx: Context<'gc>) -> Self {
        FileMetatable(Table::new(&ctx))
    }
}

fn handle_of<'gc>(
    ctx: Context<'gc>,
    value: Value<'gc>,
) -> Result<&'gc FileHandle, crate::Error<'gc>> {
    match value {
        Value::UserData(ud) => ud
            .downcast_static::<FileHandle>()
            .map_err(|_| "not a file handle".into_value(ctx).into()),
        _ => Err("not a file handle".into_value(ctx).into()),
    }
}

/// Flush whatever the handle is, where that means anything.
fn flush_handle<'gc>(ctx: Context<'gc>, handle: &mut Handle) -> Result<(), crate::Error<'gc>> {
    let result = match handle {
        Handle::File(f) => f.get_mut().flush(),
        Handle::Stdout => std::io::stdout().flush(),
        Handle::Stderr => std::io::stderr().flush(),
        Handle::Process(child, true) => match child.stdin.as_mut() {
            Some(stdin) => stdin.flush(),
            None => Ok(()),
        },
        Handle::Closed => return Err("attempt to use a closed file".into_value(ctx).into()),
        _ => Ok(()),
    };
    result.map_err(|e| e.to_string().into_value(ctx).into())
}

fn new_handle<'gc>(ctx: Context<'gc>, handle: Handle) -> UserData<'gc> {
    let ud = UserData::new_static(&ctx, FileHandle(RefCell::new(handle)));
    let FileMetatable(mt) = *ctx.singleton::<Rootable![FileMetatable<'_>]>();
    ud.set_metatable(ctx, Some(mt));
    ud
}

/// One `:read` format, applied to whatever the handle currently is.
fn read_one<'gc>(
    ctx: Context<'gc>,
    handle: &mut Handle,
    format: Value<'gc>,
) -> Result<Value<'gc>, crate::Error<'gc>> {
    // A count reads exactly that many bytes; the letters follow PUC-Rio, with or without the `*`.
    let spec = match format {
        Value::Integer(_) | Value::Number(_) => None,
        Value::String(s) => Some(s.display_lossy().to_string()),
        Value::Nil => Some("l".to_owned()),
        _ => return Err("bad argument to 'read'".into_value(ctx).into()),
    };

    fn read_line(r: &mut dyn BufRead, keep_newline: bool) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        let n = r.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        if !keep_newline && buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        Ok(Some(buf))
    }

    let reader: &mut dyn BufRead = match handle {
        Handle::File(f) => f,
        Handle::Process(child, false) => {
            // Read the child's output whole: a `BufReader` cannot be stored back into the handle
            // without restructuring it, and popen output is small in practice.
            let mut buf = Vec::new();
            if let Some(out) = child.stdout.as_mut() {
                out.read_to_end(&mut buf).ok();
            }
            let text = ctx.intern(&buf);
            *handle = Handle::Closed;
            return Ok(text.into());
        }
        Handle::Process(_, true) => {
            return Err("file is not opened for reading".into_value(ctx).into())
        }
        Handle::Stdin => return read_from_stdin(ctx, spec, format),
        Handle::Closed => return Err("attempt to use a closed file".into_value(ctx).into()),
        _ => return Err("file is not opened for reading".into_value(ctx).into()),
    };

    match spec.as_deref().map(|s| s.trim_start_matches('*')) {
        None => {
            let count = format.to_integer().unwrap_or(0).max(0) as usize;
            let mut buf = vec![0u8; count];
            let mut filled = 0;
            while filled < count {
                match reader.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => return Err(e.to_string().into_value(ctx).into()),
                }
            }
            if filled == 0 && count > 0 {
                Ok(Value::Nil)
            } else {
                buf.truncate(filled);
                Ok(ctx.intern(&buf).into())
            }
        }
        Some("a") => {
            let mut buf = Vec::new();
            reader
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string().into_value(ctx))?;
            Ok(ctx.intern(&buf).into())
        }
        Some("l") | Some("L") => {
            let keep = spec.as_deref().map(|s| s.ends_with('L')).unwrap_or(false);
            match read_line(reader, keep).map_err(|e| e.to_string().into_value(ctx))? {
                Some(line) => Ok(ctx.intern(&line).into()),
                None => Ok(Value::Nil),
            }
        }
        Some("n") => {
            let buf;
            match read_line(reader, false).map_err(|e| e.to_string().into_value(ctx))? {
                Some(line) => buf = line,
                None => return Ok(Value::Nil),
            }
            let text = std::string::String::from_utf8_lossy(&buf);
            Ok(text
                .trim()
                .parse::<i64>()
                .map(Value::Integer)
                .or_else(|_| text.trim().parse::<f64>().map(Value::Number))
                .unwrap_or(Value::Nil))
        }
        Some(other) => Err(format!("bad read format '{other}'").into_value(ctx).into()),
    }
}

fn read_from_stdin<'gc>(
    ctx: Context<'gc>,
    spec: Option<std::string::String>,
    format: Value<'gc>,
) -> Result<Value<'gc>, crate::Error<'gc>> {
    let stdin = std::io::stdin();
    let mut locked = stdin.lock();
    match spec.as_deref().map(|s| s.trim_start_matches('*')) {
        Some("a") => {
            let mut buf = Vec::new();
            locked
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string().into_value(ctx))?;
            Ok(ctx.intern(&buf).into())
        }
        None => {
            let count = format.to_integer().unwrap_or(0).max(0) as usize;
            let mut buf = vec![0u8; count];
            let mut filled = 0;
            while filled < count {
                match locked.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => return Err(e.to_string().into_value(ctx).into()),
                }
            }
            buf.truncate(filled);
            Ok(ctx.intern(&buf).into())
        }
        _ => {
            let mut line = std::string::String::new();
            match locked.read_line(&mut line) {
                Ok(0) => Ok(Value::Nil),
                Ok(_) => {
                    let keep = spec.as_deref().map(|s| s.ends_with('L')).unwrap_or(false);
                    if !keep {
                        while line.ends_with('\n') || line.ends_with('\r') {
                            line.pop();
                        }
                    }
                    Ok(ctx.intern(line.as_bytes()).into())
                }
                Err(e) => Err(e.to_string().into_value(ctx).into()),
            }
        }
    }
}

fn write_all<'gc>(
    ctx: Context<'gc>,
    handle: &mut Handle,
    values: &[Value<'gc>],
) -> Result<(), crate::Error<'gc>> {
    let mut bytes = Vec::new();
    for v in values {
        match v {
            Value::String(s) => bytes.extend_from_slice(s.as_bytes()),
            Value::Integer(i) => bytes.extend_from_slice(i.to_string().as_bytes()),
            Value::Number(n) => bytes.extend_from_slice(n.to_string().as_bytes()),
            other => {
                return Err(format!("cannot write a {}", other.type_name())
                    .into_value(ctx)
                    .into())
            }
        }
    }

    let result = match handle {
        Handle::File(f) => f.get_mut().write_all(&bytes),
        Handle::Process(child, true) => match child.stdin.as_mut() {
            Some(stdin) => stdin.write_all(&bytes),
            None => return Err("process stdin is closed".into_value(ctx).into()),
        },
        Handle::Process(_, false) => {
            return Err("file is not opened for writing".into_value(ctx).into())
        }
        Handle::Stdout => std::io::stdout().write_all(&bytes),
        Handle::Stderr => std::io::stderr().write_all(&bytes),
        Handle::Stdin => {
            return Err("file is not opened for writing".into_value(ctx).into());
        }
        Handle::Closed => return Err("attempt to use a closed file".into_value(ctx).into()),
    };
    result.map_err(|e| e.to_string().into_value(ctx).into())
}

/// Loads `print` and the `io` library.
pub fn load_io<'gc>(ctx: Context<'gc>) {
    ctx.set_global(
        "print",
        Callback::from_fn(&ctx, |_ctx, _, mut stack| {
            let mut out = std::io::stdout().lock();
            for (i, value) in stack.drain(..).enumerate() {
                if i > 0 {
                    let _ = write!(out, "\t");
                }
                let _ = write!(out, "{}", value.display());
            }
            let _ = writeln!(out);
            let _ = out.flush();
            Ok(CallbackReturn::Return)
        }),
    );

    // The methods every handle shares. `__index` points back at this table, so `f:read(…)` and
    // `io.read(…)` reach the same code.
    let methods = Table::new(&ctx);

    methods.set_field(
        ctx,
        "read",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let this = stack.get(0);
            let file = handle_of(ctx, this)?;
            let formats: Vec<Value> = stack.drain(1..).collect();
            let mut handle = file.0.borrow_mut();

            if formats.is_empty() {
                let v = read_one(ctx, &mut handle, Value::Nil)?;
                stack.replace(ctx, v);
            } else {
                let mut results = Vec::new();
                for f in formats {
                    let v = read_one(ctx, &mut handle, f)?;
                    let stop = v.is_nil();
                    results.push(v);
                    if stop {
                        break;
                    }
                }
                stack.replace(ctx, Variadic(results));
            }
            Ok(CallbackReturn::Return)
        }),
    );

    methods.set_field(
        ctx,
        "write",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let this = stack.get(0);
            let file = handle_of(ctx, this)?;
            let values: Vec<Value> = stack.drain(1..).collect();
            write_all(ctx, &mut file.0.borrow_mut(), &values)?;
            // Returns the file, so writes chain.
            stack.replace(ctx, this);
            Ok(CallbackReturn::Return)
        }),
    );

    methods.set_field(
        ctx,
        "lines",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let this = stack.get(0);
            handle_of(ctx, this)?;
            let ud = match this {
                Value::UserData(ud) => ud,
                _ => return Err("not a file handle".into_value(ctx).into()),
            };
            // An iterator closure over the same handle.
            stack.replace(
                ctx,
                Callback::from_fn_with(&ctx, ud, |ud, ctx, _, mut stack| {
                    let file = ud
                        .downcast_static::<FileHandle>()
                        .map_err(|_| "not a file handle".into_value(ctx))?;
                    let v = read_one(ctx, &mut file.0.borrow_mut(), Value::Nil)?;
                    stack.replace(ctx, v);
                    Ok(CallbackReturn::Return)
                }),
            );
            Ok(CallbackReturn::Return)
        }),
    );

    methods.set_field(
        ctx,
        "close",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let this = stack.get(0);
            let file = handle_of(ctx, this)?;
            let mut handle = file.0.borrow_mut();
            // Closing a popen handle reaps the child, as PUC-Rio does. Closing a standard stream
            // is a no-op: the process still needs it.
            match &mut *handle {
                Handle::File(_) => *handle = Handle::Closed,
                Handle::Process(child, _) => {
                    drop(child.stdin.take());
                    child.wait().ok();
                    *handle = Handle::Closed;
                }
                _ => {}
            }
            stack.replace(ctx, true);
            Ok(CallbackReturn::Return)
        }),
    );

    methods.set_field(
        ctx,
        "flush",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let this = stack.get(0);
            let file = handle_of(ctx, this)?;
            let mut handle = file.0.borrow_mut();
            let _ = match &mut *handle {
                Handle::File(f) => f.get_mut().flush(),
                Handle::Stdout => std::io::stdout().flush(),
                Handle::Stderr => std::io::stderr().flush(),
                _ => Ok(()),
            };
            stack.replace(ctx, this);
            Ok(CallbackReturn::Return)
        }),
    );

    methods.set_field(
        ctx,
        "seek",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let this = stack.get(0);
            let file = handle_of(ctx, this)?;
            let whence: Option<String> = match stack.get(1) {
                Value::String(s) => Some(s),
                _ => None,
            };
            let offset = stack.get(2).to_integer().unwrap_or(0);
            let whence = whence
                .map(|w| w.display_lossy().to_string())
                .unwrap_or_else(|| "cur".to_owned());

            let mut handle = file.0.borrow_mut();
            match &mut *handle {
                Handle::File(f) => {
                    let pos = match whence.as_str() {
                        "set" => SeekFrom::Start(offset.max(0) as u64),
                        "end" => SeekFrom::End(offset),
                        _ => SeekFrom::Current(offset),
                    };
                    let at = f.seek(pos).map_err(|e| e.to_string().into_value(ctx))?;
                    stack.replace(ctx, at as i64);
                }
                _ => return Err("cannot seek this file".into_value(ctx).into()),
            }
            Ok(CallbackReturn::Return)
        }),
    );

    methods.set_field(
        ctx,
        "setvbuf",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            // Accepted and reported as succeeding, but the mode is not honoured: reads go through
            // a `BufReader` and writes go straight out, and neither is reconfigurable per handle.
            // Answering `false` would be worse — it would make callers think the file is broken.
            let this = stack.get(0);
            handle_of(ctx, this)?;
            stack.replace(ctx, true);
            Ok(CallbackReturn::Return)
        }),
    );

    let FileMetatable(mt) = *ctx.singleton::<Rootable![FileMetatable<'_>]>();
    mt.set_field(ctx, "__index", methods);
    mt.set_field(ctx, "__name", ctx.intern(b"FILE*"));

    let io = Table::new(&ctx);

    let stdout = new_handle(ctx, Handle::Stdout);
    let stderr = new_handle(ctx, Handle::Stderr);
    let stdin = new_handle(ctx, Handle::Stdin);
    io.set_field(ctx, "stdout", stdout);
    io.set_field(ctx, "stderr", stderr);
    io.set_field(ctx, "stdin", stdin);

    // Redirected by `io.input`/`io.output`; every default-stream user reads them through this.
    let streams = DefaultStreams::new(ctx, stdin, stdout);

    io.set_field(
        ctx,
        "open",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (path, mode): (String, Option<String>) = stack.consume(ctx)?;
            let path = path.display_lossy().to_string();
            let mode = mode
                .map(|m| m.display_lossy().to_string())
                .unwrap_or_else(|| "r".to_owned());

            let mut options = std::fs::OpenOptions::new();
            match mode.trim_end_matches('b') {
                "r" => options.read(true),
                "w" => options.write(true).create(true).truncate(true),
                "a" => options.append(true).create(true),
                "r+" => options.read(true).write(true),
                "w+" => options.read(true).write(true).create(true).truncate(true),
                "a+" => options.read(true).append(true).create(true),
                _ => return Err("bad mode to 'open'".into_value(ctx).into()),
            };

            match options.open(&path) {
                Ok(f) => {
                    let handle = new_handle(ctx, Handle::File(BufReader::new(f)));
                    stack.replace(ctx, handle);
                }
                Err(err) => {
                    let msg = ctx.intern(format!("{path}: {err}").as_bytes());
                    stack.replace(ctx, (Value::Nil, msg));
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    // Same policy note as `os.execute`: `std::process::Command`, no C. A host that does not want
    // scripts spawning processes removes this field after loading `io`.
    io.set_field(
        ctx,
        "popen",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (command, mode): (String, Option<String>) = stack.consume(ctx)?;
            let command = command.display_lossy().to_string();
            let mode = mode
                .map(|m| m.display_lossy().to_string())
                .unwrap_or_else(|| "r".to_owned());

            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c").arg(&command);
            let spawned = if mode.starts_with('w') {
                cmd.stdin(std::process::Stdio::piped()).spawn()
            } else {
                cmd.stdout(std::process::Stdio::piped()).spawn()
            };

            match spawned {
                Ok(child) => {
                    let handle =
                        new_handle(ctx, Handle::Process(Box::new(child), mode.starts_with('w')));
                    stack.replace(ctx, handle);
                }
                Err(err) => {
                    let msg = ctx.intern(format!("{command}: {err}").as_bytes());
                    stack.replace(ctx, (Value::Nil, msg));
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    io.set_field(
        ctx,
        "lines",
        Callback::from_fn_with(&ctx, streams, |streams, ctx, _, mut stack| {
            // With no filename, iterate the current input stream, as PUC-Rio does.
            let ud = match stack.consume::<Option<String>>(ctx)? {
                None => streams.input.get(),
                Some(path) => {
                    let path = path.display_lossy().to_string();
                    let file = std::fs::File::open(&path)
                        .map_err(|e| format!("{path}: {e}").into_value(ctx))?;
                    new_handle(ctx, Handle::File(BufReader::new(file)))
                }
            };
            stack.replace(
                ctx,
                Callback::from_fn_with(&ctx, ud, |ud, ctx, _, mut stack| {
                    let file = ud
                        .downcast_static::<FileHandle>()
                        .map_err(|_| "not a file handle".into_value(ctx))?;
                    let v = read_one(ctx, &mut file.0.borrow_mut(), Value::Nil)?;
                    stack.replace(ctx, v);
                    Ok(CallbackReturn::Return)
                }),
            );
            Ok(CallbackReturn::Return)
        }),
    );

    io.set_field(
        ctx,
        "close",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            if let Value::UserData(_) = stack.get(0) {
                let file = handle_of(ctx, stack.get(0))?;
                let mut handle = file.0.borrow_mut();
                if matches!(*handle, Handle::File(_)) {
                    *handle = Handle::Closed;
                }
            }
            stack.replace(ctx, true);
            Ok(CallbackReturn::Return)
        }),
    );

    io.set_field(
        ctx,
        "write",
        Callback::from_fn_with(&ctx, streams, |streams, ctx, _, mut stack| {
            let output = streams.output.get();
            let file = handle_of(ctx, output.into())?;
            let values: Vec<Value> = stack.drain(..).collect();
            write_all(ctx, &mut file.0.borrow_mut(), &values)?;
            stack.replace(ctx, output);
            Ok(CallbackReturn::Return)
        }),
    );

    io.set_field(
        ctx,
        "read",
        Callback::from_fn_with(&ctx, streams, |streams, ctx, _, mut stack| {
            let input = streams.input.get();
            let file = handle_of(ctx, input.into())?;
            let format = stack.get(0);
            let v = read_one(ctx, &mut file.0.borrow_mut(), format)?;
            stack.replace(ctx, v);
            Ok(CallbackReturn::Return)
        }),
    );

    // `io.input()` / `io.output()` read the current stream; with an argument they replace it. A
    // string is opened as a file, matching PUC-Rio, which is why these can fail.
    for (name, write) in [("input", false), ("output", true)] {
        io.set_field(
            ctx,
            name,
            Callback::from_fn_with(
                &ctx,
                (write, streams),
                move |(write, streams), ctx, _, mut stack| {
                    let slot = if *write {
                        streams.output
                    } else {
                        streams.input
                    };
                    match stack.get(0) {
                        Value::Nil => {}
                        argument => slot.set(&ctx, stream_argument(ctx, argument, *write)?),
                    }
                    stack.replace(ctx, slot.get());
                    Ok(CallbackReturn::Return)
                },
            ),
        );
    }

    io.set_field(
        ctx,
        "flush",
        Callback::from_fn_with(&ctx, streams, |streams, ctx, _, mut stack| {
            let output = streams.output.get();
            let file = handle_of(ctx, output.into())?;
            flush_handle(ctx, &mut file.0.borrow_mut())?;
            stack.replace(ctx, output);
            Ok(CallbackReturn::Return)
        }),
    );

    io.set_field(
        ctx,
        "tmpfile",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            // Created and immediately unlinked, so it disappears when the handle is dropped. That
            // is what PUC-Rio's `tmpfile` promises and it needs no cleanup path of our own.
            let path = std::env::temp_dir().join(format!(
                "luna_{:x}_{:x}",
                std::process::id(),
                crate::stdlib::os::process_start().elapsed().as_nanos()
            ));
            match std::fs::File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(f) => {
                    let _ = std::fs::remove_file(&path);
                    stack.replace(ctx, new_handle(ctx, Handle::File(BufReader::new(f))));
                }
                Err(err) => {
                    stack.replace(ctx, (Value::Nil, ctx.intern(err.to_string().as_bytes())));
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    io.set_field(
        ctx,
        "type",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value = stack.get(0);
            let result = match value {
                Value::UserData(ud) => match ud.downcast_static::<FileHandle>() {
                    Ok(f) => {
                        if f.0.borrow().is_closed() {
                            Value::from(ctx.intern(b"closed file"))
                        } else {
                            Value::from(ctx.intern(b"file"))
                        }
                    }
                    Err(_) => Value::Nil,
                },
                _ => Value::Nil,
            };
            stack.replace(ctx, result);
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global("io", io);
}
