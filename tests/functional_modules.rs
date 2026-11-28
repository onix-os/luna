//! `lib.modules`: NixOS-style option declarations and definitions merged by type and priority,
//! config functions of the settled configuration, and errors that name the modules involved.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use luna::functional::{evaluate, Options};

/// A scratch directory of files, removed when dropped.
struct Project(PathBuf);

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "luna-functional-modules-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        for (name, text) in files {
            fs::write(root.join(name), text).unwrap();
        }
        Project(root)
    }

    fn json(&self, name: &str) -> Result<String, String> {
        evaluate(self.0.join(name), &Options::default())
            .map(|evaluated| evaluated.value.to_json(false).unwrap())
            .map_err(|error| error.to_string())
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SERVER: &str = r#"
local types = lib.types
return {
    options = { server = {
        enable = lib.option{ type = types.boolean, default = false, description = "Serve" },
        port = lib.option{ type = types.integer, default = 80 },
        hosts = lib.option{
            type = types.map(types.record{ root = types.string, tls = types.default(types.boolean, false) }),
            default = {},
        },
    } },
    config = function(config)
        return lib.mk_if(config.server.enable, { firewall = { ports = { config.server.port } } })
    end,
}
"#;

const FIREWALL: &str = r#"
return {
    options = { firewall = {
        ports = lib.option{ type = lib.types.list(lib.types.integer), default = {} },
    } },
    config = { firewall = { ports = lib.mk_before({ 22 }) } },
}
"#;

fn modules(extra: &str) -> String {
    format!(
        "return lib.modules.evaluate{{ modules = {{ import('server.lua'), import('firewall.lua'), {extra} }} }}"
    )
}

fn project(config: &str) -> Project {
    Project::new(&[
        ("server.lua", SERVER),
        ("firewall.lua", FIREWALL),
        ("config.lua", config),
    ])
}

#[test]
fn defaults_fill_every_option_nothing_defines() {
    let project = project(&modules("{}"));
    assert_eq!(
        project.json("config.lua").unwrap(),
        r#"{"firewall":{"ports":[22]},"server":{"enable":false,"hosts":{},"port":80}}"#
    );
}

#[test]
fn definitions_merge_by_type_and_priority() {
    let project = project(&modules(
        r#"
        { server = { enable = true, port = lib.mk_default(8080), hosts = { a = { root = "/a" } } } },
        { server = { port = 443, hosts = { b = { root = "/b", tls = true } } } },
        { firewall = { ports = lib.mk_after({ 9000 }) } },
        "#,
    ));
    assert_eq!(
        project.json("config.lua").unwrap(),
        concat!(
            r#"{"firewall":{"ports":[22,443,9000]},"#,
            r#""server":{"enable":true,"hosts":{"a":{"root":"/a","tls":false},"#,
            r#""b":{"root":"/b","tls":true}},"port":443}}"#
        )
    );
}

#[test]
fn a_forced_definition_replaces_the_others() {
    let project = project(&modules(
        r#"
        { server = { port = 1, hosts = { a = { root = "/a" } } } },
        { server = { port = lib.mk_force(2), hosts = lib.mk_force({ c = { root = "/c" } }) } },
        "#,
    ));
    assert_eq!(
        project.json("config.lua").unwrap(),
        r#"{"firewall":{"ports":[22]},"server":{"enable":false,"hosts":{"c":{"root":"/c","tls":false}},"port":2}}"#
    );
}

#[test]
fn wrappers_push_down_through_sets_of_options() {
    let project = project(&modules(
        r#"
        { server = lib.mk_merge({ lib.mk_if(false, { port = 1 }), lib.mk_force({ port = 3 }) }) },
        { server = { port = 2 } },
        "#,
    ));
    assert!(project.json("config.lua").unwrap().contains(r#""port":3"#));
}

#[test]
fn config_functions_read_the_settled_configuration_across_modules() {
    let project = Project::new(&[(
        "config.lua",
        r#"
        local types = lib.types
        local a = {
            options = { a = lib.option{ type = types.integer, default = 1 } },
            config = function(config) return { a = config.c + 1 } end,
        }
        local b = {
            options = { b = lib.option{ type = types.integer } },
            config = function(config) return { b = config.a * 10 } end,
        }
        local c = {
            options = { c = lib.option{ type = types.integer, default = 0 } },
            config = function(config) return lib.mk_if(config.b == nil, {}) end,
        }
        return lib.modules.evaluate{ modules = { b, a, c, { c = 4 } } }
        "#,
    )]);
    assert_eq!(
        project.json("config.lua").unwrap(),
        r#"{"a":5,"b":50,"c":4}"#
    );
}

#[test]
fn a_module_imported_twice_counts_once() {
    let project = project(
        r#"
        local server = import("server.lua")
        return lib.modules.evaluate{ modules = {
            { imports = { server, import("firewall.lua") } },
            { imports = { server }, server = { enable = true } },
        } }
        "#,
    );
    assert_eq!(
        project.json("config.lua").unwrap(),
        r#"{"firewall":{"ports":[22,80]},"server":{"enable":true,"hosts":{},"port":80}}"#
    );
}

#[test]
fn module_functions_receive_the_arguments() {
    let project = Project::new(&[(
        "config.lua",
        r#"
        local function named(args)
            return {
                options = { name = lib.option{ type = lib.types.string, default = args.host } },
                config = function(config, args) return { name = lib.mk_default(args.host .. "!") } end,
            }
        end
        return lib.modules.evaluate{ modules = { named }, args = { host = "box" } }
        "#,
    )]);
    assert_eq!(project.json("config.lua").unwrap(), r#"{"name":"box!"}"#);
}

#[test]
fn closures_in_the_configuration_settle() {
    let project = Project::new(&[(
        "config.lua",
        r#"
        local types = lib.types
        local config = lib.modules.evaluate{ modules = { {
            options = { n = lib.option{ type = types.integer, default = 3 }, f = lib.option{ type = types.func } },
            config = function(config) return { f = function(x) return x * config.n end } end,
        } } }
        return config.f(2)
        "#,
    )]);
    assert_eq!(project.json("config.lua").unwrap(), "6");
}

#[test]
fn options_are_documented() {
    let project = project("return lib.modules.options{ modules = { import('server.lua') } }");
    let json = project.json("config.lua").unwrap();
    assert!(json.contains(
        r#""server.enable":{"declared":"server.lua","default":false,"description":"Serve","type":"boolean"}"#
    ));
    assert!(json.contains(
        r#""server.hosts":{"declared":"server.lua","default":{},"type":"map of record"}"#
    ));
}

#[test]
fn errors_name_the_option_and_the_modules() {
    for (extra, expected) in [
        (
            "{ server = { port = 1 } }, { server = { port = 2 } }",
            "config.server.port: conflicting definitions\n  number 1 in module 3\n  number 2 in module 4",
        ),
        (
            "{ server = { prot = 1 } }",
            "config.server.prot: no such option, defined in module 3",
        ),
        (
            r#"{ server = { port = "80" } }"#,
            "config.server.port: expected an integer, got string \"80\"\n  defined in module 3",
        ),
        (
            r#"{ server = { hosts = { a = { root = "/a", tsl = true } } } }"#,
            "config.server.hosts.a.tsl: unknown field, defined in module 3",
        ),
        (
            "{ options = { server = { port = lib.option{} } } }",
            "options.server.port: declared in server.lua and in module 3",
        ),
        (
            "'server.lua'",
            "modules: modules are values; write import(\"server.lua\") instead of the name",
        ),
        (
            "function(args) return { firewall = args.config } end",
            "module functions run before the configuration exists",
        ),
        (
            "{ options = { x = lib.option{ type = lib.types.integer } } }",
            "config.x: no value is defined and the option has no default",
        ),
        (
            "{ options = { x = lib.option{ type = lib.types.integer, default = 0 } }, \
               config = function(config) return { x = config.x + 1 } end }",
            "the configuration did not settle after 16 rounds: config.x keeps changing",
        ),
    ] {
        let error = project(&modules(extra)).json("config.lua").unwrap_err();
        assert!(error.contains(expected), "{extra}\n{error}");
    }
}

#[test]
fn computed_defaults_follow_other_options() {
    let project = Project::new(&[(
        "config.lua",
        r#"
        local types = lib.types
        local base = { options = {
            home = lib.option{ type = types.string },
            bin = lib.option{ type = types.string, default_from = function(c) return c.home .. "/bin" end },
            data = lib.option{ type = types.string, default_from = function(c) return c.bin .. "/../data" end },
        } }
        local config = lib.modules.evaluate{ modules = { base, { home = "/h" } } }
        local moved = lib.modules.evaluate{ modules = { base, { home = "/h", bin = lib.mk_default("/b") } } }
        return { config.bin, config.data, moved.bin, moved.data,
                 lib.modules.options{ modules = { base } }.bin.computed }
        "#,
    )]);
    assert_eq!(
        project.json("config.lua").unwrap(),
        r#"["/h/bin","/h/bin/../data","/b","/b/../data",true]"#
    );
}

#[test]
fn map_items_can_depend_on_their_key() {
    let project = Project::new(&[(
        "config.lua",
        r#"
        local types = lib.types
        local user = function(key) return types.record{ name = types.default(types.string, key) } end
        return lib.modules.evaluate{ modules = {
            { options = {
                users = lib.option{ type = types.map(user), default = {} },
                on = lib.enable_option("the thing"),
            } },
            { users = { alice = {}, bob = { name = "Robert" } } },
        } }
        "#,
    )]);
    assert_eq!(
        project.json("config.lua").unwrap(),
        r#"{"on":false,"users":{"alice":{"name":"alice"},"bob":{"name":"Robert"}}}"#
    );
}

#[test]
fn modules_are_named_by_their_file_or_their_call_site() {
    let project = project(
        r#"
        local config = lib.modules.evaluate{ modules = {
            import("server.lua"), import("firewall.lua"),
            { firewall = { ports = "22" } },
        } }
        return config
        "#,
    );
    let error = project.json("config.lua").unwrap_err();
    assert!(
        error.contains(
            "config.firewall.ports: expected a list, got string \"22\" in module 3 at config.lua:2"
        ),
        "{error}"
    );
    let project = project_with_conflict();
    let error = project.json("config.lua").unwrap_err();
    assert!(
        error.contains("  number 1 in firewall.lua\n  number 2 in server.lua"),
        "{error}"
    );
}

fn project_with_conflict() -> Project {
    Project::new(&[
        (
            "server.lua",
            "return { options = { port = lib.option{} }, config = { port = 2 } }",
        ),
        ("firewall.lua", "return { port = 1 }"),
        (
            "config.lua",
            "return lib.modules.evaluate{ modules = { import('firewall.lua'), import('server.lua') } }",
        ),
    ])
}

/// The geto Home Manager module ported to luna, in examples/functional/home.
#[test]
fn the_geto_port_writes_its_manifest_activation_and_path() {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/functional/home");
    let options = Options {
        inputs: luna::functional::Data::from_json(r#"{"home": "/home/me"}"#).unwrap(),
        ..Options::default()
    };
    let evaluated = evaluate(example.join("home.lua"), &options).unwrap();
    let files = evaluated
        .value
        .get("home")
        .and_then(|home| home.get("files"))
        .unwrap();
    let out = Project::new(&[]);
    let written = luna::functional::write_files(files, &out.0, false).unwrap();
    assert_eq!(
        written.written,
        [
            ".config/geto/list.json",
            ".config/luna/env.sh",
            ".local/share/luna/activate"
        ]
    );
    let read = |name: &str| fs::read_to_string(out.0.join(name)).unwrap();
    let manifest = luna::functional::Data::from_json(&read(".config/geto/list.json")).unwrap();
    assert_eq!(
        manifest.to_json(false).unwrap(),
        concat!(
            r#"{"bins":{"/home/me/.local/bin/atuin":{"patch":true,"path":"/home/me/.local/bin/atuin","#,
            r#""tags":["shell","essential"],"url":"github.com/atuinsh/atuin"},"#,
            r#""/home/me/.local/bin/git-town":{"patch":true,"path":"/home/me/.local/bin/git-town","#,
            r#""tags":["essential"],"url":"github.com/git-town/git-town"},"#,
            r#""/home/me/.local/bin/mdBook":{"patch":true,"path":"/home/me/.local/bin/mdBook","#,
            r#""tags":["default"],"url":"github.com/rust-lang/mdBook"}},"#,
            r#""default_path":"/home/me/.local/bin"}"#
        )
    );
    assert!(
        read(".local/share/luna/activate").contains("GETO_NONINTERACTIVE=1 geto --tag all ensure")
    );
    assert_eq!(
        read(".config/luna/env.sh"),
        "export PATH=/home/me/.local/bin:\"$PATH\"\n"
    );
}

mod common;
