# luna

A stackless Lua interpreter in Rust, built to run untrusted scripts safely.

luna is a hard fork of [`piccolo`](https://github.com/kyren/piccolo) by way of
[`ottavino`](https://github.com/lumen-oss/ottavino), with an extended standard library and its
own release line. See [ACKNOWLEDGMENT.md](ACKNOWLEDGMENT.md).

## Why

* **Sandboxing.** A script cannot panic the interpreter, escape its arena, or reach anything you
  did not hand it.
* **Bounded execution.** Every step runs on a fuel budget measured in VM instructions, and memory
  has a ceiling checked between steps — so CPU time and memory both stop a runaway script.
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
metatables and recursive metamethods), an incremental cycle-detecting GC, the callback and
async-sequence system, and the fundamental parts of the stdlib.

**Doesn't yet:** `__gc` finalizers, stack traces, a debugger, good error messages, and much of
the peripheral stdlib. Bytecode is unoptimised.

**Won't:** the PUC-Rio C API, C library loading, bytecode compatibility, the `debug` library, or
byte-for-byte agreement with PUC-Rio on error strings, table iteration order and locale-dependent
behaviour. luna targets PUC-Rio Lua under the "C" locale with default `luaconf.h` on 64-bit Linux.

[COMPATIBILITY.md](COMPATIBILITY.md) tracks what matches PUC-Rio Lua, function by function.

## Building

`make help` lists everything. The common ones:

```
make build      # workspace and examples
make test       # tests, including doc tests
make repl       # the interpreter example
make verify     # the full local gate
```

A Nix dev shell with the pinned toolchain is in [`flake.nix`](flake.nix).

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
| + `Lua::core()` (base, string, table, math, coroutine) | 794 KB |
| + `Lua::full()` (io, os, package, utf8, debug) | 928 KB |

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

## Safety

Most of luna is safe Rust. The unsafe parts are isolated and never leak into the public API —
you can use even the low-level details without writing `unsafe` yourself. They are: hashbrown's
`RawTable` for Lua table semantics, non-`'static` userdata downcasting, tunnelling parameters
into async sequences, and avoiding fat pointers to keep `Value` small.

No attempt is made to guard against side-channel attacks. With no JIT and no callback API for
accurately measuring time, that may not be practical anyway.

## License

MIT — see [LICENSE-MIT](LICENSE-MIT).

luna's parents offer their code under MIT or CC0 at the recipient's option; luna takes the MIT
branch and ships under MIT alone. The original copyright notices are preserved.
