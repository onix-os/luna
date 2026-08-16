use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};

use ottavino_gc_arena::{Collect, Rootable};

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

fn new_handle<'gc>(ctx: Context<'gc>, handle: Handle) -> UserData<'gc> {
    let ud = UserData::new_static(&ctx, FileHandle(RefCell::new(handle)));
    let FileMetatable(mt) = *ctx.singleton::<Rootable![FileMetatable<'_>]>();
    ud.set_metatable(&ctx, Some(mt));
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
            // Closing a standard stream is a no-op: the process still needs it.
            if matches!(*handle, Handle::File(_)) {
                *handle = Handle::Closed;
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

    io.set_field(
        ctx,
        "lines",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let path: String = stack.consume(ctx)?;
            let path = path.display_lossy().to_string();
            let file =
                std::fs::File::open(&path).map_err(|e| format!("{path}: {e}").into_value(ctx))?;
            let ud = new_handle(ctx, Handle::File(BufReader::new(file)));
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
        Callback::from_fn_with(&ctx, stdout, |stdout, ctx, _, mut stack| {
            let file = stdout
                .downcast_static::<FileHandle>()
                .map_err(|_| "not a file handle".into_value(ctx))?;
            let values: Vec<Value> = stack.drain(..).collect();
            write_all(ctx, &mut file.0.borrow_mut(), &values)?;
            stack.replace(ctx, *stdout);
            Ok(CallbackReturn::Return)
        }),
    );

    io.set_field(
        ctx,
        "read",
        Callback::from_fn_with(&ctx, stdin, |stdin, ctx, _, mut stack| {
            let file = stdin
                .downcast_static::<FileHandle>()
                .map_err(|_| "not a file handle".into_value(ctx))?;
            let format = stack.get(0);
            let v = read_one(ctx, &mut file.0.borrow_mut(), format)?;
            stack.replace(ctx, v);
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
