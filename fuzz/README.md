# Native JIT differential fuzzing

This standalone package is excluded from Luna's normal workspace. It uses
libFuzzer's coverage feedback to mutate bytes that select bounded Lua source
programs, then executes each program in fresh interpreted and native states.
It does not feed arbitrary Lua text or malformed bytecode to the runtime.

## Targets and oracle

- `scalar`: integer/float boundaries, arithmetic, shifts, comparisons, moves,
  truthiness and catchable operation errors. At most 16 statements and eight
  loop iterations; each selected statement runs through `pcall`.
- `heap`: table reads/writes, cycles, closed upvalues, metatable fallback,
  weak values, catchable nil-key errors, bounded coroutine yields and four
  current-frame upvalue-rebinding scenarios. At most 16 selected heap statements
  and eight loop iterations.

Both targets compare completion, executor mode and remaining fuel after every
main-executor slice. Both states receive full host GC after each slice. A
selected early boundary clears and prepares native code again. Final results
compare primitive type, integer value, string bytes and floating-point bits
(including signed zero and infinities). NaNs compare by category, not payload.
The heap program asserts internal alias identity. Every case must execute
native instructions; the two successful alias scenarios also require native
upvalue-write counters. Panics, unexpected errors, signals and timeouts fail.
These observations are not a complete ordered-effect or arbitrary-heap oracle.

## Run through Make

The `.#fuzz` Nix shell pins nightly 2026-08-16 and supplies cargo-fuzz and clang
from the locked nixpkgs input. The package tracks its own `Cargo.lock` and fixed
seeds. Runtime corpora and crash artifacts remain untracked.

```sh
nix develop .#fuzz -c make jit-coverage-environment
nix develop .#fuzz -c make jit-coverage-fmt-check jit-coverage-check jit-coverage-test
nix develop .#fuzz -c make jit-coverage-run CG_TARGET=scalar CG_RUNS=256 CG_SECONDS=60
nix develop .#fuzz -c make jit-coverage-run CG_TARGET=heap CG_RUNS=256 CG_SECONDS=60
```

The defaults retain environment, corpus, artifact and run logs under
`target/jit-evidence/coverage-guided/<target>/`. Choose a new `CG_DIR` for an
independent campaign; existing corpora are reused, not silently deleted.
Runs must be 1..100000; campaign seconds must be 1..3600. Inputs are at most
256 bytes, individual inputs have a 20-second timeout, and libFuzzer monitors
a 2048-MiB RSS limit. An outer wall timeout is campaign seconds plus 60 seconds,
with a ten-second termination grace period. Instrumented builds occur before
this deadline and are not compiler-memory/CPU isolated. `CARGO_TARGET_DIR` and
`CARGO_BUILD_JOBS` may be set for builds. Campaign time is not acceptance timing
for JIT performance.

Cargo-fuzz 0.13.1 has no `--locked` build/run option. Make first checks the
package with offline `cargo check --locked`, then runs cargo-fuzz offline and
rejects any lockfile hash change on exit, including a failing exit. Populate
dependencies with the normal online `jit-coverage-check` first on a fresh
machine. Only `jit-coverage-lock` intentionally regenerates the dependency lock.

Replay a retained failing input with the same target:

```sh
nix develop .#fuzz -c make jit-coverage-replay CG_TARGET=heap \
  CG_INPUT=target/jit-evidence/coverage-guided/heap/artifacts/crash-INPUT_HASH
```

`make jit-coverage-help` records the pinned tool's build, run and minimization
syntax. Keep the original input and log when reducing a finding. A replay is
not a new coverage campaign.

## Limits

AddressSanitizer and coverage instrumentation apply to compiled Rust/C++ code,
including runtime and compiler paths. Cranelift-generated machine code is not
automatically instrumented by either. A finite successful campaign establishes
only those observed inputs, not the absence of bugs, full GC/alias safety,
hostile-code security, or release-platform certification. Existing deterministic
admission/scalar/heap campaigns retain their separate replay and evidence scope.
