# luna

A stackless Lua interpreter in Rust, built to run untrusted scripts safely.

luna is a hard fork of [`piccolo`](https://github.com/kyren/piccolo) by way of
[`ottavino`](https://github.com/lumen-oss/ottavino), with an extended standard library and its
own release line. See [ACKNOWLEDGMENT.md](ACKNOWLEDGMENT.md).

## Why

* **Sandboxing.** A script cannot panic the interpreter, escape its arena, or reach anything you
  did not hand it.
* **Bounded execution.** Every step runs on a fuel budget measured in VM instructions, and the
  arena tracks its own allocations — so CPU and memory both have ceilings you set.
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
