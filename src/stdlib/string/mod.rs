pub(crate) mod format;
mod pack;
mod pattern;

use std::cell::Cell;

use ottavino_gc_arena::{Collect, Gc, Rootable};

use crate::{
    async_sequence, meta_ops, Callback, CallbackReturn, Context, Error, IntoValue, SequenceReturn,
    Singleton, String, Table, Value, Variadic,
};

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct StringMetatable<'gc>(pub Table<'gc>);

impl<'gc> Singleton<'gc> for StringMetatable<'gc> {
    fn create(ctx: Context<'gc>) -> Self {
        StringMetatable(Table::new(&ctx))
    }
}

const SHORT_STRING_THRESHOLD: usize = 40;

fn make_string<'gc>(ctx: Context<'gc>, bytes: &[u8]) -> String<'gc> {
    if bytes.len() <= SHORT_STRING_THRESHOLD {
        ctx.intern(bytes)
    } else {
        String::from_slice(&ctx, bytes)
    }
}

#[derive(Collect)]
#[collect(no_drop)]
struct GmatchState<'gc> {
    source: String<'gc>,
    pattern: String<'gc>,
    #[collect(require_static)]
    pos: Cell<usize>,
    #[collect(require_static)]
    last_end: Cell<Option<usize>>,
}

pub fn load_string<'gc>(ctx: Context<'gc>) {
    let string_lib = Table::new(&ctx);

    string_lib.set_field(
        ctx,
        "len",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let s = stack.consume::<String>(ctx)?;
            stack.replace(ctx, s.len());
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "byte",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (string, i, j) = stack.consume::<(String, Option<i64>, Option<i64>)>(ctx)?;
            let i = i.unwrap_or(1);
            let substr = sub_bytes(string.as_bytes(), i, j.or(Some(i)))?;
            stack.extend(substr.iter().map(|b| Value::Integer(i64::from(*b))));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "char",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let mut buf = Vec::with_capacity(stack.len());
            for v in stack.drain(..) {
                match v {
                    Value::Integer(i) => {
                        if !(0..=255).contains(&i) {
                            return Err("bad argument to 'char' (value out of range)"
                                .into_value(ctx)
                                .into());
                        }
                        buf.push(i as u8);
                    }
                    Value::Number(n) => {
                        let i = n as i64;
                        if !(0..=255).contains(&i) || (i as f64) != n {
                            return Err("bad argument to 'char' (value out of range)"
                                .into_value(ctx)
                                .into());
                        }
                        buf.push(i as u8);
                    }
                    Value::String(s) => {
                        // Coerce string to number
                        let s_str = std::str::from_utf8(s.as_bytes()).unwrap_or("");
                        let i: i64 = s_str.trim().parse().map_err(|_| {
                            "bad argument to 'char' (number expected)".into_value(ctx)
                        })?;
                        if !(0..=255).contains(&i) {
                            return Err("bad argument to 'char' (value out of range)"
                                .into_value(ctx)
                                .into());
                        }
                        buf.push(i as u8);
                    }
                    _ => {
                        return Err("bad argument to 'char' (number expected)"
                            .into_value(ctx)
                            .into());
                    }
                }
            }
            stack.replace(ctx, ctx.intern(&buf));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "sub",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (string, i, j) = stack.consume::<(String, i64, Option<i64>)>(ctx)?;
            let substr = ctx.intern(sub_bytes(string.as_bytes(), i, j)?);
            stack.replace(ctx, substr);
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "lower",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let s = stack.consume::<String>(ctx)?;
            let lower: Vec<u8> = s
                .as_bytes()
                .iter()
                .map(|b| b.to_ascii_lowercase())
                .collect();
            stack.replace(ctx, make_string(ctx, &lower));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "upper",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let s = stack.consume::<String>(ctx)?;
            let upper: Vec<u8> = s
                .as_bytes()
                .iter()
                .map(|b| b.to_ascii_uppercase())
                .collect();
            stack.replace(ctx, make_string(ctx, &upper));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "dump",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (function, strip): (crate::Function, Option<bool>) = stack.consume(ctx)?;
            let crate::Function::Closure(closure) = function else {
                // A Rust callback has no bytecode to write out, which is what PUC-Rio says about
                // C functions too.
                return Err("unable to dump given function".into_value(ctx).into());
            };
            // Loading produces a *top-level* function, and luna only builds one from a prototype
            // whose sole upvalue is `_ENV`. A nested function reaches `_ENV` through its parent
            // instead, so it has no meaning on its own — refusing here beats handing back bytes
            // that cannot be loaded. Chunks are what precompilation actually wants anyway.
            let upvalues = &closure.prototype().upvalues;
            let loadable = upvalues.is_empty()
                || (upvalues.len() == 1
                    && upvalues[0] == crate::types::UpValueDescriptor::Environment);
            if !loadable {
                return Err(
                    "unable to dump this function: only a chunk, whose one upvalue is _ENV, can be \
                     loaded back"
                        .into_value(ctx)
                        .into(),
                );
            }
            let bytes = crate::dump::dump(&closure.prototype(), strip.unwrap_or(false));
            stack.replace(ctx, ctx.intern(&bytes));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "reverse",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let s = stack.consume::<String>(ctx)?;
            let rev: Vec<u8> = s.as_bytes().iter().copied().rev().collect();
            stack.replace(ctx, ctx.intern(&rev));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "pack",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let format: String = stack.from_front(ctx)?;
            let parsed = pack::parse_format(format.as_bytes()).map_err(|e| e.into_value(ctx))?;

            let mut out = Vec::new();
            let mut arg = 0;
            // Every option reserves its width before writing it, so a format that repeats a wide
            // `c` cannot allocate its way past the string length cap.
            let grow = |out: &Vec<u8>, add: usize| {
                pack::room(out.len(), add)
                    .map(|_| ())
                    .map_err(|e| Error::from(e.into_value(ctx)))
            };
            for item in &parsed.items {
                match item {
                    pack::Item::Padding => {
                        grow(&out, 1)?;
                        out.push(0);
                    }
                    pack::Item::Int { size, .. } => {
                        let v = stack.get(arg).to_integer().ok_or_else(|| {
                            "bad argument to 'pack' (number expected)".into_value(ctx)
                        })?;
                        arg += 1;
                        grow(&out, *size)?;
                        pack::write_int(&mut out, v, *size, parsed.little_endian);
                    }
                    pack::Item::Float => {
                        let v = stack.get(arg).to_number().ok_or_else(|| {
                            "bad argument to 'pack' (number expected)".into_value(ctx)
                        })? as f32;
                        arg += 1;
                        grow(&out, 4)?;
                        let bytes = if parsed.little_endian {
                            v.to_le_bytes()
                        } else {
                            v.to_be_bytes()
                        };
                        out.extend_from_slice(&bytes);
                    }
                    pack::Item::Double => {
                        let v = stack.get(arg).to_number().ok_or_else(|| {
                            "bad argument to 'pack' (number expected)".into_value(ctx)
                        })?;
                        arg += 1;
                        grow(&out, 8)?;
                        let bytes = if parsed.little_endian {
                            v.to_le_bytes()
                        } else {
                            v.to_be_bytes()
                        };
                        out.extend_from_slice(&bytes);
                    }
                    pack::Item::LenString { size } => {
                        let s: String = match stack.get(arg) {
                            Value::String(s) => s,
                            v => ctx.intern(v.display().to_string().as_bytes()),
                        };
                        arg += 1;
                        grow(&out, size + s.as_bytes().len())?;
                        pack::write_int(
                            &mut out,
                            s.as_bytes().len() as i64,
                            *size,
                            parsed.little_endian,
                        );
                        out.extend_from_slice(s.as_bytes());
                    }
                    pack::Item::ZeroString => {
                        let s: String = match stack.get(arg) {
                            Value::String(s) => s,
                            v => ctx.intern(v.display().to_string().as_bytes()),
                        };
                        arg += 1;
                        grow(&out, s.as_bytes().len() + 1)?;
                        out.extend_from_slice(s.as_bytes());
                        out.push(0);
                    }
                    pack::Item::FixedString { size } => {
                        let s: String = match stack.get(arg) {
                            Value::String(s) => s,
                            v => ctx.intern(v.display().to_string().as_bytes()),
                        };
                        arg += 1;
                        let bytes = s.as_bytes();
                        if bytes.len() > *size {
                            return Err("string longer than given size".into_value(ctx).into());
                        }
                        grow(&out, *size)?;
                        out.extend_from_slice(bytes);
                        out.resize(out.len() + (*size - bytes.len()), 0);
                    }
                }
            }

            stack.replace(ctx, ctx.intern(&out));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "packsize",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let format: String = stack.consume(ctx)?;
            let parsed = pack::parse_format(format.as_bytes()).map_err(|e| e.into_value(ctx))?;
            let size = pack::packed_size(&parsed).map_err(|e| e.into_value(ctx))?;
            stack.replace(ctx, size as i64);
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "unpack",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (format, data, pos): (String, String, Option<i64>) = stack.consume(ctx)?;
            let parsed = pack::parse_format(format.as_bytes()).map_err(|e| e.into_value(ctx))?;
            let bytes = data.as_bytes();
            let mut at = unpack_init(bytes.len(), pos.unwrap_or(1)).ok_or_else(|| {
                "bad argument #3 to 'unpack' (initial position out of string)".into_value(ctx)
            })?;

            let short = || Error::from("data string too short".into_value(ctx));
            // Every width below is checked against the remaining data before it is sliced: a
            // length read out of the data itself can be large enough to overflow `at + n`.
            let room = |at: usize, n: usize| at.checked_add(n).filter(|e| *e <= bytes.len());
            let mut out: Vec<Value> = Vec::new();

            for item in &parsed.items {
                match item {
                    pack::Item::Padding => {
                        at = room(at, 1).ok_or_else(short)?;
                    }
                    pack::Item::Int { size, signed } => {
                        let end = room(at, *size).ok_or_else(short)?;
                        let v =
                            pack::read_int(&bytes[at..end], *size, *signed, parsed.little_endian)
                                .map_err(|e| e.into_value(ctx))?;
                        out.push(Value::Integer(v));
                        at = end;
                    }
                    pack::Item::Float => {
                        if room(at, 4).is_none() {
                            return Err(short());
                        }
                        let raw: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
                        let v = if parsed.little_endian {
                            f32::from_le_bytes(raw)
                        } else {
                            f32::from_be_bytes(raw)
                        };
                        out.push(Value::Number(v as f64));
                        at += 4;
                    }
                    pack::Item::Double => {
                        if room(at, 8).is_none() {
                            return Err(short());
                        }
                        let raw: [u8; 8] = bytes[at..at + 8].try_into().unwrap();
                        let v = if parsed.little_endian {
                            f64::from_le_bytes(raw)
                        } else {
                            f64::from_be_bytes(raw)
                        };
                        out.push(Value::Number(v));
                        at += 8;
                    }
                    pack::Item::LenString { size } => {
                        let head = room(at, *size).ok_or_else(short)?;
                        let len =
                            pack::read_int(&bytes[at..head], *size, false, parsed.little_endian)
                                .map_err(|e| e.into_value(ctx))?
                                as usize;
                        let end = room(head, len).ok_or_else(short)?;
                        out.push(ctx.intern(&bytes[head..end]).into());
                        at = end;
                    }
                    pack::Item::ZeroString => {
                        let end = bytes[at..]
                            .iter()
                            .position(|b| *b == 0)
                            .map(|p| at + p)
                            .ok_or_else(|| "unfinished string for format 'z'".into_value(ctx))?;
                        out.push(ctx.intern(&bytes[at..end]).into());
                        at = end + 1;
                    }
                    pack::Item::FixedString { size } => {
                        let end = room(at, *size).ok_or_else(short)?;
                        out.push(ctx.intern(&bytes[at..end]).into());
                        at = end;
                    }
                }
            }

            // Lua returns the position just past what was read, as the last value.
            out.push(Value::Integer(at as i64 + 1));
            stack.replace(ctx, Variadic(out));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "rep",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, n, sep) = stack.consume::<(String, i64, Option<String>)>(ctx)?;
            if n <= 0 {
                stack.replace(ctx, ctx.intern(b""));
                return Ok(CallbackReturn::Return);
            }
            let sep_bytes: &[u8] = sep.as_ref().map(|s| s.as_bytes()).unwrap_or(b"");
            let s_bytes = s.as_bytes();
            // Calculate total size and check for overflow / too-large
            let rep_n = n as usize;
            let sep_total = sep_bytes.len().saturating_mul(rep_n.saturating_sub(1));
            let s_total = s_bytes.len().saturating_mul(rep_n);
            let total = s_total
                .checked_add(sep_total)
                .ok_or_else(|| "resulting string too large".into_value(ctx))?;
            // Cap at ~1GiB like PUC-Rio Lua
            if total > crate::string::MAX_STRING_LENGTH {
                return Err("resulting string too large".into_value(ctx).into());
            }
            let mut buf = Vec::with_capacity(total);
            for i in 0..rep_n {
                if i > 0 {
                    buf.extend_from_slice(sep_bytes);
                }
                buf.extend_from_slice(s_bytes);
            }
            stack.replace(ctx, make_string(ctx, &buf));
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "find",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, pat, init, plain) =
                stack.consume::<(String, String, Option<i64>, Option<Value>)>(ctx)?;
            let src = s.as_bytes();
            let pat_bytes = pat.as_bytes();
            let plain_flag = match plain {
                Some(Value::Boolean(b)) => b,
                Some(v) => v.to_bool(),
                None => false,
            };

            let init = normalise_init(src.len(), init.unwrap_or(1));

            if plain_flag || pattern::is_plain(pat_bytes) {
                // Plain search
                match find_plain(src, pat_bytes, init) {
                    None => {
                        stack.replace(ctx, Value::Nil);
                    }
                    Some(pos) => {
                        let start = (pos + 1) as i64;
                        let end = (pos + pat_bytes.len()) as i64;
                        stack.replace(ctx, (start, end));
                    }
                }
            } else {
                match pattern::find(src, pat_bytes, init) {
                    Err(e) => return Err(e.into_value(ctx).into()),
                    Ok(None) => {
                        stack.replace(ctx, Value::Nil);
                    }
                    Ok(Some((start, end, captures))) => {
                        let start_ret = (start + 1) as i64;
                        let end_ret = end as i64;
                        stack.push_front(Value::Integer(end_ret));
                        stack.push_front(Value::Integer(start_ret));
                        // push captures
                        for cap in &captures {
                            match cap {
                                pattern::Capture::Substring(cs, ce) => {
                                    stack.push_back(ctx.intern(&src[*cs..*ce]).into());
                                }
                                pattern::Capture::Position(p) => {
                                    stack.push_back(Value::Integer(*p as i64));
                                }
                            }
                        }
                    }
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "match",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, pat, init) = stack.consume::<(String, String, Option<i64>)>(ctx)?;
            let src = s.as_bytes();
            let init = normalise_init(src.len(), init.unwrap_or(1));

            match pattern::find(src, pat.as_bytes(), init) {
                Err(e) => Err(e.into_value(ctx).into()),
                Ok(None) => {
                    stack.replace(ctx, Value::Nil);
                    Ok(CallbackReturn::Return)
                }
                Ok(Some((start, end, captures))) => {
                    if captures.is_empty() {
                        // Whole match
                        stack.replace(ctx, ctx.intern(&src[start..end]));
                    } else {
                        stack.drain(..);
                        for cap in &captures {
                            match cap {
                                pattern::Capture::Substring(cs, ce) => {
                                    stack.push_back(ctx.intern(&src[*cs..*ce]).into());
                                }
                                pattern::Capture::Position(p) => {
                                    stack.push_back(Value::Integer(*p as i64));
                                }
                            }
                        }
                    }
                    Ok(CallbackReturn::Return)
                }
            }
        }),
    );

    string_lib.set_field(
        ctx,
        "gmatch",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, pat, init) = stack.consume::<(String, String, Option<i64>)>(ctx)?;
            // `init` follows the same convention as `find`: 1-based, negative counts from the end.
            let start = match init {
                None => 0,
                Some(i) => {
                    let len = s.as_bytes().len() as i64;
                    let at = if i < 0 {
                        (len + i + 1).max(1)
                    } else {
                        i.max(1)
                    };
                    (at - 1).clamp(0, len) as usize
                }
            };
            let state = Gc::new(
                &ctx,
                GmatchState {
                    source: s,
                    pattern: pat,
                    pos: Cell::new(start),
                    last_end: Cell::new(None),
                },
            );
            let iter = Callback::from_fn_with(&ctx, state, |state, ctx, _, mut stack| {
                let src = state.source.as_bytes();
                let pat = state.pattern.as_bytes();
                let pos = state.pos.get();
                let last_end = state.last_end.get();

                match pattern::find_next(src, pat, pos, last_end) {
                    Err(e) => Err(e.into_value(ctx).into()),
                    Ok(None) => {
                        stack.replace(ctx, Value::Nil);
                        Ok(CallbackReturn::Return)
                    }
                    Ok(Some(m)) => {
                        // Advance iterator
                        let next_pos = if m.end > m.start { m.end } else { m.end + 1 };
                        state.pos.set(next_pos);
                        state.last_end.set(Some(m.end));

                        stack.drain(..);
                        if m.captures.is_empty() {
                            stack.push_back(ctx.intern(&src[m.start..m.end]).into());
                        } else {
                            for cap in &m.captures {
                                match cap {
                                    pattern::Capture::Substring(cs, ce) => {
                                        stack.push_back(ctx.intern(&src[*cs..*ce]).into());
                                    }
                                    pattern::Capture::Position(p) => {
                                        stack.push_back(Value::Integer(*p as i64));
                                    }
                                }
                            }
                        }
                        Ok(CallbackReturn::Return)
                    }
                }
            });
            stack.replace(ctx, iter);
            Ok(CallbackReturn::Return)
        }),
    );

    string_lib.set_field(
        ctx,
        "gsub",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, pat, repl, max_n) =
                stack.consume::<(String, String, Value, Option<i64>)>(ctx)?;

            let src_bytes = s.as_bytes().to_vec();
            let pat_bytes = pat.as_bytes().to_vec();
            let max_subs = max_n.unwrap_or(i64::MAX);

            match repl {
                Value::String(repl_s) => {
                    // String replacement – no async needed
                    let repl_bytes = repl_s.as_bytes().to_vec();
                    let (result, count) =
                        gsub_string(&src_bytes, &pat_bytes, &repl_bytes, max_subs)
                            .map_err(|e| e.into_value(ctx))?;
                    let res_str = make_string(ctx, &result);
                    stack.replace(ctx, (res_str, count));
                    Ok(CallbackReturn::Return)
                }
                Value::Table(t) => {
                    // Table replacement: table lookup, no Lua call, so no async needed.
                    let pp = pattern::prepare(&pat_bytes).map_err(|e| e.into_value(ctx))?;
                    let anchored = is_anchored(&pat_bytes);
                    let mut result: Vec<u8> = Vec::new();
                    let mut at = 0usize;
                    let mut count = 0i64;
                    let mut last_match: Option<usize> = None;

                    while count < max_subs {
                        let found = pattern::match_at(&src_bytes, &pat_bytes, pp, at)
                            .map_err(|e| e.into_value(ctx))?;
                        let m = match found {
                            Some(m) if Some(m.end) != last_match => m,
                            _ if at < src_bytes.len() => {
                                result.push(src_bytes[at]);
                                at += 1;
                                if anchored {
                                    break;
                                }
                                continue;
                            }
                            _ => break,
                        };

                        // Key = first capture or whole match
                        let key_bytes = if m.captures.is_empty() {
                            src_bytes[m.start..m.end].to_vec()
                        } else {
                            match m.captures[0] {
                                pattern::Capture::Substring(cs, ce) => src_bytes[cs..ce].to_vec(),
                                pattern::Capture::Position(p) => p.to_string().into_bytes(),
                            }
                        };
                        let key = ctx.intern(&key_bytes);
                        let v = index_through_tables(ctx, t, key);
                        if v.to_bool() {
                            match v {
                                Value::String(s) => result.extend_from_slice(s.as_bytes()),
                                Value::Integer(i) => {
                                    result.extend_from_slice(i.to_string().as_bytes())
                                }
                                Value::Number(n) => result.extend_from_slice(
                                    Value::Number(n).display().to_string().as_bytes(),
                                ),
                                _ => {
                                    return Err(
                                        "invalid replacement value (string/number expected)"
                                            .into_value(ctx)
                                            .into(),
                                    )
                                }
                            }
                        } else {
                            result.extend_from_slice(&src_bytes[m.start..m.end]);
                        }

                        count += 1;
                        last_match = Some(m.end);
                        at = m.end;
                        if anchored {
                            break;
                        }
                    }
                    result.extend_from_slice(&src_bytes[at..]);
                    let res_str = make_string(ctx, &result);
                    stack.replace(ctx, (res_str, count));
                    Ok(CallbackReturn::Return)
                }
                Value::Function(f) => {
                    // Function replacement: call f(captures...) for each match
                    let seq = async_sequence(&ctx, |locals, mut seq| {
                        let src_ref = locals.stash(&ctx, ctx.intern(&src_bytes));
                        let pat_ref = locals.stash(&ctx, ctx.intern(&pat_bytes));
                        let f_ref = locals.stash(&ctx, f);
                        async move {
                            let src: Vec<u8> = seq.enter(|_, locals, _, _| {
                                locals.fetch(&src_ref).as_bytes().to_vec()
                            });
                            let pat: Vec<u8> = seq.enter(|_, locals, _, _| {
                                locals.fetch(&pat_ref).as_bytes().to_vec()
                            });

                            let pp = seq.try_enter(|ctx, _, _, _| {
                                pattern::prepare(&pat)
                                    .map_err(|e: std::string::String| e.into_value(ctx).into())
                            })?;
                            let anchored = is_anchored(&pat);
                            let mut result: Vec<u8> = Vec::new();
                            let mut at = 0usize;
                            let mut count = 0i64;
                            let mut last_match: Option<usize> = None;

                            while count < max_subs {
                                let found = seq.try_enter(|ctx, _, _, _| {
                                    pattern::match_at(&src, &pat, pp, at)
                                        .map_err(|e: std::string::String| e.into_value(ctx).into())
                                })?;
                                let m = match found {
                                    Some(m) if Some(m.end) != last_match => m,
                                    _ if at < src.len() => {
                                        result.push(src[at]);
                                        at += 1;
                                        if anchored {
                                            break;
                                        }
                                        continue;
                                    }
                                    _ => break,
                                };

                                // Push the match / captures onto the stack and call f
                                let call_fn = seq.try_enter(|ctx, locals, _, mut stack| {
                                    let f = locals.fetch(&f_ref);
                                    stack.drain(..);
                                    if m.captures.is_empty() {
                                        stack.push_back(ctx.intern(&src[m.start..m.end]).into());
                                    } else {
                                        for cap in &m.captures {
                                            match cap {
                                                pattern::Capture::Substring(cs, ce) => {
                                                    stack.push_back(
                                                        ctx.intern(&src[*cs..*ce]).into(),
                                                    );
                                                }
                                                pattern::Capture::Position(p) => {
                                                    stack.push_back(Value::Integer(*p as i64));
                                                }
                                            }
                                        }
                                    }
                                    let call = meta_ops::call(ctx, Value::Function(f))
                                        .map_err(|e| Error::from(e.to_string().into_value(ctx)))?;
                                    Ok(locals.stash(&ctx, call))
                                })?;

                                seq.call(&call_fn, 0).await?;

                                let ret_val = seq.enter(|ctx, locals, _, mut stack| {
                                    let v = stack.pop_front().unwrap_or(Value::Nil);
                                    locals.stash(&ctx, v)
                                });

                                let match_bytes = src[m.start..m.end].to_vec();
                                seq.enter(|_, locals, _, _| {
                                    let v = locals.fetch(&ret_val);
                                    if v.to_bool() {
                                        match v {
                                            Value::String(s) => {
                                                result.extend_from_slice(s.as_bytes())
                                            }
                                            Value::Integer(i) => {
                                                result.extend_from_slice(i.to_string().as_bytes())
                                            }
                                            Value::Number(n) => {
                                                result.extend_from_slice(
                                                    Value::Number(n)
                                                        .display()
                                                        .to_string()
                                                        .as_bytes(),
                                                );
                                            }
                                            _ => result.extend_from_slice(&match_bytes),
                                        }
                                    } else {
                                        result.extend_from_slice(&match_bytes);
                                    }
                                });

                                count += 1;
                                last_match = Some(m.end);
                                at = m.end;
                                if anchored {
                                    break;
                                }
                            }
                            result.extend_from_slice(&src[at..]);

                            seq.enter(move |ctx, _, _, mut stack| {
                                let res_str = make_string(ctx, &result);
                                stack.replace(ctx, (res_str, count));
                            });
                            Ok(SequenceReturn::Return)
                        }
                    });
                    Ok(CallbackReturn::Sequence(seq))
                }
                _ => Err("bad argument #3 to 'gsub' (string/function/table expected)"
                    .into_value(ctx)
                    .into()),
            }
        }),
    );

    string_lib.set_field(
        ctx,
        "format",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let all: Vec<Value> = stack.drain(..).collect();
            if all.is_empty() {
                return Err("bad argument #1 to 'format' (string expected)"
                    .into_value(ctx)
                    .into());
            }
            let fmt_str = match all[0] {
                Value::String(s) => s,
                _ => {
                    return Err("bad argument #1 to 'format' (string expected)"
                        .into_value(ctx)
                        .into())
                }
            };

            // Check if any %s arg needs __tostring (table/userdata)
            let s_positions = format::find_string_arg_positions(fmt_str.as_bytes());
            let needs_tostring = s_positions.iter().any(|&i| {
                if i < all.len() {
                    matches!(all[i], Value::Table(_) | Value::UserData(_))
                } else {
                    false
                }
            });

            if !needs_tostring {
                // Fast path: no metamethods needed
                match format::format_value(fmt_str.as_bytes(), &all, ctx) {
                    Ok(bytes) => {
                        let s = make_string(ctx, &bytes);
                        stack.replace(ctx, s);
                        return Ok(CallbackReturn::Return);
                    }
                    Err(e) => return Err(e.into_value(ctx).into()),
                }
            }

            // Slow path: some %s args need __tostring called via async
            use crate::stash::StashedValue;

            let fmt_bytes = fmt_str.as_bytes().to_vec(); // owned Vec<u8>, 'static

            // Stash each arg individually using the crate's stash mechanism
            let seq = async_sequence(&ctx, |locals, mut seq| {
                // Stash all args as StashedValue ('static handles)
                let mut stashed_args: Vec<StashedValue> =
                    all.iter().map(|&v| locals.stash(&ctx, v)).collect();

                async move {
                    // For each %s position that needs __tostring, call it
                    for &arg_i in &s_positions {
                        if arg_i >= stashed_args.len() {
                            continue;
                        }

                        // Check if this arg needs __tostring call
                        let call_stash_opt = seq.try_enter(|ctx, locals, _, _| {
                            let v = locals.fetch(&stashed_args[arg_i]);
                            match v {
                                Value::Table(_) | Value::UserData(_) => {
                                    match meta_ops::tostring(ctx, v) {
                                        Err(e) => Err(Error::from(e.to_string().into_value(ctx))),
                                        Ok(crate::meta_ops::MetaResult::Value(sv)) => {
                                            // Direct string: store and skip async call
                                            stashed_args[arg_i] = locals.stash(&ctx, sv);
                                            Ok(None)
                                        }
                                        Ok(crate::meta_ops::MetaResult::Call(call)) => {
                                            // Need to call __tostring: stash the function
                                            Ok(Some(locals.stash(&ctx, call.function)))
                                        }
                                    }
                                }
                                _ => Ok(None),
                            }
                        })?;

                        if let Some(call_stash) = call_stash_opt {
                            // Push the arg value onto the stack for the call
                            seq.try_enter(|_, locals, _, mut stack| {
                                stack.drain(..);
                                stack.push_back(locals.fetch(&stashed_args[arg_i]));
                                Ok(())
                            })?;
                            seq.call(&call_stash, 0).await?;

                            // Get result and update stashed arg
                            seq.try_enter(|ctx, locals, _, mut stack| {
                                let result = stack.pop_front().unwrap_or(Value::Nil);
                                if !matches!(result, Value::String(_)) {
                                    return Err("'__tostring' must return a string"
                                        .into_value(ctx)
                                        .into());
                                }
                                stashed_args[arg_i] = locals.stash(&ctx, result);
                                Ok(())
                            })?;
                        }
                    }

                    // All __tostring conversions done; run format
                    seq.try_enter(|ctx, locals, _, mut stack| {
                        let args: Vec<Value> =
                            stashed_args.iter().map(|sv| locals.fetch(sv)).collect();
                        match format::format_value(&fmt_bytes, &args, ctx) {
                            Ok(bytes) => {
                                let s = make_string(ctx, &bytes);
                                stack.replace(ctx, s);
                                Ok(SequenceReturn::Return)
                            }
                            Err(e) => Err(e.into_value(ctx).into()),
                        }
                    })
                }
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    );

    ctx.set_global("string", string_lib);

    // Set __index on the StringMetatable singleton so that s:method() works.
    let mt = ctx.singleton::<Rootable![StringMetatable<'_>]>();
    let string_lib_val: Value = ctx.globals().get_value(ctx, "string");
    mt.0.set(ctx, "__index", string_lib_val).unwrap();
}

/// Normalise a 1-based Lua index into a 0-based byte offset.
/// Negative indices are relative to end+1; 0 becomes 0.
/// Does NOT clamp to [0, len] (so callers can detect out-of-range upper values).
fn normalise_init(len: usize, init: i64) -> usize {
    if init >= 1 {
        (init - 1) as usize
    } else if init == 0 {
        0
    } else {
        let abs: usize = init.unsigned_abs().try_into().unwrap_or(0);
        len.saturating_sub(abs)
    }
}

/// Turn `string.unpack`'s 1-based `pos` argument into a 0-based offset, following PUC's
/// `posrelatI`: negative counts back from the end, and 0 or a position before the start means one.
/// `None` means the position is past the end of the data, which PUC rejects.
fn unpack_init(len: usize, pos: i64) -> Option<usize> {
    let start = if pos > 0 {
        usize::try_from(pos).unwrap_or(usize::MAX)
    } else if pos == 0 || pos.unsigned_abs() > len as u64 {
        1
    } else {
        len - pos.unsigned_abs() as usize + 1
    };
    let at = start - 1;
    (at <= len).then_some(at)
}

/// Plain (non-pattern) substring search.
pub fn find_plain(src: &[u8], pat: &[u8], init: usize) -> Option<usize> {
    if init > src.len() {
        return None;
    }
    if pat.is_empty() {
        return Some(init);
    }
    if pat.len() > src.len() - init {
        return None;
    }
    src[init..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| p + init)
}

/// sub_bytes: implement Lua string.sub semantics on raw bytes.
fn sub_bytes(string: &[u8], i: i64, j: Option<i64>) -> Result<&[u8], std::num::TryFromIntError> {
    let i = match i {
        i if i > 0 => i.saturating_sub(1).try_into()?,
        0 => 0,
        i => string.len().saturating_sub(i.unsigned_abs().try_into()?),
    };
    let j = if let Some(j) = j {
        if j >= 0 {
            j.try_into()?
        } else {
            let j: usize = j.unsigned_abs().try_into()?;
            string.len().saturating_sub(j.saturating_sub(1))
        }
    } else {
        string.len()
    }
    .clamp(0, string.len());

    Ok(if i >= j || i >= string.len() {
        &[]
    } else {
        &string[i..j]
    })
}

/// Simple string replacement for gsub with string replacement arg.
fn gsub_string(
    src: &[u8],
    pat: &[u8],
    repl: &[u8],
    max_subs: i64,
) -> Result<(Vec<u8>, i64), std::string::String> {
    let pp = pattern::prepare(pat)?;
    let anchored = is_anchored(pat);
    let mut result = Vec::new();
    let mut at = 0usize;
    let mut count = 0i64;
    let mut last_match: Option<usize> = None;

    while count < max_subs {
        match pattern::match_at(src, pat, pp, at)? {
            Some(m) if Some(m.end) != last_match => {
                count += 1;
                let replacement =
                    pattern::apply_replacement(repl, src, m.start, m.end, &m.captures)?;
                result.extend_from_slice(&replacement);
                last_match = Some(m.end);
                at = m.end;
            }
            _ if at < src.len() => {
                result.push(src[at]);
                at += 1;
            }
            _ => break,
        }
        if anchored {
            break;
        }
    }
    result.extend_from_slice(&src[at..]);
    Ok((result, count))
}

/// Does this pattern anchor to the start of the subject?
///
/// `^` means gsub substitutes at most once, at position one — not once per position. Reference Lua
/// leaves its substitution loop after a single attempt when the pattern is anchored
/// (`lstrlib.c`, `str_gsub`), and every replacement form here has to do the same or
/// `("aaa"):gsub("^a", "X")` answers `XXX 3` where Lua answers `Xaa 1`.
fn is_anchored(pat: &[u8]) -> bool {
    pat.first() == Some(&b'^')
}

/// Look a key up through `__index`, as long as every link in the chain is a table.
///
/// `gsub`'s table replacement used a raw lookup, so a replacement table backed by a class-style
/// `__index` silently found nothing. A `__index` *function* would need an `Executor` to call, which
/// this loop has no way to drive, so that case still falls through to the raw value.
fn index_through_tables<'gc>(ctx: Context<'gc>, table: Table<'gc>, key: String<'gc>) -> Value<'gc> {
    let mut current = table;
    // Bounded so that a metatable cycle cannot spin forever.
    for _ in 0..100 {
        let found = current.get_value(ctx, key);
        if !found.is_nil() {
            return found;
        }
        let Some(meta) = current.metatable() else {
            return Value::Nil;
        };
        match meta.get_value(ctx, crate::MetaMethod::Index) {
            Value::Table(next) => current = next,
            _ => return Value::Nil,
        }
    }
    Value::Nil
}
