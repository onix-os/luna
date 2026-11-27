//! `Data` as TOML, YAML and INI text, the TOML reader, and the `lib` encodings built on them.

use luna::{functional, Closure, Executor, ExternError};

use functional::Data;

fn json(text: &str) -> Data {
    Data::from_json(text).unwrap()
}

#[test]
fn toml_reads_every_kind_of_value() {
    let data = Data::from_toml(
        r#"
# comment
title = "TOML \"example\" \u00e9"
literal = 'C:\path'
multi = """
first \
    second
"""
raw = '''
keep \n this'''
quotes = """two "" inside"""
ints = [ 0, +17, -9_223_372_036_854_775_808, 0xff, 0o17, 0b101, ]
floats = [3.5, -0.01, 5e+22, 1E-2, 6.626e-34, 9_000.5]
special = [inf, -inf]
flags = [true, false]
date = 1979-05-27
datetime = 1979-05-27T07:32:00-08:00
spaced = 1979-05-27 07:32:00.999Z
time = 07:32:00
inline = { x = 1, y.z = "deep" }
"quoted key" = 1
dotted.key = "value"

[server]
host = "h"

[server.tls]
enabled = true

[[package]]
name = "a"

[[package]]
name = "b"
deps = [
  "a",  # trailing comment
]

[package.meta]
k = 1
"#,
    )
    .unwrap();
    let Some(Data::List(special)) = data.get("special") else {
        panic!("{data:?}");
    };
    assert_eq!(
        special,
        &[Data::Number(f64::INFINITY), Data::Number(f64::NEG_INFINITY)]
    );
    let mut without = data.clone();
    if let Data::Map(entries) = &mut without {
        entries.retain(|(key, _)| key.as_str() != Some("special"));
    }
    let expected = json(
        r#"{
        "title": "TOML \"example\" é", "literal": "C:\\path", "multi": "first second\n",
        "raw": "keep \\n this", "quotes": "two \"\" inside",
        "ints": [0, 17, -9223372036854775808, 255, 15, 5],
        "floats": [3.5, -0.01, 5e22, 0.01, 6.626e-34, 9000.5],
        "flags": [true, false],
        "date": "1979-05-27", "datetime": "1979-05-27T07:32:00-08:00",
        "spaced": "1979-05-27 07:32:00.999Z", "time": "07:32:00",
        "inline": {"x": 1, "y": {"z": "deep"}}, "quoted key": 1, "dotted": {"key": "value"},
        "server": {"host": "h", "tls": {"enabled": true}},
        "package": [{"name": "a"}, {"name": "b", "deps": ["a"], "meta": {"k": 1}}]
        }"#,
    );
    assert_eq!(without, expected);
}

#[test]
fn toml_errors_name_the_line() {
    for (text, expected) in [
        ("a = 1\na = 2", "line 2: the key 'a' is defined twice"),
        ("[t]\n[t]", "line 2: the table 't' is defined twice"),
        (
            "[a]\nb.c = 1\n[a.b]",
            "line 3: the table 'a.b' is defined twice",
        ),
        (
            "[a.b]\n[a]\nb.c = 1",
            "line 3: dotted keys cannot extend the table 'b'",
        ),
        (
            "a = [1]\n[[a]]",
            "line 2: 'a' is already defined as another kind of value",
        ),
        ("x = { a = 1 }\nx.b = 2", "line 2: 'x' is not a table"),
        ("n = 012", "line 1: invalid value '012'"),
        ("n = 1__0", "line 1: invalid value '1__0'"),
        ("n = -0x10", "line 1: invalid value '-0x10'"),
        ("s = \"open", "line 1: unterminated string"),
        ("s = \"\\q\"", "line 1: invalid escape sequence"),
        ("a = 1 b = 2", "line 1: expected the end of the line"),
        ("a = [1 2]", "line 1: expected ',' or ']' in an array"),
        ("a = { b = 1, }", "line 1: expected a key"),
        ("d = 1979-13-01", "line 1: invalid value '1979-13-01'"),
    ] {
        let error = Data::from_toml(text).unwrap_err().to_string();
        assert_eq!(error, expected, "{text:?}");
    }
}

fn sample() -> Data {
    json(
        r#"{
        "name": "box", "port": 8080, "ratio": 0.5, "on": true, "tags": ["a", "b"],
        "yes": "quoted", "a.b": 1,
        "server": {"host": "h", "tls": {"cert": "c"}},
        "users": [{"name": "x", "groups": ["wheel"]}, {"name": "y"}],
        "mixed": [1, {"k": "v"}], "empty": {}
        }"#,
    )
}

#[test]
fn toml_writes_tables_after_values() {
    assert_eq!(
        sample().to_toml().unwrap(),
        r#""a.b" = 1
mixed = [1, { k = "v" }]
name = "box"
on = true
port = 8080
ratio = 0.5
tags = ["a", "b"]
yes = "quoted"

[empty]

[server]
host = "h"

[server.tls]
cert = "c"

[[users]]
groups = ["wheel"]
name = "x"

[[users]]
name = "y"
"#
    );
}

#[test]
fn toml_round_trips() {
    let data = sample();
    assert_eq!(Data::from_toml(&data.to_toml().unwrap()).unwrap(), data);
}

#[test]
fn yaml_quotes_every_string_and_reserved_key() {
    assert_eq!(
        sample().to_yaml().unwrap(),
        r#""a.b": 1
empty: {}
mixed:
  - 1
  - k: "v"
name: "box"
"on": true
port: 8080
ratio: 0.5
server:
  host: "h"
  tls:
    cert: "c"
tags:
  - "a"
  - "b"
users:
  - groups:
      - "wheel"
    name: "x"
  - name: "y"
"yes": "quoted"
"#
    );
    assert_eq!(
        json("[1e300, [], [[1]]]").to_yaml().unwrap(),
        "- 1.0e+300\n- []\n-\n  -\n    - 1\n"
    );
}

#[test]
fn ini_writes_sections_of_scalars() {
    let data = json(
        r#"{"user": "me", "core": {"editor": "vim", "paths": ["/a", "/b"]}, "x": {"on": true}}"#,
    );
    assert_eq!(
        data.to_ini().unwrap(),
        "user=me\n\n[core]\neditor=vim\npaths=/a\npaths=/b\n\n[x]\non=true\n"
    );
    for (text, expected) in [
        (
            r#"{"a": {"b": {"c": 1}}}"#,
            ".a.b: INI sections cannot contain maps",
        ),
        (
            r#"{"a": "two\nlines"}"#,
            ".a: INI text cannot contain '\\n'",
        ),
        ("[1]", "INI needs a map at the top, not a list"),
    ] {
        assert_eq!(json(text).to_ini().unwrap_err().to_string(), expected);
    }
}

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = common::functional();
    let executor = lua.try_enter(|ctx| {
        let env = functional::environment(ctx);
        let closure = Closure::load_with_env(ctx, None, source.as_bytes(), env)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

#[test]
fn lib_encodes_and_parses() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local value = { b = { 1, 2 }, a = "x" }
        local parsed = lib.from_toml(lib.to_toml(value))
        local ok, message = pcall(lib.to_toml, { f = function() end })
        return lib.to_json(value) == '{"a":"x","b":[1,2]}'
            and lib.to_json({ 1 }, { pretty = true }) == "[\n  1\n]"
            and lib.equal(parsed, value)
            and not pcall(lib.from_json, "[1, null]")
            and lib.from_json('{"k": 1.5}').k == 1.5
            and lib.to_yaml({ k = "v" }) == 'k: "v"\n'
            and lib.to_ini({ s = { k = 1 } }) == "[s]\nk=1\n"
            and not ok and message:find("value.f: cannot convert a function", 1, true) ~= nil
    "#
    )?);
    Ok(())
}

mod common;
