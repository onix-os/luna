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

luna's defaults are the whole library and nothing optional. Two features add API surface that not
every embedder wants to compile:

| Feature | Adds | Costs |
| --- | --- | --- |
| `async` | `AsyncSequence::await_future`, `Lua::execute_async` — awaiting foreign futures from a callback | Nothing but code. It is `std::task` throughout; choosing a runtime stays yours |
| `derive` | `#[derive(FromValue)]`, `#[derive(IntoValue)]` | `syn`, `quote` and `proc-macro2` in the build |

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

No attempt is made to guard against side-channel attacks. With no JIT and no callback API for
accurately measuring time, that may not be practical anyway.

## License

MIT — see [LICENSE](LICENSE).

luna's parents offer their code under MIT or CC0 at the recipient's option; luna takes the MIT
branch and ships under MIT alone. The original copyright notices are preserved.
