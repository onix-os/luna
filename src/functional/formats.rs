//! TOML, YAML and INI renderings of [`Data`], and a TOML reader.

use super::data::{describe_key, fail, Data, DataError, MAX_DEPTH};

impl Data {
    /// TOML text with tables and arrays of tables in canonical key order.
    ///
    /// The value must be a map. Integer keys become decimal strings, strings must be UTF-8, and
    /// nothing may be nil.
    pub fn to_toml(&self) -> Result<String, DataError> {
        let Data::Map(entries) = self else {
            return fail(
                "",
                format!("TOML needs a map at the top, not {}", self.kind()),
            );
        };
        let mut out = String::new();
        toml_table(entries, &mut Vec::new(), &mut out, "")?;
        Ok(out)
    }

    /// YAML text in block style. Every string is double-quoted, so no value changes type when
    /// read back by a YAML 1.1 or 1.2 parser.
    pub fn to_yaml(&self) -> Result<String, DataError> {
        let mut out = String::new();
        match self {
            Data::Map(entries) if !entries.is_empty() => yaml_map(entries, &mut out, 0, "")?,
            Data::List(items) if !items.is_empty() => yaml_list(items, &mut out, 0, "")?,
            scalar => {
                out.push_str(&yaml_scalar(scalar, "")?);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// INI text: top-level scalars first, then one `[section]` per map of scalars.
    ///
    /// A list of scalars is written as one line per item under the same key.
    pub fn to_ini(&self) -> Result<String, DataError> {
        let Data::Map(entries) = self else {
            return fail(
                "",
                format!("INI needs a map at the top, not {}", self.kind()),
            );
        };
        let mut out = String::new();
        for (key, value) in entries {
            if !matches!(value, Data::Map(_)) {
                ini_entry(key, value, &mut out, "")?;
            }
        }
        for (key, value) in entries {
            let Data::Map(section) = value else {
                continue;
            };
            let path = describe_key(key);
            if !out.is_empty() {
                out.push('\n');
            }
            out.push('[');
            out.push_str(&ini_text(key, &path, "]")?);
            out.push_str("]\n");
            for (key, value) in section {
                if matches!(value, Data::Map(_)) {
                    return fail(
                        &format!("{path}{}", describe_key(key)),
                        "INI sections cannot contain maps",
                    );
                }
                ini_entry(key, value, &mut out, &path)?;
            }
        }
        Ok(out)
    }

    /// Parse TOML 1.0. Tables become maps and arrays lists; dates and times keep their text.
    pub fn from_toml(text: &str) -> Result<Data, DataError> {
        let mut parser = Toml {
            text,
            src: text.as_bytes(),
            at: usize::from(text.starts_with('\u{feff}')) * 3,
        };
        let mut root = TomlTable::default();
        let mut current = Vec::new();
        loop {
            parser.blank()?;
            let start = parser.at;
            if parser.at == parser.src.len() {
                break;
            }
            if parser.peek() == Some(b'[') {
                let array = parser.starts(b"[[");
                parser.at += if array { 2 } else { 1 };
                parser.space();
                let keys = parser.key()?;
                parser.space();
                let close: &[u8] = if array { b"]]" } else { b"]" };
                if !parser.starts(close) {
                    return parser.fail("expected the end of the table header");
                }
                parser.at += close.len();
                parser.end_of_line()?;
                if let Err(message) = root.header(&keys, array) {
                    parser.at = start;
                    return parser.fail(message);
                }
                current = keys;
            } else {
                let keys = parser.key()?;
                parser.space();
                if !parser.eat(b'=') {
                    return parser.fail("expected '=' after a key");
                }
                parser.space();
                let value = parser.value(0)?;
                parser.end_of_line()?;
                if let Err(message) = root.at(&current).insert(&keys, value) {
                    parser.at = start;
                    return parser.fail(message);
                }
            }
        }
        root.into_data()
    }
}

/// A string in double quotes with every control character, DEL, C1 control and BOM escaped,
/// which TOML basic strings and YAML double-quoted scalars both read back unchanged.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
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
            c if (c as u32) < 0x20 || ('\u{7f}'..='\u{9f}').contains(&c) || c == '\u{feff}' => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn utf8<'a>(bytes: &'a [u8], path: &str, format: &str) -> Result<&'a str, DataError> {
    std::str::from_utf8(bytes).or_else(|_| {
        fail(
            path,
            format!("{format} cannot represent text that is not UTF-8"),
        )
    })
}

fn toml_key(key: &Data, path: &str) -> Result<String, DataError> {
    let name = match key {
        Data::String(bytes) => utf8(bytes, path, "TOML")?.to_owned(),
        Data::Integer(i) => i.to_string(),
        other => {
            return fail(
                path,
                format!(
                    "TOML keys must be strings or integers, not {}",
                    other.kind()
                ),
            )
        }
    };
    let bare = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    Ok(if bare { name } else { quoted(&name) })
}

fn is_table_array(value: &Data) -> bool {
    matches!(value, Data::List(items) if !items.is_empty()
        && items.iter().all(|item| matches!(item, Data::Map(_))))
}

fn is_section(value: &Data) -> bool {
    matches!(value, Data::Map(_)) || is_table_array(value)
}

fn toml_header(out: &mut String, header: &[String], array: bool) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(if array { "[[" } else { "[" });
    out.push_str(&header.join("."));
    out.push_str(if array { "]]\n" } else { "]\n" });
}

fn toml_table(
    entries: &[(Data, Data)],
    header: &mut Vec<String>,
    out: &mut String,
    path: &str,
) -> Result<(), DataError> {
    for (key, value) in entries.iter().filter(|(_, value)| !is_section(value)) {
        let path = format!("{path}{}", describe_key(key));
        out.push_str(&toml_key(key, &path)?);
        out.push_str(" = ");
        toml_inline(value, out, &path)?;
        out.push('\n');
    }
    for (key, value) in entries.iter().filter(|(_, value)| is_section(value)) {
        let path = format!("{path}{}", describe_key(key));
        header.push(toml_key(key, &path)?);
        match value {
            Data::Map(children) => {
                if children.is_empty() || children.iter().any(|(_, v)| !is_section(v)) {
                    toml_header(out, header, false);
                }
                toml_table(children, header, out, &path)?;
            }
            Data::List(items) => {
                for (index, item) in items.iter().enumerate() {
                    if let Data::Map(children) = item {
                        toml_header(out, header, true);
                        toml_table(children, header, out, &format!("{path}[{}]", index + 1))?;
                    }
                }
            }
            _ => {}
        }
        header.pop();
    }
    Ok(())
}

fn toml_inline(value: &Data, out: &mut String, path: &str) -> Result<(), DataError> {
    match value {
        Data::Nil => return fail(path, "TOML cannot represent nil"),
        Data::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Data::Integer(i) => out.push_str(&i.to_string()),
        Data::Number(n) if n.is_nan() => out.push_str("nan"),
        Data::Number(n) if n.is_infinite() => out.push_str(if *n > 0.0 { "inf" } else { "-inf" }),
        Data::Number(n) => out.push_str(&format!("{n:?}")),
        Data::String(bytes) => out.push_str(&quoted(utf8(bytes, path, "TOML")?)),
        Data::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                toml_inline(item, out, &format!("{path}[{}]", index + 1))?;
            }
            out.push(']');
        }
        Data::Map(entries) => {
            out.push('{');
            for (index, (key, value)) in entries.iter().enumerate() {
                let path = format!("{path}{}", describe_key(key));
                out.push_str(if index > 0 { ", " } else { " " });
                out.push_str(&toml_key(key, &path)?);
                out.push_str(" = ");
                toml_inline(value, out, &path)?;
            }
            out.push_str(if entries.is_empty() { "}" } else { " }" });
        }
    }
    Ok(())
}

fn pad(out: &mut String, indent: usize) {
    out.extend(std::iter::repeat_n(' ', indent));
}

fn yaml_scalar(value: &Data, path: &str) -> Result<String, DataError> {
    Ok(match value {
        Data::Nil => "null".to_owned(),
        Data::Boolean(b) => b.to_string(),
        Data::Integer(i) => i.to_string(),
        Data::Number(n) if n.is_nan() => ".nan".to_owned(),
        Data::Number(n) if n.is_infinite() => if *n > 0.0 { ".inf" } else { "-.inf" }.to_owned(),
        Data::Number(n) => {
            let text = format!("{n:?}");
            match text.split_once('e') {
                Some((mantissa, exponent)) => {
                    let dot = if mantissa.contains('.') { "" } else { ".0" };
                    let sign = if exponent.starts_with('-') { "" } else { "+" };
                    format!("{mantissa}{dot}e{sign}{exponent}")
                }
                None => text,
            }
        }
        Data::String(bytes) => quoted(utf8(bytes, path, "YAML")?),
        Data::List(_) => "[]".to_owned(),
        Data::Map(_) => "{}".to_owned(),
    })
}

fn yaml_key(key: &Data, path: &str) -> Result<String, DataError> {
    let Data::String(bytes) = key else {
        return yaml_scalar(key, path);
    };
    let text = utf8(bytes, path, "YAML")?;
    let plain = text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !matches!(
            text.to_ascii_lowercase().as_str(),
            "y" | "n" | "yes" | "no" | "on" | "off" | "true" | "false" | "null"
        );
    Ok(if plain { text.to_owned() } else { quoted(text) })
}

/// The value of a map entry or list item whose key or dash has just been written.
fn yaml_value(value: &Data, out: &mut String, indent: usize, path: &str) -> Result<(), DataError> {
    match value {
        Data::Map(entries) if !entries.is_empty() => {
            out.push('\n');
            yaml_map(entries, out, indent, path)
        }
        Data::List(items) if !items.is_empty() => {
            out.push('\n');
            yaml_list(items, out, indent, path)
        }
        scalar => {
            out.push(' ');
            out.push_str(&yaml_scalar(scalar, path)?);
            out.push('\n');
            Ok(())
        }
    }
}

fn yaml_entries(
    entries: &[(Data, Data)],
    out: &mut String,
    indent: usize,
    path: &str,
    first_inline: bool,
) -> Result<(), DataError> {
    for (index, (key, value)) in entries.iter().enumerate() {
        let path = format!("{path}{}", describe_key(key));
        if index > 0 || !first_inline {
            pad(out, indent);
        }
        out.push_str(&yaml_key(key, &path)?);
        out.push(':');
        yaml_value(value, out, indent + 2, &path)?;
    }
    Ok(())
}

fn yaml_map(
    entries: &[(Data, Data)],
    out: &mut String,
    indent: usize,
    path: &str,
) -> Result<(), DataError> {
    yaml_entries(entries, out, indent, path, false)
}

fn yaml_list(items: &[Data], out: &mut String, indent: usize, path: &str) -> Result<(), DataError> {
    for (index, item) in items.iter().enumerate() {
        let path = format!("{path}[{}]", index + 1);
        pad(out, indent);
        out.push('-');
        match item {
            Data::Map(entries) if !entries.is_empty() => {
                out.push(' ');
                yaml_entries(entries, out, indent + 2, &path, true)?;
            }
            other => yaml_value(other, out, indent + 2, &path)?,
        }
    }
    Ok(())
}

fn ini_text(value: &Data, path: &str, forbidden: &str) -> Result<String, DataError> {
    let text = match value {
        Data::String(bytes) => utf8(bytes, path, "INI")?.to_owned(),
        Data::Integer(i) => i.to_string(),
        Data::Boolean(b) => b.to_string(),
        Data::Number(n) if n.is_finite() => format!("{n:?}"),
        Data::Number(_) => return fail(path, "INI cannot represent a NaN or infinite number"),
        other => return fail(path, format!("INI cannot represent {} here", other.kind())),
    };
    if let Some(c) = text
        .chars()
        .find(|&c| c == '\n' || c == '\r' || forbidden.contains(c))
    {
        return fail(path, format!("INI text cannot contain {c:?}"));
    }
    Ok(text)
}

fn ini_entry(key: &Data, value: &Data, out: &mut String, path: &str) -> Result<(), DataError> {
    let path = format!("{path}{}", describe_key(key));
    let name = ini_text(key, &path, "=[")?;
    let items = match value {
        Data::List(items) => items.as_slice(),
        scalar => std::slice::from_ref(scalar),
    };
    for (index, item) in items.iter().enumerate() {
        let path = if matches!(value, Data::List(_)) {
            format!("{path}[{}]", index + 1)
        } else {
            path.clone()
        };
        out.push_str(&name);
        out.push('=');
        out.push_str(&ini_text(item, &path, "")?);
        out.push('\n');
    }
    Ok(())
}

#[derive(Default)]
struct TomlTable {
    entries: Vec<(String, TomlNode)>,
    /// Defined by a `[table]` header.
    defined: bool,
    /// Created by a dotted key.
    dotted: bool,
}

enum TomlNode {
    Value(Data),
    Table(TomlTable),
    Tables(Vec<TomlTable>),
}

impl TomlTable {
    fn position(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|(name, _)| name == key)
    }

    /// The table at `key`, created if absent. A header walks into the last of an array of
    /// tables; a dotted key may not reopen a table defined by a header.
    fn descend(&mut self, key: &str, dotted: bool) -> Result<&mut TomlTable, String> {
        let index = match self.position(key) {
            Some(index) => index,
            None => {
                let table = TomlTable {
                    dotted,
                    ..TomlTable::default()
                };
                self.entries.push((key.to_owned(), TomlNode::Table(table)));
                self.entries.len() - 1
            }
        };
        match &mut self.entries[index].1 {
            TomlNode::Table(table) if dotted && table.defined => {
                Err(format!("dotted keys cannot extend the table '{key}'"))
            }
            TomlNode::Table(table) => Ok(table),
            TomlNode::Tables(tables) if !dotted => Ok(tables.last_mut().expect("never empty")),
            _ => Err(format!("'{key}' is not a table")),
        }
    }

    fn header(&mut self, keys: &[String], array: bool) -> Result<(), String> {
        let (last, prefix) = keys.split_last().expect("a header has a key");
        let mut table = self;
        for key in prefix {
            table = table.descend(key, false)?;
        }
        let fresh = TomlTable {
            defined: true,
            ..TomlTable::default()
        };
        let name = keys.join(".");
        match table
            .position(last)
            .map(|index| &mut table.entries[index].1)
        {
            None if array => table
                .entries
                .push((last.clone(), TomlNode::Tables(vec![fresh]))),
            None => table.entries.push((last.clone(), TomlNode::Table(fresh))),
            Some(TomlNode::Tables(tables)) if array => tables.push(fresh),
            Some(TomlNode::Table(existing)) if !array && !existing.defined && !existing.dotted => {
                existing.defined = true
            }
            Some(TomlNode::Table(_)) if !array => {
                return Err(format!("the table '{name}' is defined twice"))
            }
            Some(_) => {
                return Err(format!(
                    "'{name}' is already defined as another kind of value"
                ))
            }
        }
        Ok(())
    }

    /// The table a header path names.
    fn at(&mut self, path: &[String]) -> &mut TomlTable {
        let mut table = self;
        for key in path {
            let index = table.position(key).expect("headers create their path");
            table = match &mut table.entries[index].1 {
                TomlNode::Table(table) => table,
                TomlNode::Tables(tables) => tables.last_mut().expect("never empty"),
                TomlNode::Value(_) => unreachable!("headers only lead through tables"),
            };
        }
        table
    }

    fn insert(&mut self, keys: &[String], value: Data) -> Result<(), String> {
        let (last, prefix) = keys.split_last().expect("a key has a part");
        let mut table = self;
        for key in prefix {
            table = table.descend(key, true)?;
        }
        if table.position(last).is_some() {
            return Err(format!("the key '{}' is defined twice", keys.join(".")));
        }
        table.entries.push((last.clone(), TomlNode::Value(value)));
        Ok(())
    }

    fn into_data(self) -> Result<Data, DataError> {
        let entries = self
            .entries
            .into_iter()
            .map(|(key, node)| {
                let value = match node {
                    TomlNode::Value(value) => value,
                    TomlNode::Table(table) => table.into_data()?,
                    TomlNode::Tables(tables) => Data::List(
                        tables
                            .into_iter()
                            .map(TomlTable::into_data)
                            .collect::<Result<_, _>>()?,
                    ),
                };
                Ok((Data::from(key), value))
            })
            .collect::<Result<_, DataError>>()?;
        Data::map(entries)
    }
}

struct Toml<'a> {
    text: &'a str,
    src: &'a [u8],
    at: usize,
}

fn is_control(b: u8) -> bool {
    (b < 0x20 && b != b'\t') || b == 0x7f
}

impl Toml<'_> {
    fn fail<T>(&self, message: impl Into<String>) -> Result<T, DataError> {
        let line = self.src[..self.at.min(self.src.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        fail("", format!("line {line}: {}", message.into()))
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.at + offset).copied()
    }

    fn starts(&self, prefix: &[u8]) -> bool {
        self.src[self.at..].starts_with(prefix)
    }

    fn eat(&mut self, b: u8) -> bool {
        let found = self.peek() == Some(b);
        self.at += usize::from(found);
        found
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.at += 1;
        }
    }

    fn newline(&mut self) -> bool {
        if self.eat(b'\n') {
            return true;
        }
        let crlf = self.starts(b"\r\n");
        self.at += usize::from(crlf) * 2;
        crlf
    }

    fn comment(&mut self) -> Result<(), DataError> {
        if self.peek() != Some(b'#') {
            return Ok(());
        }
        while let Some(b) = self.peek() {
            if b == b'\n' || self.starts(b"\r\n") {
                break;
            }
            if is_control(b) {
                return self.fail("control character in a comment");
            }
            self.at += 1;
        }
        Ok(())
    }

    fn end_of_line(&mut self) -> Result<(), DataError> {
        self.space();
        self.comment()?;
        if self.at == self.src.len() || self.newline() {
            Ok(())
        } else {
            self.fail("expected the end of the line")
        }
    }

    /// Whitespace, comments and newlines.
    fn blank(&mut self) -> Result<(), DataError> {
        loop {
            self.space();
            self.comment()?;
            if !self.newline() {
                return Ok(());
            }
        }
    }

    fn key(&mut self) -> Result<Vec<String>, DataError> {
        let mut keys = vec![self.simple_key()?];
        loop {
            let start = self.at;
            self.space();
            if !self.eat(b'.') {
                self.at = start;
                return Ok(keys);
            }
            self.space();
            keys.push(self.simple_key()?);
        }
    }

    fn simple_key(&mut self) -> Result<String, DataError> {
        match self.peek() {
            Some(b'"') if self.starts(b"\"\"\"") => {
                self.fail("a key cannot be a multi-line string")
            }
            Some(b'"') => self.basic_string(),
            Some(b'\'') if self.starts(b"'''") => self.fail("a key cannot be a multi-line string"),
            Some(b'\'') => self.literal_string(),
            _ => {
                let start = self.at;
                while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    self.at += 1;
                }
                if start == self.at {
                    return self.fail("expected a key");
                }
                Ok(self.text[start..self.at].to_owned())
            }
        }
    }

    fn char_into(&mut self, out: &mut String) {
        let c = self.text[self.at..].chars().next().expect("not at the end");
        out.push(c);
        self.at += c.len_utf8();
    }

    fn escape(&mut self, out: &mut String) -> Result<(), DataError> {
        self.at += 1;
        let c = match self.peek() {
            Some(b'b') => '\u{8}',
            Some(b't') => '\t',
            Some(b'n') => '\n',
            Some(b'f') => '\u{c}',
            Some(b'r') => '\r',
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'u') => return self.unicode(4, out),
            Some(b'U') => return self.unicode(8, out),
            _ => return self.fail("invalid escape sequence"),
        };
        self.at += 1;
        out.push(c);
        Ok(())
    }

    fn unicode(&mut self, digits: usize, out: &mut String) -> Result<(), DataError> {
        self.at += 1;
        let hex = self
            .src
            .get(self.at..self.at + digits)
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit));
        let Some(c) = hex
            .and_then(|hex| u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok())
            .and_then(char::from_u32)
        else {
            return self.fail("invalid Unicode escape");
        };
        self.at += digits;
        out.push(c);
        Ok(())
    }

    fn basic_string(&mut self) -> Result<String, DataError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None | Some(b'\n' | b'\r') => return self.fail("unterminated string"),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => self.escape(&mut out)?,
                Some(b) if is_control(b) => return self.fail("control character in a string"),
                Some(_) => self.char_into(&mut out),
            }
        }
    }

    fn literal_string(&mut self) -> Result<String, DataError> {
        self.at += 1;
        let start = self.at;
        loop {
            match self.peek() {
                None | Some(b'\n' | b'\r') => return self.fail("unterminated string"),
                Some(b'\'') => {
                    self.at += 1;
                    return Ok(self.text[start..self.at - 1].to_owned());
                }
                Some(b) if is_control(b) => return self.fail("control character in a string"),
                Some(_) => self.at += 1,
            }
        }
    }

    /// The closing delimiter of a multi-line string, which may be preceded by up to two quotes
    /// that belong to the content.
    fn closing(&mut self, quote: u8, out: &mut String) -> Result<bool, DataError> {
        let mut quotes = 0;
        while self.peek_at(quotes) == Some(quote) {
            quotes += 1;
        }
        if quotes < 3 {
            return Ok(false);
        }
        if quotes > 5 {
            return self.fail("too many quotes at the end of a multi-line string");
        }
        out.extend(std::iter::repeat_n(char::from(quote), quotes - 3));
        self.at += quotes;
        Ok(true)
    }

    fn multiline(&mut self, quote: u8) -> Result<String, DataError> {
        self.at += 3;
        self.newline();
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.fail("unterminated multi-line string"),
                Some(b) if b == quote && self.closing(quote, &mut out)? => return Ok(out),
                Some(b'\\') if quote == b'"' => {
                    let start = self.at;
                    self.at += 1;
                    self.space();
                    if self.newline() {
                        loop {
                            self.space();
                            if !self.newline() {
                                break;
                            }
                        }
                    } else {
                        self.at = start;
                        self.escape(&mut out)?;
                    }
                }
                Some(b'\n') => {
                    out.push('\n');
                    self.at += 1;
                }
                Some(b'\r') if self.peek_at(1) == Some(b'\n') => {
                    out.push('\n');
                    self.at += 2;
                }
                Some(b) if is_control(b) => return self.fail("control character in a string"),
                Some(_) => self.char_into(&mut out),
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Data, DataError> {
        if depth > MAX_DEPTH {
            return self.fail("values are nested too deeply");
        }
        match self.peek() {
            Some(b'"') if self.starts(b"\"\"\"") => Ok(Data::from(self.multiline(b'"')?)),
            Some(b'"') => Ok(Data::from(self.basic_string()?)),
            Some(b'\'') if self.starts(b"'''") => Ok(Data::from(self.multiline(b'\'')?)),
            Some(b'\'') => Ok(Data::from(self.literal_string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.inline_table(depth),
            _ => self.scalar(),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Data, DataError> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.blank()?;
            if self.eat(b']') {
                return Ok(Data::List(items));
            }
            items.push(self.value(depth + 1)?);
            self.blank()?;
            if self.eat(b']') {
                return Ok(Data::List(items));
            }
            if !self.eat(b',') {
                return self.fail("expected ',' or ']' in an array");
            }
        }
    }

    fn inline_table(&mut self, depth: usize) -> Result<Data, DataError> {
        self.at += 1;
        self.space();
        let mut table = TomlTable::default();
        if !self.eat(b'}') {
            loop {
                self.space();
                let start = self.at;
                let keys = self.key()?;
                self.space();
                if !self.eat(b'=') {
                    return self.fail("expected '=' after a key");
                }
                self.space();
                let value = self.value(depth + 1)?;
                if let Err(message) = table.insert(&keys, value) {
                    self.at = start;
                    return self.fail(message);
                }
                self.space();
                if self.eat(b'}') {
                    break;
                }
                if !self.eat(b',') {
                    return self.fail("expected ',' or '}' in an inline table");
                }
            }
        }
        table.into_data()
    }

    fn scalar(&mut self) -> Result<Data, DataError> {
        let start = self.at;
        let token =
            |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'.' | b':');
        while self.peek().is_some_and(token) {
            self.at += 1;
        }
        if is_date(&self.src[start..self.at])
            && self.peek() == Some(b' ')
            && self.src.get(self.at + 1..self.at + 4).is_some_and(|time| {
                time[0].is_ascii_digit() && time[1].is_ascii_digit() && time[2] == b':'
            })
        {
            self.at += 1;
            while self.peek().is_some_and(token) {
                self.at += 1;
            }
        }
        let text = &self.text[start..self.at];
        match text {
            "true" => return Ok(Data::Boolean(true)),
            "false" => return Ok(Data::Boolean(false)),
            "" => return self.fail("expected a value"),
            _ => {}
        }
        if is_datetime(text) {
            return Ok(Data::from(text));
        }
        match number(text) {
            Some(number) => Ok(number),
            None => {
                self.at = start;
                self.fail(format!("invalid value '{text}'"))
            }
        }
    }
}

fn digits(bytes: &[u8], range: std::ops::Range<usize>) -> Option<u32> {
    let part = bytes.get(range)?;
    part.iter()
        .all(u8::is_ascii_digit)
        .then(|| std::str::from_utf8(part).ok()?.parse().ok())
        .flatten()
}

fn is_date(bytes: &[u8]) -> bool {
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && digits(bytes, 0..4).is_some()
        && digits(bytes, 5..7).is_some_and(|month| (1..=12).contains(&month))
        && digits(bytes, 8..10).is_some_and(|day| (1..=31).contains(&day))
}

/// `HH:MM:SS` with an optional fraction, then optionally `Z` or a `±HH:MM` offset.
fn is_time(bytes: &[u8], offset: bool) -> bool {
    if bytes.len() < 8
        || bytes[2] != b':'
        || bytes[5] != b':'
        || !digits(bytes, 0..2).is_some_and(|hour| hour < 24)
        || !digits(bytes, 3..5).is_some_and(|minute| minute < 60)
        || !digits(bytes, 6..8).is_some_and(|second| second <= 60)
    {
        return false;
    }
    let mut rest = &bytes[8..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let count = fraction.iter().take_while(|b| b.is_ascii_digit()).count();
        if count == 0 {
            return false;
        }
        rest = &fraction[count..];
    }
    match rest {
        [] => true,
        [b'Z' | b'z'] => offset,
        [b'+' | b'-', zone @ ..] => {
            offset
                && zone.len() == 5
                && zone[2] == b':'
                && digits(zone, 0..2).is_some_and(|hour| hour < 24)
                && digits(zone, 3..5).is_some_and(|minute| minute < 60)
        }
        _ => false,
    }
}

fn is_datetime(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() >= 10 && is_date(&bytes[..10]) {
        return bytes.len() == 10
            || (matches!(bytes[10], b'T' | b't' | b' ') && is_time(&bytes[11..], true));
    }
    is_time(bytes, false)
}

/// Digits separated by single underscores, with the underscores removed.
fn underscored(text: &str, radix: u32) -> Option<String> {
    let valid = !text.is_empty()
        && !text.starts_with('_')
        && !text.ends_with('_')
        && !text.contains("__")
        && text.chars().all(|c| c == '_' || c.is_digit(radix));
    valid.then(|| text.replace('_', ""))
}

fn number(text: &str) -> Option<Data> {
    let (negative, body) = match text.as_bytes().first()? {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };
    match body {
        "inf" => {
            return Some(Data::Number(if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }))
        }
        "nan" => return Some(Data::Number(f64::NAN)),
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = body.strip_prefix(prefix) {
            if body.len() != text.len() {
                return None;
            }
            return i64::from_str_radix(&underscored(rest, radix)?, radix)
                .ok()
                .map(Data::Integer);
        }
    }
    let sign = if negative { "-" } else { "" };
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let whole = underscored(whole, 10)?;
    if whole.len() > 1 && whole.starts_with('0') {
        return None;
    }
    if fraction.is_none() && exponent.is_none() {
        return format!("{sign}{whole}").parse().ok().map(Data::Integer);
    }
    let mut float = format!("{sign}{whole}");
    if let Some(fraction) = fraction {
        float.push('.');
        float.push_str(&underscored(fraction, 10)?);
    }
    if let Some(exponent) = exponent {
        let (exponent_sign, exponent) = match exponent.as_bytes().first() {
            Some(b'+') => ("", &exponent[1..]),
            Some(b'-') => ("-", &exponent[1..]),
            _ => ("", exponent),
        };
        float.push('e');
        float.push_str(exponent_sign);
        float.push_str(&underscored(exponent, 10)?);
    }
    float.parse().ok().map(Data::Number)
}
