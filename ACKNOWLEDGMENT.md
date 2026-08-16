# Acknowledgment

`luna` is a hard fork. Almost none of what makes it work was written here, and this file
says whose it is.

## `piccolo` — the interpreter

[`piccolo`](https://github.com/kyren/piccolo), by **kyren** and its contributors, is where
luna comes from. It is the stackless VM, the compiler, the sequence and callback API, the
fuel-metered execution model, and the design that lets untrusted Lua be run without giving it
the power to hang or exhaust the host. Everything luna claims about sandboxing and bounded
execution is piccolo's work.

Its garbage collector, [`gc-arena`](https://github.com/kyren/gc-arena) — the `Collect` trait,
`Gc` pointers and the arena/mutation model that makes Rust values safe to hand to a collector —
comes from the same author and is the piece luna could least do without.

kyren has written about the design in
[piccolo - A Stackless Lua Interpreter](https://kyju.org/blog/piccolo-a-stackless-lua-interpreter/),
which is still the best explanation of how this interpreter works.

## `ottavino` — the standard library and the maintenance

[`ottavino`](https://github.com/lumen-oss/ottavino), by **lumen-oss**, is the fork luna is
taken directly from. It carried piccolo forward through a quiet period upstream and did the
unglamorous half of the work: extending the standard library, chasing PUC-Rio Lua
compatibility closely enough for real rockspecs to run, and keeping
[`ottavino-gc-arena`](https://github.com/lumen-oss/gc-arena) published so the collector stayed
available on crates.io.

luna still depends on `ottavino-gc-arena` directly, and will keep doing so.

## What luna changed

The name, and the intent. `ottavino` described itself as a temporary, parallel fork that would
be deprecated once piccolo's standard library caught up. luna does not plan to merge back; it
is developed independently, on its own release line, and divergence from either parent is
expected rather than avoided.

That is a change of direction, not a complaint. Both projects were run well, and this fork
exists because their work was good enough to build on.

## Licensing

piccolo and ottavino offer their code under MIT or CC0, at the recipient's option. luna takes
the MIT branch and ships under MIT alone ([LICENSE-MIT](LICENSE-MIT)) — a choice that grant
exists to allow, not a relicensing of anyone's work. Upstream copyright notices are preserved,
and the CC0 option remains available to anyone who takes the code from piccolo or ottavino
directly.

Individual contributors credited in [CHANGELOG.md](CHANGELOG.md) for work done upstream keep
that credit; those entries predate this fork and have not been rewritten.
