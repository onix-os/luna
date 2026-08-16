use crate::{Callback, CallbackReturn, Context, IntoValue, String, Table, Value, Variadic};

/// Decode one UTF-8 sequence at `i`, returning the code point and its length in bytes.
///
/// Rejects the things Lua's own validator rejects: truncated sequences, bad continuation bytes,
/// overlong encodings and anything past the last code point.
fn decode(bytes: &[u8], i: usize) -> Option<(u32, usize)> {
    let first = *bytes.get(i)?;
    let len = match first {
        0x00..=0x7f => return Some((first as u32, 1)),
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    if i + len > bytes.len() {
        return None;
    }

    let mut cp = (first as u32) & (0x7f >> len);
    for k in 1..len {
        let b = bytes[i + k];
        if b & 0xc0 != 0x80 {
            return None;
        }
        cp = (cp << 6) | (b as u32 & 0x3f);
    }

    let minimum = [0, 0, 0x80, 0x800, 0x10000][len];
    if cp < minimum || cp > 0x10_ffff {
        return None;
    }
    Some((cp, len))
}

/// Lua's `i`/`j` convention: 1-based, negative counts from the end.
fn absolute(index: i64, len: usize) -> i64 {
    if index >= 0 {
        index
    } else {
        len as i64 + index + 1
    }
}

pub fn load_utf8<'gc>(ctx: Context<'gc>) {
    let utf8 = Table::new(&ctx);

    // The pattern that matches exactly one UTF-8 sequence.
    utf8.set_field(
        ctx,
        "charpattern",
        ctx.intern(b"[\0-\x7F\xC2-\xFD][\x80-\xBF]*"),
    );

    utf8.set_field(
        ctx,
        "char",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let mut out = std::string::String::new();
            for i in 0..stack.len() {
                let cp = stack
                    .get(i)
                    .to_integer()
                    .and_then(|n| u32::try_from(n).ok())
                    .and_then(char::from_u32)
                    .ok_or_else(|| "value out of range for 'utf8.char'".into_value(ctx))?;
                out.push(cp);
            }
            stack.replace(ctx, ctx.intern(out.as_bytes()));
            Ok(CallbackReturn::Return)
        }),
    );

    utf8.set_field(
        ctx,
        "codepoint",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, i, j): (String, Option<i64>, Option<i64>) = stack.consume(ctx)?;
            let bytes = s.as_bytes();
            let i = absolute(i.unwrap_or(1), bytes.len()).max(1) as usize;
            let j = absolute(j.unwrap_or(i as i64), bytes.len()).min(bytes.len() as i64);

            let mut out = Vec::new();
            let mut pos = i - 1;
            while (pos as i64) < j {
                let (cp, len) =
                    decode(bytes, pos).ok_or_else(|| "invalid UTF-8 code".into_value(ctx))?;
                out.push(Value::Integer(cp as i64));
                pos += len;
            }
            stack.replace(ctx, Variadic(out));
            Ok(CallbackReturn::Return)
        }),
    );

    utf8.set_field(
        ctx,
        "len",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, i, j): (String, Option<i64>, Option<i64>) = stack.consume(ctx)?;
            let bytes = s.as_bytes();
            let i = absolute(i.unwrap_or(1), bytes.len()).max(1) as usize;
            let j = absolute(j.unwrap_or(-1), bytes.len());

            let mut count = 0i64;
            let mut pos = i - 1;
            while (pos as i64) <= j - 1 && pos < bytes.len() {
                match decode(bytes, pos) {
                    Some((_, len)) => {
                        count += 1;
                        pos += len;
                    }
                    // Lua reports the byte position of the first bad sequence.
                    None => {
                        stack.replace(ctx, (Value::Nil, pos as i64 + 1));
                        return Ok(CallbackReturn::Return);
                    }
                }
            }
            stack.replace(ctx, count);
            Ok(CallbackReturn::Return)
        }),
    );

    utf8.set_field(
        ctx,
        "offset",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (s, n, i): (String, i64, Option<i64>) = stack.consume(ctx)?;
            let bytes = s.as_bytes();
            let default_i = if n >= 0 { 1 } else { bytes.len() as i64 + 1 };
            let mut pos = absolute(i.unwrap_or(default_i), bytes.len()) - 1;

            let is_continuation =
                |p: i64| p >= 0 && (p as usize) < bytes.len() && bytes[p as usize] & 0xc0 == 0x80;

            let mut n = n;
            if n == 0 {
                while is_continuation(pos) {
                    pos -= 1;
                }
                stack.replace(ctx, pos + 1);
                return Ok(CallbackReturn::Return);
            }

            if n > 0 {
                n -= 1;
                while n > 0 && pos < bytes.len() as i64 {
                    pos += 1;
                    while is_continuation(pos) {
                        pos += 1;
                    }
                    n -= 1;
                }
                if n > 0 {
                    stack.replace(ctx, Value::Nil);
                } else {
                    stack.replace(ctx, pos + 1);
                }
            } else {
                while n < 0 && pos > 0 {
                    pos -= 1;
                    while is_continuation(pos) {
                        pos -= 1;
                    }
                    n += 1;
                }
                if n < 0 {
                    stack.replace(ctx, Value::Nil);
                } else {
                    stack.replace(ctx, pos + 1);
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    // `for p, c in utf8.codes(s)`: a stateless iterator, so it returns the triple Lua's generic
    // `for` expects rather than a closure.
    utf8.set_field(
        ctx,
        "codes",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let s: String = stack.consume(ctx)?;
            let iterator = Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let (s, prev): (String, i64) = stack.consume(ctx)?;
                let bytes = s.as_bytes();

                // Step over the sequence starting at `prev`, then decode the next one.
                let mut pos = prev as usize;
                if pos > 0 {
                    match decode(bytes, pos - 1) {
                        Some((_, len)) => pos = pos - 1 + len,
                        None => return Err("invalid UTF-8 code".into_value(ctx).into()),
                    }
                }
                if pos >= bytes.len() {
                    stack.replace(ctx, Value::Nil);
                    return Ok(CallbackReturn::Return);
                }
                let (cp, _) =
                    decode(bytes, pos).ok_or_else(|| "invalid UTF-8 code".into_value(ctx))?;
                stack.replace(ctx, (pos as i64 + 1, cp as i64));
                Ok(CallbackReturn::Return)
            });
            stack.replace(ctx, (iterator, s, 0));
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global("utf8", utf8);
}
