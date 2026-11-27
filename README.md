# luna

A stackless Lua interpreter in Rust, built to run untrusted scripts safely.

luna is a hard fork of [`piccolo`](https://github.com/kyren/piccolo) by way of
[`ottavino`](https://github.com/lumen-oss/ottavino), with an extended standard library and its
own release line. See [ACKNOWLEDGMENT.md](ACKNOWLEDGMENT.md).

## Why

* **Sandboxing.** A script cannot panic the interpreter, escape its arena, or reach anything you
  did not hand it. A Rust panic is not a catchable Lua error, so anything a script can reach that
  panics is a hole in the sandbox, not a bug in a library function — see
  [Hardening](#hardening) for how that is tested.
* **Bounded execution.** Every step runs on a fuel budget measured in VM instructions, and memory
  has a ceiling checked between steps — so CPU time and memory both stop a runaway script. Every
  loop the VM can enter is bounded, including metamethod chains.
* **Safe bindings.** Rust values become garbage-collected `UserData`, and callbacks can call
  back into Lua without using the Rust stack.

The VM is "stackless": Lua and Rust never nest on the Rust call stack. Control returns to your
loop between steps, which is what makes pausing, cancelling and metering possible at all.

For the API, `make rustdoc` builds the docs locally.

## Example

```rust
use luna::{Closure, Executor, Lua};

let mut lua = Lua::full();

let ex = lua.try_enter(|ctx| {
    let closure = Closure::load(ctx, None, &b"return 1 + 1"[..])?;
    Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
})?;

assert_eq!(lua.execute::<i64>(&ex)?, 2);
```

A REPL and two larger examples live in [`examples/`](examples); run one with `make repl`.

## Functional states

`Lua::functional()` is for configuration that has to be pure: the same inputs always give the
same result, and nothing a config produces can change after it is evaluated. It follows
Starlark's model, not Nix's: evaluation is strict, a chunk may build and mutate its own tables,
and the host freezes the results when the chunk finishes.

| Guarantee | Mechanism |
| --- | --- |
| No ambient inputs or effects | No `io`, `os`, `debug`, `package`, `coroutine`, `print`, `collectgarbage`, `dofile`, `loadfile`, `math.random(seed)`, `string.dump` |
| Shared state cannot change | Globals and every library table, including the string metatable, are frozen |
| Results cannot change | `luna::functional::freeze` deep-freezes tables, table and userdata metatables, and closure captures |
| Closures carry no hidden state | A captured local is effectively final: assigning to it anywhere, inside or outside the closure, is a compile error, so a value can be frozen at any point during evaluation |
| Same traversal every run | `pairs`/`next` visit booleans, numbers, then strings by bytes; other key types are refused |
| Output does not depend on memory layout or CPU | `tostring` and `%s` print `table`/`function` without addresses; `%p` is refused; every NaN prints as `nan` and packs to the same bytes, whatever sign bit the CPU produced |
| Only checked code runs | Binary chunks are refused; `load` is text-only |
| GC timing is unobservable | Metatables with `__gc` or `__mode` are refused |
| Same numbers on every target | `math.exp`, `log`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan` use the portable `libm` crate; the `^` operator still uses the platform `pow` |
| Files are pure inputs | `functional::import_environment` adds `import(name)`: the host's loader supplies the text, each name is evaluated once and its result frozen and cached, and cycles are refused |
| Works with the JIT | Native code honours every guarantee; `tests/functional.rs` runs under JIT Off, Auto and Force and checks native output is byte-identical to the interpreter's and to separate processes' |

```rust
use luna::{functional, Closure, Executor, Lua, RuntimeError, Value};

let mut lua = Lua::functional();
let (ex, env) = lua.try_enter(|ctx| {
    let env = functional::environment(ctx);
    let source = &b"name = 'luna' return { name = name }"[..];
    let closure = Closure::load_with_env(ctx, None, source, env)?;
    Ok((ctx.stash(Executor::start(ctx, closure.into(), ())), ctx.stash(env)))
})?;
lua.finish(&ex).map_err(RuntimeError::new)?;
lua.try_enter(|ctx| {
    let config: Value = ctx.fetch(&ex).take_result(ctx)??;
    functional::freeze(ctx, config);
    functional::freeze(ctx, ctx.fetch(&env).into());
    Ok(())
})?;
```

`functional::environment` gives each chunk its own globals over the frozen library. Host
callbacks are opaque to the runtime: a host that registers one is responsible for keeping it
pure, for example by recording declarations and applying them after evaluation.

### Evaluating a configuration

`functional::evaluate(path, &options)` does all of the above in one call and returns plain
`Data`, and `luna eval` is the same thing from a shell, printing canonical JSON the way
`nix eval --json` does, or TOML, YAML or INI with `--format`, or a string result as-is with
`--raw`:

```sh
luna eval service.lua --input-string env=prod --schema schema.lua --cache ~/.cache/luna
```

- **Inputs** arrive as the frozen `inputs` global, an empty map by default. A file that returns
  a function is called with them, as a Nix file returning a function is.
- **Imports** read `import("lib/x.lua")` relative to the configuration's directory, or `--root`.
  Absolute paths, `.`/`..` and symlinks that leave the root are refused.
- **Data files**: `import("pkgs.toml")` and `import("lock.json")` return the parsed file as frozen
  data, the way Nix's `fromTOML` and `fromJSON` are used. Dates and times stay as their text.
- **Limits**: a total instruction budget (`--fuel`) and a memory ceiling (`--memory`) stop
  runaway configurations with an error instead of hanging the host.
- **Errors** name the import chain: `c.lua:1: broken config` followed by
  `in 'c.lua', imported from b.lua:3` and `in 'b.lua', imported from a.lua:2`.
- **Hashes**: every result carries the SHA-256 of every file read with the inputs, API and
  schema (`input_hash`), and of the canonical result (`output_hash`), so a configuration has a
  reproducible identity. `--hash` prints them. `--files` prints every file read with its hash, for
  build systems that track dependencies.
- **Cache**: with `--cache DIR`, a result is reused without evaluating while the file, its
  inputs, its API, its schema and every file it imported are byte-for-byte unchanged.

The result must be data: nil, booleans, numbers, strings, lists (keys exactly `1..n`) and maps.
A function in the result is an error naming its path, such as
`value.services.web.start: cannot convert a function`. JSON output additionally needs string
keys, UTF-8 strings and finite numbers.

### Registration-style APIs

A configuration written as calls, such as `oslo.env.set("TOP", oslo.sys.pwd())`, stays pure when
the host describes its API with `functional::Api` instead of acting on each call:

```rust
let api = Api::new()
    .record("env.set")            // calls are appended to a log the script cannot read
    .returning("sys.pwd", "/srv") // host state becomes a fixed input
    .value("version", "1.2");
```

The arguments of every recorded call are frozen when it is made, and `Evaluated::declarations`
lists the calls in order for the host to apply afterwards. From the shell:
`luna eval .env.lua --api oslo --record env.set --returning 'sys.pwd="/srv"' --declarations`.

### The `lib` global

Every functional state has a frozen `lib`, written in functional Lua. Nothing in it changes its
arguments, so it works on frozen and imported data.

| | |
| --- | --- |
| Merging | `merge(a, b, ...)` (Nix `//`), `deep_merge(a, b)` (`recursiveUpdate`; lists are replaced) |
| Lists | `map`, `filter`, `fold`, `concat`, `flatten`, `range`, `find`, `any`, `all`, `contains`, `unique`, `sort`, `group_by`, `is_list` |
| Maps | `keys`, `values`, `map_values`, `filter_values`, `from_pairs` (all in canonical key order) |
| Structure | `equal` (deep, cycle-safe), `copy` (a mutable deep copy) |
| Overrides | `rewrite(tree, replace)`: Guix-style input rewriting. Replaces nodes by identity, through a table keyed by old nodes or a function, rebuilds only what contains them and keeps everything else shared |
| Encodings | `to_json(v, { pretty = true })`, `to_toml`, `to_yaml`, `to_ini` produce text in canonical key order; `from_json`, `from_toml` parse it |
| Modules | `modules.evaluate{ modules, args, rounds }`, `modules.options{ modules }`, `option{ type, default, description }`, `mk_default`, `mk_force`, `mk_override`, `mk_if`, `mk_merge`, `mk_before`, `mk_after`, `mk_order` |
| Schemas | `check(value, type)` with `lib.types`: `any`, `string`, `integer`, `number`, `boolean`, `func`, `list(t)`, `map(t)`, `record(fields, { open = true })`, `optional(t)`, `default(t, v)`, `enum{...}`, `one_of(...)`, `custom(name, test)` |

A schema both validates and normalises: defaults are filled in, unknown record fields are
refused, and errors name the exact path, as in `config.hosts[2]: expected a string, got number 3`.

### Modules

`lib.modules.evaluate` is the NixOS module system in strict form. Each module declares options
and defines values for any option, and the definitions are merged by the option's type:

```lua
-- server.lua
local types = lib.types
return {
    options = { server = {
        enable = lib.option{ type = types.boolean, default = false },
        port = lib.option{ type = types.integer, default = 80 },
    } },
    config = function(config)
        return lib.mk_if(config.server.enable, { firewall = { ports = { config.server.port } } })
    end,
}

-- host.lua
return lib.modules.evaluate{ modules = {
    import("server.lua"), import("firewall.lua"),
    { server = { enable = true, port = lib.mk_default(8080) } },
} }
```

- **Merging**: lists concatenate (`mk_before`/`mk_after` order them), maps and records merge key
  by key, and every other value must be defined the same everywhere. Otherwise the error lists
  each definition and its module.
- **Priorities**: a plain definition beats `mk_default`, which beats the option's `default`;
  `mk_force` beats them all. Wrappers work at any depth and push down through sets of options.
- **Modules** are tables of `imports`, `options` and `config`, or functions of `args` returning
  one. A table with neither `options` nor `config` is all config. A module imported twice counts
  once.
- **The configuration** is visible through `config = function(config) ... end`. Without
  laziness, these functions run again until the configuration stops changing, at most `rounds`
  times (16 by default), and a final strict round reports errors against the settled result.
  A configuration that keeps changing is an error naming the path. Calls to a recorded API made
  inside these functions are recorded once per round, so make them outside.
- **Errors** name the option and the modules: `config.server.prot: no such option, defined in
  host.lua`, or `config.x: no value is defined and the option has no default`.
- `lib.modules.options{ modules = ... }` lists every option by dotted name with its type,
  default, description and the module that declared it, which is enough to generate docs.

## Status

Pre-1.0 and experimental. Expect frequent breaking changes on minor version bumps.

**Works:** the core language (closures, proper tail calls, varargs, coroutines, goto, `_ENV`,
metatables and recursive metamethods), an incremental cycle-detecting GC with `__gc` finalizers and
weak tables, the callback and async-sequence system, the `debug` library including `traceback`,
`sethook` and `getlocal`/`setlocal`, bytecode dump/load, and the stdlib.

**Doesn't yet:** optimised bytecode, and the last corners of the stdlib — see
[COMPATIBILITY.md](COMPATIBILITY.md).

**Won't:** the PUC-Rio C API, C library loading (`package.cpath`, `loadlib`), PUC-Rio bytecode
compatibility, `debug` call/return hook masks, or byte-for-byte agreement with PUC-Rio on error
strings, table iteration order and locale-dependent behaviour. luna targets PUC-Rio Lua under the
"C" locale with default `luaconf.h` on 64-bit Linux.

[COMPATIBILITY.md](COMPATIBILITY.md) tracks what matches PUC-Rio Lua, function by function.

## Hardening

The sandbox claim above is only worth what it is tested against, so it is tested against scripts
written to break it. Two suites of hostile programs — unbounded recursion, cyclic metamethod
chains, `2^60`-byte strings, pathological table borders, malformed bytecode, `__gc` handlers that
error, coroutines that resume themselves — must each produce a *catchable Lua error* or a correct
result. Never a panic, a hang, or an out-of-memory kill.

That bar was set by an audit of the whole tree, which found six ways a pure Lua script could kill
or hang its host and three ways it could silently corrupt data. All are fixed, each with a
regression test; [CHANGELOG.md](CHANGELOG.md) lists them individually. The general lessons:

- **A panic is a sandbox escape.** `math.mininteger % -1` aborted the process because Rust's `%`
  traps on that one input. `pcall` cannot catch it, so the host dies with the script.
- **A border is not a count.** `#t` can be `2^62` on a table holding two entries, and anything that
  preallocates `#t` elements dies on it.
- **Silent corruption is worse than a crash.** `string.pack("s1", <300 bytes>)` used to write the
  payload with a truncated length prefix, so the reader got a prefix and no error.

Where PUC-Rio would hang or abort, luna raises instead; the bounds are listed in
[COMPATIBILITY.md](COMPATIBILITY.md#limits). Beyond that the suite is ~330 tests, and `make verify`
runs all of them with every feature enabled.

## Building

`make help` lists everything. The common ones:

```
make build      # workspace and examples
make test       # tests, including doc tests
make repl       # the interpreter example
make verify     # the full local gate
```

A Nix dev shell with the pinned toolchain is in [`flake.nix`](flake.nix).

## Features

luna's defaults are the whole library and nothing optional. Optional features add API surface that not
every embedder wants to compile:

| Feature | Adds | Costs |
| --- | --- | --- |
| `async` | `AsyncSequence::await_future`, `Lua::execute_async` — awaiting foreign futures from a callback | Nothing but code. It is `std::task` throughout; choosing a runtime stays yours |
| `derive` | `#[derive(FromValue)]`, `#[derive(IntoValue)]` | `syn`, `quote` and `proc-macro2` in the build |
| `jit` | Experimental native scalar/heap slices; constructors remain interpreted by default | Pinned Cranelift on Linux and macOS, x86-64/aarch64; the out-of-process compiler worker is Linux-only. See [JIT.md](JIT.md) for current coverage and limits |

```rust
# use luna::{FromValue, IntoValue};
#[derive(FromValue, IntoValue)]
struct Point {
    x: i64,
    y: i64,
}
```

A struct becomes a table keyed by field name; a fieldless enum becomes its variant's name as a
string. Generic types work — each type parameter picks up the matching bound, so `Pair<T>` converts
whenever `T` does. A tuple struct or a variant carrying data is a *compile* error naming the
problem, rather than a guess at what the table should look like.

## Threads

`Lua` is not `Send`, and that is a deliberate boundary rather than a gap waiting to be closed. The
arena owns every value behind a `'gc` lifetime brand, and that ownership is exactly what lets luna
guarantee a value can never outlive its collector without a line of `unsafe` in the VM. Making the
state movable between threads means changing that model, not adding an `impl`.

The supported pattern is **one `Lua` per thread, with values crossing as owned Rust data**:

- Anything the script returns converts to ordinary Rust types through the same `FromMultiValue`
  machinery callbacks use, and those cross a channel freely.
- For values with no fixed Rust type, `luna_util::serde::SerializeValue` pairs a value with its
  context and serializes it into any serde format; `from_value` reads one back on the other side.
- `Stashed*` handles let work be suspended and resumed across `enter` boundaries on the owning
  thread, so a worker can interleave several scripts without holding a borrow.

[`examples/worker_thread.rs`](examples/worker_thread.rs) is a working version: a `Lua` living on a
worker thread, fed source over a channel and replying with results.

What this rules out is sharing a *single* interpreter between threads — that needs a lock around
the whole state in any implementation, PUC-Rio included, so little is lost. What it does not rule
out is parallelism: N threads with N interpreters scale linearly, with no global lock between them.

## Binary size

Measured on x86-64 Linux, stripped, `lto = true`, `codegen-units = 1`, `panic = "abort"`, as the
delta over an identical binary that does not use luna:

| What you use | Added |
| --- | --- |
| `Lua::empty()` — VM, GC, values, tables, strings | 428 KB |
| + `Closure::load` (the Lua source compiler) | 595 KB |
| + `Lua::core()` (base, string, table, math, coroutine, utf8) | 794 KB |
| + `Lua::full()` (io, os, package, debug) | 928 KB |

The tiers are real measurements of separate binaries, not estimates. Because luna is pure Rust with
no FFI, the linker can see the whole program: skip `Closure::load` and the parser and code generator
are gone; use `Lua::core()` and you do not pay for `io`/`os`/`package`. Of the dependency tree, the
entire cost is around 40 KB — gc-arena, anyhow, hashbrown, allocator-api2, ahash, rand and getrandom
combined. The rest is luna's own code.

`opt-level = "s"` is worth setting if size matters:

| `opt-level` | `Lua::core()` | VM speed |
| --- | --- | --- |
| `3` (cargo's default) | 794 KB | baseline |
| `"s"` | **600 KB** | ~19% slower |
| `"z"` | 557 KB | ~2.4x slower |

**Use `"s"`, not `"z"`.** The extra 43 KB `"z"` saves costs a 2.4x slower VM: `run_vm` is a single
large dispatch loop and `"z"` turns off the inlining it depends on. `"s"` gives most of the size
win for a fraction of the cost.

One consequence worth knowing before you benchmark luna: at `opt-level = "s"` with LTO and one
codegen unit, `run_vm` is large enough that unrelated edits move its code layout, and that moves
microbenchmark results by several percent in either direction. A measured 8% change on integer `%`
turned out to be layout alone — the arithmetic was byte-identical between the two builds, and at
`opt-level = 3` they were indistinguishable. Compare builds by A/B inside a *single* binary, or
measure at `opt-level = 3`, before believing a small regression.

## Safety

Most of luna is safe Rust. The unsafe parts are isolated and never leak into the public API —
you can use even the low-level details without writing `unsafe` yourself. They are: hashbrown's
`RawTable` for Lua table semantics, non-`'static` userdata downcasting, tunnelling parameters
into async sequences, and avoiding fat pointers to keep `Value` small.

No attempt is made to guard against side-channel attacks. The optional native tier does not
provide side-channel isolation, and host callbacks can expose timing information.

## License

MIT — see [LICENSE](LICENSE).

luna's parents offer their code under MIT or CC0 at the recipient's option; luna takes the MIT
branch and ships under MIT alone. The original copyright notices are preserved.
