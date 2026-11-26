//! Plain data taken out of and put into functional states, with canonical and JSON encodings.

use std::{cmp::Ordering, fmt};

use crate::{Context, Function, Table, Value};

/// A Lua value with no identity: nil, booleans, numbers, strings, lists and maps.
///
/// Map entries are kept in the canonical key order of `pairs` in a functional state, with unique
/// keys that are booleans, numbers or strings. A table whose keys are exactly `1..=n` is a list;
/// every other table, including an empty one, is a map.
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(Vec<u8>),
    List(Vec<Data>),
    Map(Vec<(Data, Data)>),
}

/// Why a value could not be converted or decoded, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for DataError {}

fn fail<T>(path: &str, message: impl Into<String>) -> Result<T, DataError> {
    Err(DataError {
        path: path.to_owned(),
        message: message.into(),
    })
}

const MAX_DEPTH: usize = 512;

impl Data {
    /// Build a map, sorting entries into canonical order. Keys must be unique scalars.
    pub fn map(mut entries: Vec<(Data, Data)>) -> Result<Data, DataError> {
        for (key, _) in &entries {
            if !key.is_key() {
                return fail("", format!("a map key cannot be {}", key.kind()));
            }
        }
        entries.sort_by(|a, b| compare_keys(&a.0, &b.0));
        if entries
            .windows(2)
            .any(|pair| compare_keys(&pair[0].0, &pair[1].0) == Ordering::Equal)
        {
            return fail("", "a map key appears twice");
        }
        Ok(Data::Map(entries))
    }

    /// The value under a string key of a map.
    pub fn get(&self, key: &str) -> Option<&Data> {
        match self {
            Data::Map(entries) => entries
                .iter()
                .find(|(k, _)| matches!(k, Data::String(k) if k == key.as_bytes()))
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Data::String(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Data::Integer(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Data]> {
        match self {
            Data::List(items) => Some(items),
            _ => None,
        }
    }

    fn is_key(&self) -> bool {
        match self {
            Data::Boolean(_) | Data::Integer(_) | Data::String(_) => true,
            Data::Number(n) => !n.is_nan(),
            _ => false,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Data::Nil => "nil",
            Data::Boolean(_) => "a boolean",
            Data::Integer(_) | Data::Number(_) => "a number",
            Data::String(_) => "a string",
            Data::List(_) => "a list",
            Data::Map(_) => "a map",
        }
    }

    /// Copy a Lua value out of a state. Functions, threads, userdata and cyclic tables are refused.
    pub fn from_value<'gc>(ctx: Context<'gc>, value: Value<'gc>) -> Result<Data, DataError> {
        let mut path = Vec::new();
        from_value(ctx, value, &mut String::new(), &mut path)
    }

    /// Build fresh Lua values for this data. A nil inside a list is refused.
    pub fn to_value<'gc>(&self, ctx: Context<'gc>) -> Result<Value<'gc>, DataError> {
        to_value(ctx, self, &mut String::new(), 0)
    }

    /// The exact, canonical byte encoding used for hashing and caching.
    ///
    /// Equal data always encodes to equal bytes, and every NaN encodes as the same quiet NaN.
    pub fn canonical(&self) -> Vec<u8> {
        let mut out = Vec::new();
        encode_canonical(self, &mut out);
        out
    }

    /// Decode [`Data::canonical`] bytes.
    pub fn from_canonical(bytes: &[u8]) -> Result<Data, DataError> {
        let mut at = 0;
        let data = decode_canonical(bytes, &mut at, 0)?;
        if at != bytes.len() {
            return fail("", "trailing bytes after canonical data");
        }
        Ok(data)
    }

    /// Canonical JSON: map keys in canonical order, no insignificant whitespace unless `pretty`.
    ///
    /// Map keys must be strings, strings must be UTF-8, and numbers must be finite. Floats keep a
    /// fractional part or exponent so they stay distinct from integers.
    pub fn to_json(&self, pretty: bool) -> Result<String, DataError> {
        let mut out = String::new();
        encode_json(self, &mut out, pretty, 0, &mut String::new())?;
        Ok(out)
    }

    /// Parse JSON. Integers that fit `i64` stay integers; objects become maps.
    pub fn from_json(text: &str) -> Result<Data, DataError> {
        let mut parser = JsonParser {
            bytes: text.as_bytes(),
            at: 0,
        };
        parser.skip_space();
        let data = parser.value(0)?;
        parser.skip_space();
        if parser.at != parser.bytes.len() {
            return parser.fail("unexpected trailing characters");
        }
        Ok(data)
    }
}

impl From<bool> for Data {
    fn from(value: bool) -> Self {
        Data::Boolean(value)
    }
}

impl From<i64> for Data {
    fn from(value: i64) -> Self {
        Data::Integer(value)
    }
}

impl From<f64> for Data {
    fn from(value: f64) -> Self {
        Data::Number(value)
    }
}

impl From<&str> for Data {
    fn from(value: &str) -> Self {
        Data::String(value.as_bytes().to_vec())
    }
}

impl From<String> for Data {
    fn from(value: String) -> Self {
        Data::String(value.into_bytes())
    }
}

impl From<Vec<Data>> for Data {
    fn from(value: Vec<Data>) -> Self {
        Data::List(value)
    }
}

/// The canonical order of map keys: booleans, then numbers by value, then strings by bytes.
pub fn compare_keys(a: &Data, b: &Data) -> Ordering {
    let rank = |key: &Data| match key {
        Data::Boolean(_) => 0,
        Data::Integer(_) | Data::Number(_) => 1,
        _ => 2,
    };
    match (a, b) {
        (Data::Boolean(a), Data::Boolean(b)) => a.cmp(b),
        (Data::Integer(a), Data::Integer(b)) => a.cmp(b),
        (Data::Number(a), Data::Number(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Data::Integer(a), Data::Number(b)) => {
            crate::constant::cmp_int_float(*a, *b).unwrap_or(Ordering::Equal)
        }
        (Data::Number(a), Data::Integer(b)) => crate::constant::cmp_int_float(*b, *a)
            .map(Ordering::reverse)
            .unwrap_or(Ordering::Equal),
        (Data::String(a), Data::String(b)) => a.cmp(b),
        (a, b) => rank(a).cmp(&rank(b)),
    }
}

fn describe_key(key: &Data) -> String {
    match key {
        Data::String(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            let plain = !text.is_empty()
                && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !text.starts_with(|c: char| c.is_ascii_digit());
            if plain {
                format!(".{text}")
            } else {
                format!("[{text:?}]")
            }
        }
        Data::Integer(i) => format!("[{i}]"),
        Data::Number(n) => format!("[{n:?}]"),
        Data::Boolean(b) => format!("[{b}]"),
        _ => String::new(),
    }
}

fn from_value<'gc>(
    ctx: Context<'gc>,
    value: Value<'gc>,
    path: &mut String,
    open: &mut Vec<Table<'gc>>,
) -> Result<Data, DataError> {
    Ok(match value {
        Value::Nil => Data::Nil,
        Value::Boolean(b) => Data::Boolean(b),
        Value::Integer(i) => Data::Integer(i),
        Value::Number(n) => Data::Number(n),
        Value::String(s) => Data::String(s.as_bytes().to_vec()),
        Value::Table(table) => {
            if open.contains(&table) {
                return fail(path, "cannot convert a cyclic table");
            }
            if open.len() >= MAX_DEPTH {
                return fail(path, "tables are nested too deeply");
            }
            open.push(table);
            let mut entries = Vec::new();
            for (key, value) in table.iter(ctx) {
                let key = match key {
                    Value::Boolean(_) | Value::Integer(_) | Value::Number(_) | Value::String(_) => {
                        from_value(ctx, key, path, open)?
                    }
                    other => {
                        return fail(path, format!("cannot convert a {} key", other.type_name()))
                    }
                };
                let length = path.len();
                path.push_str(&describe_key(&key));
                let value = from_value(ctx, value, path, open)?;
                path.truncate(length);
                entries.push((key, value));
            }
            open.pop();
            entries.sort_by(|a, b| compare_keys(&a.0, &b.0));
            let is_list = !entries.is_empty()
                && entries
                    .iter()
                    .enumerate()
                    .all(|(i, (key, _))| matches!(key, Data::Integer(k) if *k == i as i64 + 1));
            if is_list {
                Data::List(entries.into_iter().map(|(_, value)| value).collect())
            } else {
                Data::Map(entries)
            }
        }
        Value::Function(Function::Closure(_)) | Value::Function(Function::Callback(_)) => {
            return fail(path, "cannot convert a function")
        }
        other => return fail(path, format!("cannot convert a {}", other.type_name())),
    })
}

fn to_value<'gc>(
    ctx: Context<'gc>,
    data: &Data,
    path: &mut String,
    depth: usize,
) -> Result<Value<'gc>, DataError> {
    if depth >= MAX_DEPTH {
        return fail(path, "data is nested too deeply");
    }
    Ok(match data {
        Data::Nil => Value::Nil,
        Data::Boolean(b) => Value::Boolean(*b),
        Data::Integer(i) => Value::Integer(*i),
        Data::Number(n) => Value::Number(*n),
        Data::String(bytes) => Value::String(ctx.intern(bytes)),
        Data::List(items) => {
            let table = Table::new(&ctx);
            for (i, item) in items.iter().enumerate() {
                let length = path.len();
                path.push_str(&format!("[{}]", i + 1));
                if matches!(item, Data::Nil) {
                    return fail(path, "a list cannot contain nil");
                }
                let value = to_value(ctx, item, path, depth + 1)?;
                path.truncate(length);
                table
                    .set_raw(&ctx, Value::Integer(i as i64 + 1), value)
                    .unwrap();
            }
            Value::Table(table)
        }
        Data::Map(entries) => {
            let table = Table::new(&ctx);
            for (key, value) in entries {
                let length = path.len();
                path.push_str(&describe_key(key));
                let key = to_value(ctx, key, path, depth + 1)?;
                let value = to_value(ctx, value, path, depth + 1)?;
                path.truncate(length);
                table.set_raw(&ctx, key, value).unwrap();
            }
            Value::Table(table)
        }
    })
}

fn encode_canonical(data: &Data, out: &mut Vec<u8>) {
    match data {
        Data::Nil => out.push(b'n'),
        Data::Boolean(true) => out.push(b't'),
        Data::Boolean(false) => out.push(b'f'),
        Data::Integer(i) => out.extend(format!("i{i};").as_bytes()),
        Data::Number(n) => {
            let bits = if n.is_nan() {
                f64::NAN.to_bits()
            } else {
                n.to_bits()
            };
            out.extend(format!("d{bits:016x};").as_bytes());
        }
        Data::String(bytes) => {
            out.extend(format!("s{}:", bytes.len()).as_bytes());
            out.extend(bytes);
        }
        Data::List(items) => {
            out.push(b'l');
            for item in items {
                encode_canonical(item, out);
            }
            out.push(b'e');
        }
        Data::Map(entries) => {
            out.push(b'm');
            for (key, value) in entries {
                encode_canonical(key, out);
                encode_canonical(value, out);
            }
            out.push(b'e');
        }
    }
}

fn decode_canonical(bytes: &[u8], at: &mut usize, depth: usize) -> Result<Data, DataError> {
    if depth >= MAX_DEPTH {
        return fail("", "canonical data is nested too deeply");
    }
    let Some(&tag) = bytes.get(*at) else {
        return fail("", "canonical data ends early");
    };
    *at += 1;
    let mut until = |end: u8| -> Result<&[u8], DataError> {
        let start = *at;
        let length = bytes[start..]
            .iter()
            .position(|&b| b == end)
            .ok_or_else(|| DataError {
                path: String::new(),
                message: "canonical data ends early".to_owned(),
            })?;
        *at = start + length + 1;
        Ok(&bytes[start..start + length])
    };
    let text = |field: &[u8]| std::str::from_utf8(field).unwrap_or("").to_owned();
    Ok(match tag {
        b'n' => Data::Nil,
        b't' => Data::Boolean(true),
        b'f' => Data::Boolean(false),
        b'i' => match text(until(b';')?).parse() {
            Ok(i) => Data::Integer(i),
            Err(_) => return fail("", "bad canonical integer"),
        },
        b'd' => match u64::from_str_radix(&text(until(b';')?), 16) {
            Ok(bits) => Data::Number(f64::from_bits(bits)),
            Err(_) => return fail("", "bad canonical number"),
        },
        b's' => {
            let length: usize = match text(until(b':')?).parse() {
                Ok(length) => length,
                Err(_) => return fail("", "bad canonical string length"),
            };
            let Some(content) = bytes.get(*at..*at + length) else {
                return fail("", "canonical data ends early");
            };
            *at += length;
            Data::String(content.to_vec())
        }
        b'l' | b'm' => {
            let mut items = Vec::new();
            while bytes.get(*at) != Some(&b'e') {
                items.push(decode_canonical(bytes, at, depth + 1)?);
            }
            *at += 1;
            if tag == b'l' {
                Data::List(items)
            } else {
                if items.len() % 2 != 0 {
                    return fail("", "canonical map has a key without a value");
                }
                let mut entries = Vec::new();
                let mut items = items.into_iter();
                while let (Some(key), Some(value)) = (items.next(), items.next()) {
                    entries.push((key, value));
                }
                Data::map(entries)?
            }
        }
        _ => return fail("", "bad canonical tag"),
    })
}

fn encode_json(
    data: &Data,
    out: &mut String,
    pretty: bool,
    depth: usize,
    path: &mut String,
) -> Result<(), DataError> {
    let newline = |out: &mut String, depth: usize| {
        if pretty {
            out.push('\n');
            out.extend(std::iter::repeat_n("  ", depth));
        }
    };
    match data {
        Data::Nil => out.push_str("null"),
        Data::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Data::Integer(i) => out.push_str(&i.to_string()),
        Data::Number(n) => {
            if !n.is_finite() {
                return fail(path, "JSON cannot represent a NaN or infinite number");
            }
            out.push_str(&format!("{n:?}"));
        }
        Data::String(bytes) => {
            let Ok(text) = std::str::from_utf8(bytes) else {
                return fail(path, "JSON cannot represent a string that is not UTF-8");
            };
            encode_json_string(text, out);
        }
        Data::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                let length = path.len();
                path.push_str(&format!("[{}]", i + 1));
                encode_json(item, out, pretty, depth + 1, path)?;
                path.truncate(length);
            }
            if !items.is_empty() {
                newline(out, depth);
            }
            out.push(']');
        }
        Data::Map(entries) => {
            out.push('{');
            for (i, (key, value)) in entries.iter().enumerate() {
                let Data::String(bytes) = key else {
                    return fail(
                        path,
                        format!("JSON object keys must be strings, not {}", key.kind()),
                    );
                };
                let length = path.len();
                path.push_str(&describe_key(key));
                let Ok(name) = std::str::from_utf8(bytes) else {
                    return fail(path, "JSON cannot represent a key that is not UTF-8");
                };
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                encode_json_string(name, out);
                out.push(':');
                if pretty {
                    out.push(' ');
                }
                encode_json(value, out, pretty, depth + 1, path)?;
                path.truncate(length);
            }
            if !entries.is_empty() {
                newline(out, depth);
            }
            out.push('}');
        }
    }
    Ok(())
}

fn encode_json_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl JsonParser<'_> {
    fn fail<T>(&self, message: &str) -> Result<T, DataError> {
        fail(&format!("JSON at byte {}", self.at), message)
    }

    fn skip_space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &str, value: Data) -> Result<Data, DataError> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            self.fail("unexpected character")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Data, DataError> {
        if depth >= MAX_DEPTH {
            return self.fail("JSON is nested too deeply");
        }
        match self.bytes.get(self.at) {
            Some(b'n') => self.literal("null", Data::Nil),
            Some(b't') => self.literal("true", Data::Boolean(true)),
            Some(b'f') => self.literal("false", Data::Boolean(false)),
            Some(b'"') => Ok(Data::String(self.string()?.into_bytes())),
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Ok(Data::List(items));
                }
                loop {
                    self.skip_space();
                    items.push(self.value(depth + 1)?);
                    self.skip_space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Data::List(items));
                        }
                        _ => return self.fail("expected ',' or ']'"),
                    }
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut entries = Vec::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(Data::Map(entries));
                }
                loop {
                    self.skip_space();
                    if self.bytes.get(self.at) != Some(&b'"') {
                        return self.fail("expected a string key");
                    }
                    let key = self.string()?;
                    self.skip_space();
                    if self.bytes.get(self.at) != Some(&b':') {
                        return self.fail("expected ':'");
                    }
                    self.at += 1;
                    self.skip_space();
                    let value = self.value(depth + 1)?;
                    entries.push((Data::String(key.into_bytes()), value));
                    self.skip_space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Data::map(entries).or_else(|error| self.fail(&error.message));
                        }
                        _ => return self.fail("expected ',' or '}'"),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => self.fail("unexpected character"),
            None => self.fail("unexpected end of input"),
        }
    }

    fn number(&mut self) -> Result<Data, DataError> {
        let start = self.at;
        let mut float = false;
        while let Some(&b) = self.bytes.get(self.at) {
            match b {
                b'0'..=b'9' | b'-' | b'+' => {}
                b'.' | b'e' | b'E' => float = true,
                _ => break,
            }
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).unwrap();
        if !float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Data::Integer(i));
            }
        }
        match text.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(Data::Number(n)),
            _ => self.fail("invalid number"),
        }
    }

    fn string(&mut self) -> Result<String, DataError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(&b) = self.bytes.get(self.at) else {
                return self.fail("unterminated string");
            };
            match b {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let escape = self.bytes.get(self.at).copied();
                    self.at += 1;
                    match escape {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            let high = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&high) {
                                if !self.bytes[self.at..].starts_with(b"\\u") {
                                    return self.fail("unpaired surrogate");
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&low) {
                                    return self.fail("unpaired surrogate");
                                }
                                0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                            } else {
                                high
                            };
                            match char::from_u32(code) {
                                Some(c) => out.push(c),
                                None => return self.fail("invalid escape"),
                            }
                        }
                        _ => return self.fail("invalid escape"),
                    }
                }
                b if b < 0x20 => return self.fail("control character in string"),
                _ => {
                    let rest = &self.bytes[self.at..];
                    let width = match b {
                        0x00..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    match rest
                        .get(..width)
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    {
                        Some(text) => out.push_str(text),
                        None => return self.fail("invalid UTF-8"),
                    }
                    self.at += width;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, DataError> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| u32::from_str_radix(text, 16).ok());
        match digits {
            Some(value) => {
                self.at += 4;
                Ok(value)
            }
            None => self.fail("invalid \\u escape"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Data)]) -> Data {
        Data::map(
            entries
                .iter()
                .map(|(k, v)| (Data::from(*k), v.clone()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn json_round_trips_and_is_canonical() {
        let data = map(&[
            ("zeta", Data::Integer(1)),
            (
                "alpha",
                Data::List(vec![Data::Number(1.0), Data::Number(0.5)]),
            ),
            ("text", Data::from("a\"b\\c\n\u{1}é")),
            ("empty", Data::Map(Vec::new())),
            ("none", Data::Nil),
        ]);
        let json = data.to_json(false).unwrap();
        assert_eq!(
            json,
            r#"{"alpha":[1.0,0.5],"empty":{},"none":null,"text":"a\"b\\c\n\u0001é","zeta":1}"#
        );
        assert_eq!(Data::from_json(&json).unwrap(), data);
        assert_eq!(Data::from_json(&data.to_json(true).unwrap()).unwrap(), data);
        assert_eq!(
            Data::from_json(r#" { "b" : [ 1e2 , -0 , "😀" ] , "a" : true } "#).unwrap(),
            map(&[
                ("a", Data::Boolean(true)),
                (
                    "b",
                    Data::List(vec![
                        Data::Number(100.0),
                        Data::Integer(0),
                        Data::from("😀")
                    ])
                ),
            ])
        );
    }

    #[test]
    fn json_refuses_what_it_cannot_represent() {
        assert!(Data::Number(f64::NAN).to_json(false).is_err());
        assert!(Data::String(vec![0xff]).to_json(false).is_err());
        let error = Data::map(vec![(Data::Integer(2), Data::Nil)])
            .unwrap()
            .to_json(false)
            .unwrap_err();
        assert!(error.message.contains("keys must be strings"));
        for bad in [
            "",
            "{",
            "[1,]",
            r#"{"a":1,"a":2}"#,
            "01x",
            r#""\ud800""#,
            "nan",
        ] {
            assert!(Data::from_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn canonical_encoding_round_trips_exactly() {
        let data = Data::List(vec![
            Data::Nil,
            Data::Boolean(false),
            Data::Integer(i64::MIN),
            Data::Number(-0.0),
            Data::String(vec![0, 0xff, b':']),
            map(&[("k", Data::Number(1.5))]),
        ]);
        let bytes = data.canonical();
        assert_eq!(Data::from_canonical(&bytes).unwrap(), data);
        assert_eq!(
            Data::Number(-f64::NAN).canonical(),
            Data::Number(f64::NAN).canonical()
        );
        assert!(Data::from_canonical(b"l").is_err());
        assert!(Data::from_canonical(b"s9:ab").is_err());
    }
}
