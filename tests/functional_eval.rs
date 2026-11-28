//! `functional::evaluate` and the `luna eval` command: imports, inputs, recorded APIs, limits,
//! schemas, import traces, content hashes and the cache.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use luna::functional::{evaluate, Api, Data, EvalError, Evaluated, Options};

/// A scratch directory of files, removed when dropped.
struct Project(PathBuf);

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "luna-functional-eval-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let project = Project(root.canonicalize().unwrap());
        for (name, text) in files {
            project.write(name, text);
        }
        project
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn eval(&self, name: &str, options: &Options) -> Result<Evaluated, EvalError> {
        evaluate(self.path(name), options)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn json(data: &Data) -> String {
    data.to_json(false).unwrap()
}

fn inputs(text: &str) -> Options {
    Options {
        inputs: Data::from_json(text).unwrap(),
        ..Options::default()
    }
}

#[test]
fn a_config_is_evaluated_to_frozen_data() {
    let project = Project::new(&[
        ("lib/ports.lua", "return { http = 80, https = 443 }"),
        (
            "config.lua",
            r#"
            local ports = import("lib/ports.lua")
            return function(inputs)
                return {
                    name = inputs.name,
                    ports = lib.values(ports),
                    debug = inputs.debug or false,
                    tags = lib.map({ "a", "b" }, string.upper),
                }
            end
            "#,
        ),
        ("plain.lua", "return { seen = inputs.name, sum = 1 + 2 }"),
    ]);
    let evaluated = project
        .eval("config.lua", &inputs(r#"{"name":"demo"}"#))
        .unwrap();
    assert_eq!(
        json(&evaluated.value),
        r#"{"debug":false,"name":"demo","ports":[80,443],"tags":["A","B"]}"#
    );
    assert!(!evaluated.cached);
    assert_eq!(
        evaluated.files.keys().collect::<Vec<_>>(),
        ["config.lua", "lib/ports.lua"]
    );
    let plain = project
        .eval("plain.lua", &inputs(r#"{"name":"x"}"#))
        .unwrap();
    assert_eq!(json(&plain.value), r#"{"seen":"x","sum":3}"#);
}

#[test]
fn registration_calls_are_recorded_not_performed() {
    let project = Project::new(&[
        ("env.lua", "oslo.env.set('A', import('more.lua').value) return true"),
        (
            "more.lua",
            "oslo.env.unset('B') oslo.path.add(oslo.sys.pwd() .. '/bin') return { value = oslo.version }",
        ),
        ("peek.lua", "return pcall(function() return oslo.env.set.log end)"),
    ]);
    let api = Api::new()
        .record("env.set")
        .record("env.unset")
        .record("path.add")
        .returning("sys.pwd", "/home/me")
        .value("version", "1.2");
    let options = Options {
        api: Some(("oslo".to_owned(), api)),
        ..Options::default()
    };
    let evaluated = project.eval("env.lua", &options).unwrap();
    assert_eq!(
        json(&evaluated.declarations),
        r#"[{"args":["B"],"name":"env.unset"},{"args":["/home/me/bin"],"name":"path.add"},{"args":["A","1.2"],"name":"env.set"}]"#
    );
    assert_eq!(json(&evaluated.value), "true");
    assert!(matches!(
        project.eval("peek.lua", &options).unwrap().value,
        Data::Boolean(false)
    ));
}

#[test]
fn hashes_identify_sources_inputs_and_output() {
    let project = Project::new(&[
        ("a.lua", "return { n = import('b.lua') + inputs.k }"),
        ("b.lua", "return 1"),
    ]);
    let first = project.eval("a.lua", &inputs(r#"{"k":1}"#)).unwrap();
    let again = project.eval("a.lua", &inputs(r#"{"k":1}"#)).unwrap();
    assert_eq!(first, again);
    assert_eq!(first.input_hash.len(), 64);

    let other_input = project.eval("a.lua", &inputs(r#"{"k":2}"#)).unwrap();
    assert_ne!(other_input.input_hash, first.input_hash);
    assert_ne!(other_input.output_hash, first.output_hash);

    project.write("b.lua", "return 2");
    let edited = project.eval("a.lua", &inputs(r#"{"k":0}"#)).unwrap();
    assert_ne!(edited.input_hash, first.input_hash);
    assert_eq!(edited.output_hash, first.output_hash);
}

#[test]
fn the_cache_skips_unchanged_evaluations_and_notices_edits() {
    let project = Project::new(&[
        ("a.lua", "return { n = import('lib/b.lua') }"),
        ("lib/b.lua", "return 1"),
    ]);
    let options = Options {
        cache: Some(project.path("cache")),
        ..Options::default()
    };
    let first = project.eval("a.lua", &options).unwrap();
    let hit = project.eval("a.lua", &options).unwrap();
    assert!(!first.cached && hit.cached);
    assert_eq!(
        Evaluated {
            cached: false,
            ..hit
        },
        first
    );

    project.write("lib/b.lua", "return 2");
    let miss = project.eval("a.lua", &options).unwrap();
    assert!(!miss.cached);
    assert_eq!(json(&miss.value), r#"{"n":2}"#);

    for entry in fs::read_dir(project.path("cache")).unwrap() {
        fs::write(entry.unwrap().path(), b"not canonical data").unwrap();
    }
    let recovered = project.eval("a.lua", &options).unwrap();
    assert!(!recovered.cached);
    assert_eq!(json(&recovered.value), r#"{"n":2}"#);
}

#[test]
fn instruction_and_memory_limits_stop_runaway_configs() {
    let project = Project::new(&[
        ("loop.lua", "while true do end"),
        (
            "grow.lua",
            "local t = {} for i = 1, 1e9 do t[i] = { i } end return t",
        ),
    ]);
    let fuel = project.eval(
        "loop.lua",
        &Options {
            fuel: 100_000,
            ..Options::default()
        },
    );
    assert_eq!(fuel, Err(EvalError::Fuel(100_000)));
    let memory = project.eval(
        "grow.lua",
        &Options {
            memory: Some(8 << 20),
            ..Options::default()
        },
    );
    assert!(matches!(memory, Err(EvalError::Memory(limit)) if limit > 8 << 20));
}

#[test]
fn imports_cannot_leave_the_root() {
    let project = Project::new(&[
        ("inner/config.lua", "return import(inputs.name)"),
        ("inner/ok.lua", "return 'ok'"),
        ("secret.lua", "return 'secret'"),
    ]);
    std::os::unix::fs::symlink(project.path("secret.lua"), project.path("inner/link.lua")).unwrap();
    let ok = project
        .eval("inner/config.lua", &inputs(r#"{"name":"ok.lua"}"#))
        .unwrap();
    assert_eq!(json(&ok.value), r#""ok""#);
    for name in [
        "../secret.lua",
        "/etc/hostname",
        "./ok.lua",
        "link.lua",
        "missing.lua",
    ] {
        let error = project
            .eval(
                "inner/config.lua",
                &inputs(&format!(r#"{{"name":"{name}"}}"#)),
            )
            .unwrap_err();
        assert!(
            matches!(&error, EvalError::Lua(message) if message.contains("cannot import")),
            "{name}: {error}"
        );
    }
    let wider = Options {
        root: Some(project.0.clone()),
        ..inputs(r#"{"name":"secret.lua"}"#)
    };
    assert_eq!(
        json(&project.eval("inner/config.lua", &wider).unwrap().value),
        r#""secret""#
    );
}

#[test]
fn errors_carry_an_import_trace() {
    let project = Project::new(&[
        ("a.lua", "local x = 1\nlocal b = import('b.lua')\nreturn b"),
        ("b.lua", "\n\nlocal c = import('c.lua')\nreturn c"),
        ("c.lua", "error('broken config')"),
        ("tail.lua", "return import('tail_b.lua')"),
        ("tail_b.lua", "return import('c.lua')"),
    ]);
    let message = project
        .eval("a.lua", &Options::default())
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "c.lua:1: broken config\n  in 'c.lua', imported from b.lua:3\n  in 'b.lua', imported from a.lua:2"
    );
    let message = project
        .eval("tail.lua", &Options::default())
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "c.lua:1: broken config\n  in 'c.lua', imported from tail_b.lua (in a tail call)\n  in 'tail_b.lua', imported from the root chunk (in a tail call)"
    );
}

#[test]
fn a_schema_checks_and_normalises_the_result() {
    let project = Project::new(&[
        (
            "schema.lua",
            r#"
            local t = lib.types
            return t.record({
                name = t.string,
                port = t.default(t.integer, 8080),
                mode = t.enum({ "dev", "prod" }),
                hosts = t.list(t.string),
                limits = t.optional(t.map(t.number)),
            })
            "#,
        ),
        (
            "good.lua",
            "return { name = 'api', mode = 'prod', hosts = { 'a', 'b' } }",
        ),
        (
            "bad.lua",
            "return { name = 'api', mode = 'prod', hosts = { 'a', 3 } }",
        ),
        (
            "extra.lua",
            "return { name = 'api', mode = 'dev', hosts = {}, colour = 'red' }",
        ),
        ("missing.lua", "return { mode = 'dev', hosts = {} }"),
    ]);
    let options = Options {
        schema: Some(project.path("schema.lua")),
        ..Options::default()
    };
    let good = project.eval("good.lua", &options).unwrap();
    assert_eq!(
        json(&good.value),
        r#"{"hosts":["a","b"],"mode":"prod","name":"api","port":8080}"#
    );
    assert!(good.files.keys().any(|name| name.starts_with("schema:")));
    for (file, expected) in [
        (
            "bad.lua",
            "config.hosts[2]: expected a string, got number 3",
        ),
        ("extra.lua", "config.colour: unknown field"),
        ("missing.lua", "config.name: missing required string"),
    ] {
        let error = project.eval(file, &options).unwrap_err().to_string();
        assert!(error.contains(expected), "{file}: {error}");
    }
}

#[test]
fn results_that_are_not_data_are_refused_with_a_path() {
    let project = Project::new(&[("f.lua", "return { nested = { action = function() end } }")]);
    let error = project.eval("f.lua", &Options::default()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "value.nested.action: cannot convert a function"
    );
}

fn luna(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_luna"))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn the_command_prints_canonical_json() {
    let project = Project::new(&[
        (
            "c.lua",
            "oslo.env.set('K', inputs.v) return { b = inputs.n, a = { 1, 2.5 }, s = oslo.sys.pwd() }",
        ),
        ("inputs.json", r#"{"n": 7}"#),
        ("broken.lua", "return 1 +"),
    ]);
    let file = project.path("c.lua");
    let inputs_file = project.path("inputs.json");
    let common = [
        "eval",
        arg(&file),
        "--inputs",
        arg(&inputs_file),
        "--input-string",
        "v=text",
        "--api",
        "oslo",
        "--record",
        "env.set",
        "--returning",
        "sys.pwd=\"/w\"",
    ];
    let (code, out, _) = luna(&common);
    assert_eq!(
        (code, out.as_str()),
        (0, "{\"a\":[1,2.5],\"b\":7,\"s\":\"/w\"}\n")
    );

    let mut with_declarations = common.to_vec();
    with_declarations.push("--declarations");
    let (code, out, _) = luna(&with_declarations);
    assert_eq!(code, 0);
    assert!(out.contains(r#""declarations":[{"args":["K","text"],"name":"env.set"}]"#));

    let cache = project.path("cache");
    let mut hashed = common.to_vec();
    hashed.extend(["--hash", "--cache", arg(&cache)]);
    let (_, first, _) = luna(&hashed);
    let (_, second, _) = luna(&hashed);
    assert!(first.contains(r#""cached":false"#) && second.contains(r#""cached":true"#));
    assert_eq!(
        first.replace("false", "true"),
        second,
        "hashes are stable across processes"
    );

    let (code, _, err) = luna(&["eval", arg(&project.path("broken.lua"))]);
    assert_eq!(code, 1);
    assert!(err.starts_with("luna: "), "{err}");
    let (code, _, err) = luna(&["eval"]);
    assert_eq!(code, 2);
    assert!(err.contains("usage: luna eval FILE"));
    let (code, _, _) = luna(&["eval", arg(&file), "--bogus"]);
    assert_eq!(code, 2);
}

#[test]
fn data_files_import_as_frozen_data_and_key_the_cache() {
    let project = Project::new(&[
        (
            "a.lua",
            r#"
            local pkgs, lock = import("pkgs.toml"), import("lock.json")
            local ok = pcall(function() pkgs.extra = 1 end)
            return { version = pkgs.tool.version, pinned = lock.pins[1], frozen = not ok }
            "#,
        ),
        ("pkgs.toml", "[tool]\nversion = \"1.2\"\n"),
        ("lock.json", r#"{"pins": ["abc"]}"#),
        ("bad.lua", "return import('broken.toml')"),
        ("broken.toml", "x = = 1"),
    ]);
    let options = Options {
        cache: Some(project.path("cache")),
        ..Options::default()
    };
    let first = project.eval("a.lua", &options).unwrap();
    assert_eq!(
        json(&first.value),
        r#"{"frozen":true,"pinned":"abc","version":"1.2"}"#
    );
    assert!(first.files.contains_key("pkgs.toml") && first.files.contains_key("lock.json"));
    assert!(project.eval("a.lua", &options).unwrap().cached);
    project.write("pkgs.toml", "[tool]\nversion = \"1.3\"\n");
    let edited = project.eval("a.lua", &options).unwrap();
    assert!(!edited.cached);
    assert!(json(&edited.value).contains(r#""version":"1.3""#));

    let error = project.eval("bad.lua", &Options::default()).unwrap_err();
    assert!(
        error.to_string().contains("cannot import 'broken.toml'")
            && error.to_string().contains("line 1: expected a value"),
        "{error}"
    );
}

#[test]
fn the_command_prints_toml_yaml_ini_raw_text_and_files() {
    let project = Project::new(&[
        (
            "c.lua",
            "return { name = 'box', ports = { 80 }, tls = { on = true } }",
        ),
        ("text.lua", "return lib.to_ini({ k = 'v' })"),
    ]);
    let file = project.path("c.lua");
    let run = |extra: &[&str]| {
        let mut args = vec!["eval", arg(&file)];
        args.extend(extra);
        luna(&args)
    };
    assert_eq!(
        run(&["--format", "toml"]),
        (
            0,
            "name = \"box\"\nports = [80]\n\n[tls]\non = true\n".to_owned(),
            String::new()
        )
    );
    assert_eq!(
        run(&["--format", "yaml"]).1,
        "name: \"box\"\nports:\n  - 80\ntls:\n  \"on\": true\n"
    );
    assert_eq!(
        run(&["--format", "ini"]).1,
        "name=box\nports=80\n\n[tls]\non=true\n"
    );
    let (code, _, err) = run(&["--format", "xml"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("--format expects json, toml, yaml or ini"),
        "{err}"
    );
    let (code, _, err) = run(&["--raw"]);
    assert_eq!(code, 1);
    assert!(err.contains("--raw needs the configuration to return a string"));
    let (code, out, _) = run(&["--files"]);
    assert_eq!(code, 0);
    assert!(out.starts_with(r#"{"c.lua":""#), "{out}");

    let text = project.path("text.lua");
    assert_eq!(luna(&["eval", arg(&text), "--raw"]).1, "k=v\n");
}

#[test]
fn files_and_directories_are_read_inside_the_root_and_key_the_cache() {
    let project = Project::new(&[
        (
            "a.lua",
            r#"
            local modules = {}
            for name, kind in pairs(read_dir("modules")) do
                if kind == "file" then modules[#modules + 1] = import("modules/" .. name) end
            end
            return { motd = read_file("motd.txt"), modules = modules, root = read_dir(".")["a.lua"] }
            "#,
        ),
        ("motd.txt", "hello\n"),
        ("modules/a.lua", "return 'a'"),
        ("modules/b.lua", "return 'b'"),
        ("escape.lua", "return read_file('../x')"),
        ("missing.lua", "return read_dir('nowhere')"),
    ]);
    let cache = Project::new(&[]);
    let options = Options {
        cache: Some(cache.path("cache")),
        ..Options::default()
    };
    let first = project.eval("a.lua", &options).unwrap();
    assert_eq!(
        json(&first.value),
        r#"{"modules":["a","b"],"motd":"hello\n","root":"file"}"#
    );
    assert!(first.files.contains_key("motd.txt") && first.files.contains_key("modules/"));
    assert!(project.eval("a.lua", &options).unwrap().cached);
    project.write("modules/c.lua", "return 'c'");
    let grown = project.eval("a.lua", &options).unwrap();
    assert!(!grown.cached);
    assert!(json(&grown.value).contains(r#"["a","b","c"]"#));
    project.write("motd.txt", "bye\n");
    assert!(!project.eval("a.lua", &options).unwrap().cached);

    let error = project.eval("escape.lua", &Options::default()).unwrap_err();
    assert!(error.to_string().contains("cannot read '../x'"), "{error}");
    let error = project
        .eval("missing.lua", &Options::default())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot read the directory 'nowhere'"),
        "{error}"
    );
}

#[test]
fn the_build_command_writes_and_removes_only_its_own_files() {
    let project = Project::new(&[
        (
            "home.lua",
            r##"return { files = {
                ["a/b.txt"] = "b\n",
                ["bin/x"] = { text = "#!/bin/sh\n", executable = true },
            } }"##,
        ),
        (
            "less.lua",
            r##"return { files = { ["bin/x"] = { text = "#!/bin/sh\n", executable = true } } }"##,
        ),
    ]);
    let out = project.path("out");
    let build = |file: &str, extra: &[&str]| {
        let path = project.path(file);
        let mut args = vec!["build", arg(&path), "--out", arg(&out), "--attr", "files"];
        args.extend(extra);
        luna(&args)
    };
    assert_eq!(
        build("home.lua", &[]),
        (0, "+ a/b.txt\n+ bin/x\n".to_owned(), String::new())
    );
    assert_eq!(build("home.lua", &[]).1, "");
    assert_eq!(build("less.lua", &[]).1, "- a/b.txt\n");
    assert!(!out.join("a").exists(), "an emptied directory is removed");

    fs::write(out.join("bin/x"), "edited").unwrap();
    let (code, _, err) = build("home.lua", &[]);
    assert_eq!(code, 1);
    assert!(err.contains("bin/x (changed since luna wrote it)"), "{err}");
    assert_eq!(fs::read_to_string(out.join("bin/x")).unwrap(), "edited");
    assert_eq!(build("home.lua", &["--force"]).0, 0);
    assert_eq!(
        fs::read_to_string(out.join("bin/x")).unwrap(),
        "#!/bin/sh\n"
    );

    let (code, _, err) = luna(&["build", arg(&project.path("home.lua"))]);
    assert_eq!(code, 2);
    assert!(err.contains("build expects --out DIR"));
    let (code, _, err) = build("home.lua", &["--attr", "nope"]);
    assert_eq!(code, 1);
    assert!(err.contains("--attr: the result has no 'nope'"), "{err}");
}
