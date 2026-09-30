# Native JIT implementation plan

## Status and intent

- **Status:** implementation in progress on `feat/native-jit`; no release acceptance yet.
- **Planned against:** commit `47b34de`, Luna `0.5.1`, 2026-09-30.
- **Priority:** architecture/performance; correctness and execution control take precedence over speed.
- **Risk:** high; generated machine code introduces a new trusted, unsafe execution boundary.
- **Effort:** a substantial, plausibly multi-month compiler/runtime project. Estimates must be revised after the first integrated native slice is measured.
- **Requested artifact:** this root-level `PLAN_JIT.md`; no separate plan index is required.

Build a native JIT for Luna's existing runtime. Keep its language, Rust bindings, values, collector, stackless executor, and scheduling model. Frequently executed code should become native code without moving into another interpreter.

This is **not** a plan to embed the LuaJIT product, expose LuaJIT's FFI, or become compatible with LuaJIT native modules. LuaJIT has its own runtime and targets Lua 5.1 with extensions. Choosing that ecosystem instead requires a separately approved runtime migration, not an implementation shortcut inside this plan.

The interpreter remains the semantic reference and a permanent execution tier. Interpreter fallback is an intended correctness mechanism, not permission to call an arithmetic-only prototype a finished JIT.

### Executor instructions

1. Read this entire document and inspect the live code before editing.
2. Implement phases in dependency order. Update each phase's status and acceptance evidence in this file.
3. Do not mark a phase complete merely because compilation succeeds or a benchmark returns the right answer.
4. Use Makefile targets for builds, checks, tests, benchmarks, and fuzzing. Targets described as **proposed** below do not exist yet.
5. Use patch/edit tools, not Python, to modify files.
6. Code comments describe functionality only. Keep in-function comment lines at or below 20% of implementation lines. Design rationale belongs in this document.
7. Do not commit, push, or release without authorization. Authorized commits must be unsigned, title-only Conventional Commits, with a message no longer than 50 characters after the type/scope prefix.
8. If a STOP condition applies, report the evidence and required decision instead of weakening the contract.

### Drift check

```sh
git status --short
git diff --stat 47b34de..HEAD -- Cargo.toml Cargo.lock Makefile flake.nix workflows src tests examples util derive README.md COMPATIBILITY.md
```

Compare changed symbols with the current-state excerpts below. Preserve unrelated edits. A changed design assumption requires a plan update before implementation. Line numbers are routing aids, not immutable identifiers.

## 1. Non-negotiable product contract

| Area | Required behavior |
| --- | --- |
| Language | Preserve Luna's existing Lua 5.4 semantics, including distinct integers/floats, `_ENV`, `<const>`, `<close>`, metamethods, and proper tail calls. |
| Rust embedding | Existing `Lua`, `Context`, `Closure`, `Executor`, conversions, callbacks, userdata, stashed handles, and async bindings continue working. Configuration is additive. |
| Identity | Both tiers operate on the same objects. No serialization bridge or shadow object graph. |
| Scheduling | Compiled execution returns control through the existing executor. No unbounded compiled loop, hidden synchronous compilation inside `Executor::step`, or native-stack growth proportional to Lua recursion. |
| Collection | Preserve barriers, arena lifetime rules, weak references, finalization, resurrection, and host-controlled GC pacing. |
| Errors | Preserve catchable Lua errors, typed Rust causes, source positions, unwinding, and close-handler ordering. No hardware arithmetic trap substituted for a Lua error. |
| Debugging | Debug hooks and local/upvalue inspection remain semantically correct. Disabling native execution while hooks are active is acceptable. |
| Portability | Unsupported JIT platforms retain a working interpreter. Explicit configuration reports unavailable capabilities honestly. |
| Resource controls | Account for compiler queues, metadata, native code, and executable-memory overhead; bound admission and cache growth. |
| Verification | Demonstrate native execution, interpreter/native agreement, controlled suspension, and integrated GC correctness. |

Preserving these controls does not establish that a new JIT is safe for hostile code. Security claims must be reviewed and tested again. The current interpreter also has documented limits: its memory ceiling is checked between slices, fuel is approximate rather than a wall-clock timer, and crafted binary execution is not fully validated. Do not strengthen those claims accidentally.

## 2. Current state and code map

### Runtime and compilation

| File/symbol | Role and integration relevance |
| --- | --- |
| `src/lua.rs`: `Lua`, `Context`, `State` | `Lua` owns an arena, memory configuration, and GC scheduling. `Context<'gc>` exposes an active arena mutation. Host-side JIT ownership must not let branded references escape it. |
| `src/compiler/` | Source parser and bytecode compiler. Retain this language frontend. |
| `src/closure.rs`: `FunctionPrototype`, `Closure`, `UpValue` | Immutable prototype instructions/constants, source metadata, and closure state. Generated code must not conflate a prototype with a closure's mutable upvalues or environment. |
| `src/opcode.rs`: `Operation`, `OpCode` | Existing executable instruction set. The JIT frontend must handle every instruction through direct lowering, a runtime helper, or an explicit interpreter exit. |
| `src/value.rs`: `Value<'gc>` | Public Rust enum with integer, float, and GC-backed variants. It is not a stable machine-code ABI. |
| `src/constant.rs` | Numeric semantics, including exact integer/float comparisons and protected floor division/modulo. Use these rules as reference. |
| `src/thread/executor.rs`: `Executor::step` | Drives Lua, callbacks, sequences, errors, and futures; charges execution fuel. This is the tier-dispatch integration point. |
| `src/thread/vm.rs`: `run_vm` | Current instruction dispatch, hook checks, and frame transitions. Keep a reference implementation throughout development. |
| `src/thread/thread.rs`: `Frame`, `LuaFrame`, `LuaRegisters` | Canonical Lua frame state, stack borrowing, calls, returns, varargs, open upvalues, and close tracking. |
| `src/thread/close.rs`: `CloseSequence` | Drives `<close>` handlers through executor sequences, including error replacement. Reuse this machinery. |
| `src/callback.rs`, `src/async_callback.rs` | Callback and sequence protocols. Native execution must hand back control rather than bypass them. |
| `src/meta_ops.rs` | Metamethod and slow-path semantics; a primary source of shared runtime helpers. |

### GC, mutation, and compatibility

| File | Relevant responsibility |
| --- | --- |
| `src/table/table.rs`, `src/table/raw.rs` | Readonly/intercepting tables, canonical keys, array/hash storage, mutable borrows. |
| `src/table/weak.rs`, `src/finalizers.rs`, `src/registry.rs`, `src/stash.rs` | Weak references, finalization, rooted handles, and lifetime transitions. |
| `src/closure.rs`: `Closure::set_upvalue` | Explicit `Gc::write` barrier before changing upvalue references. |
| `src/dump.rs` | Luna bytecode signature/version, loader checks, and documented crafted-bytecode limitations. Native-code persistence is not part of this format. |
| `tests/fuel.rs`, `tests/reentrancy.rs`, `tests/async_foreign.rs` | Existing embedding/scheduling patterns to retain and extend. |
| `tests/numeric_semantics.rs`, `tests/vm_semantics.rs` | Numeric and VM regression oracles. |
| `tests/close_attribute.rs`, `tests/gc_*.rs`, `tests/weak_*.rs` | Close, collector, finalizer, weak-table, and pacing acceptance cases. |
| `tests/scripts.rs`, `tests/goldenscripts.rs` | Script corpus and captured output/error tests. |
| `tests/sandbox.rs`, `tests/hardening.rs` | Existing hostile-input and bounded-behavior coverage; not a complete JIT security audit. |
| `util/`, `derive/` | Existing serialization and conversion consumers; must not require a second binding ecosystem. |

### Load-bearing excerpts

`src/lua.rs:260` owns the collector, and `src/lua.rs:423` uses a higher-ranked lifetime boundary:

```rust
pub struct Lua {
    arena: Arena<Rootable![State<'_>]>,
    memory_limit: Option<usize>,
    gc_automatic: bool,
}

pub fn enter<F, T>(&mut self, f: F) -> T
where
    F: for<'gc> FnOnce(Context<'gc>) -> T,
```

`src/closure.rs:46` stores Luna's own bytecode and GC-managed constants:

```rust
pub struct FunctionPrototype<'gc> {
    pub chunk_name: String<'gc>,
    pub reference: FunctionRef<String<'gc>>,
    pub fixed_params: u8,
    pub has_varargs: bool,
    pub stack_size: u16,
    pub constants: boxed::Box<[Constant<String<'gc>>], MetricsAlloc<'gc>>,
    pub opcodes: boxed::Box<[OpCode], MetricsAlloc<'gc>>,
```

The excerpt is intentionally partial; source metadata, upvalues, nested prototypes, and locals follow.

`src/thread/executor.rs:566` dispatches a Lua slice and charges its reported instruction count:

```rust
match run_vm(ctx, lua_frame, Self::VM_GRANULARITY) {
```

```rust
Ok(instructions_run) => {
    fuel.consume(instructions_run.try_into().unwrap());
}
```

At this revision, `VM_GRANULARITY` is 64, callback fuel is 8, sequence-step fuel is 4, and executor-step fuel is 4. These are existing approximate accounting rules, not a proposed exact time guarantee.

`src/thread/vm.rs:117` advances the PC before performing the operation:

```rust
let op = current_prototype.opcodes[*registers.pc].decode();
*registers.pc += 1;
```

An interpreter bailout before instruction `p` must resume at `p`; a completed operation resumes at its actual successor. An error position currently derives from the already-advanced PC. Do not duplicate an instruction, a hook, or a side effect when crossing tiers.

`src/thread/thread.rs`: `Frame::Lua` records `bottom`, `closure`, `base`, `is_variable`, `pc`, `stack_size`, and `expected_return`. Open upvalues and to-be-closed values also live in thread state. A register snapshot alone is not a complete suspended execution state.

## 3. Architecture decision

### Chosen direction

Use a **region/function-oriented JIT with guarded specialization**, backed by Cranelift. Start with canonical frame slots and explicit executor exits; later retain scalar values in native registers between safe boundaries. Do not begin with cross-function tracing, inlining, or a replacement value representation.

Cranelift provides machine-code generation, not a Lua implementation or an automatically safe collector integration. Its exact crate version, Rust requirement, target support, relocation APIs, and executable-memory facilities must be verified and pinned in Phase 0.

```text
source -> existing Luna compiler -> existing Luna bytecode/prototypes
                                         |
                                  Executor::step
                                  /            \
                           run_vm               native region
                                  \            /
                              canonical Luna frames
                                         |
                         same objects, helpers, GC, callbacks

owned bytecode snapshot -> validated JIT IR -> bounded compile queue
                                               |
                                         native code image
                                               |
                                     owner-thread installation
```

### Alternatives not selected

- **LuaJIT via mlua:** useful for a separately chosen LuaJIT runtime, but cannot preserve this runtime's object model and execution contract transparently.
- **Lua 5.4-to-LuaJIT transpilation:** not a credible shortcut for integer semantics, environments, close handlers, GC, and Rust binding identity.
- **Custom assembly emitters first:** increases architecture and ABI maintenance before proving the runtime design.
- **LLVM first:** not prohibited forever, but not needed to establish the initial portable runtime/JIT boundary.
- **Full tracing/inlining first:** postpones GC, deoptimization, and stackless correctness behind a much larger optimization project.
- **Wasm translation first:** adds a separate execution substrate without solving the required runtime integration.

These are design choices, not claims that competing compilers can never perform better.

## 4. Internal design and invariants

### 4.1 Tier modes and additive configuration

Add an optional Cargo feature `jit`; keep `default = []`. Constructors remain interpreted by default in the first release. Embedders explicitly select `Off` or `Auto` through additive JIT configuration.

Proposed names are `JitConfig`, `JitMode`, `JitCapabilities`, `JitStats`, and `Lua::set_jit_config`. Final signatures are settled in Phase 1, before downstream-facing code is implemented. `Executor::step` and the conversion/callback interfaces must not change their public signatures.

Provide test-only forced scheduling through helpers in `tests/common/mod.rs`, not a production-wide environment switch. The helper may read `LUNA_TEST_JIT_MODE=off|auto|force`, explicitly configure each state, and force compilation/installation before stepping. `force` means compile admitted eligible regions regardless of hotness, not bypass guards or require every opcode to have a native fast path.

Unsupported environments report a capability/reason. Auto mode may stay interpreted; an explicit request to prepare native code reports unavailability. Never report successful native preparation when no executable code was installed.

The dispatcher receives `Context`, not `&mut Lua`, so the host/runtime connection must be designed explicitly. A suitable initial shape is a feature-gated, per-state `Rc` handle to a private non-GC JIT manager, shared by host-side `Lua` configuration/service code and arena `State`. That manager contains only owned, unbranded metadata and code entries; GC-bearing weak prototype registrations live in a separately traced arena sidecar. Verify the collector's `Collect` treatment for the static handle rather than suppressing tracing on arbitrary fields.

Cache lookup acquires a pinned owned code lease, then releases any manager interior borrow before entering generated code or runtime helpers. Reentrant execution must not encounter a long-lived `RefCell`/cache borrow held by the outer native invocation. Clearing/disabling the cache retires entries but cannot revoke an active lease. Keep manager fields and the lookup branch feature-gated so interpreter-only builds do not pay for this ownership machinery.

### 4.2 Canonical frame state and exits

The persistent source of truth remains `Frame::Lua` and the existing stack/upvalue/close state. The JIT may cache temporary scalars while executing, but must materialize all live interpreter-visible state at exits.

Specify an internal slice result distinguishing:

| Outcome | Required action |
| --- | --- |
| Continue/region boundary | Commit successor PC and consumed work; executor may dispatch the next region. |
| Interpret next instruction | Commit state before that instruction; execute it through `run_vm` without immediately re-entering the same failing region. |
| Budget exhausted/interrupted | Commit resumable state and return to the host through existing step semantics. |
| Call/return/tail call | Use existing frame-transition machinery; return to executor dispatch. |
| Yield/wait | Preserve existing coroutine/sequence modes; no suspended native stack. |
| Error | Preserve the original error and instruction position; use existing unwinding and close machinery. |

This table is a protocol requirement, not a Rust ABI declaration. Carry GC-bearing values through rooted Rust state, not arbitrary C-layout enum payloads.

No generated Lua call recursively enters another generated Lua function on the native stack. Phase 8 may remove dispatch overhead using bounded trampolines, but may not sacrifice this invariant.

### 4.3 Audited machine-code ABI

- Generated code receives an opaque invocation frame and a versioned table of trusted runtime entry points.
- Never emit loads/stores based on the unspecified layout of `Value`, `Gc`, `RefLock`, `Frame`, or a Rust trait object.
- Scalar exchange uses explicit fixed-width, defined-layout descriptors. Reference exchange initially uses checked canonical-slot indices and runtime helpers.
- The owning Rust wrapper retains required borrows during a slice and releases them before reentrant callbacks or frame mutations that require different borrows.
- Rust errors are stored in rooted runtime state and returned as status codes. Rust unwinding must not cross generated frames. Identify and handle panic paths at the Rust boundary; do not advertise recovery from aborts or arbitrary memory corruption.
- Review architecture calling conventions, stack alignment, helper clobbers, instruction-cache synchronization, and entry/exit safety contracts.
- Do not use `transmute` to extend `'gc` lifetimes or make `Lua` sendable.

### 4.4 GC ownership and safe boundaries

Collection currently happens outside arena mutation. The initial JIT does not collect while native code is executing under `Context<'gc>`.

Before leaving that context, all live references must be reachable through the existing traced frame/stack or explicitly traced sidecars. Temporary reference values in native registers cannot remain the only roots after an exit. Stack maps alone do not teach `gc-arena` how to trace new objects.

Initial stores go through existing barrier-aware helpers. Direct stores require a separate reviewed barrier/borrow proof before enabling them. Weak-table access must retain its existing upgrade/collection behavior; never treat weak references as permanent strong pointers.

GC requests, including finalizer-triggered work, return through the existing arena/slice boundary. Tests must cover GC after every exit, callbacks that reenter Lua, finalizer resurrection, open upvalues, and explicit/manual GC pacing.

### 4.5 Compilation ownership and resource admission

`Executor::step` must not synchronously invoke Cranelift. Hotness updates can enqueue bounded compilation requests; actual compilation occurs outside the arena and outside the execution slice.

Create a validated, owned compilation snapshot containing instructions, scalar literals, constant-slot references, control flow, and required source mappings. It contains no `Context`, `Value<'gc>`, `Gc`, closure pointer, or arena string reference. Strings are either runtime slot references or bounded owned bytes, never process-global GC pointers.

Hotness handling inside a slice queues only bounded identity/entry requests; it does not copy an entire prototype or construct its CFG there. The host service resolves a request through the traced weak sidecar and creates its size-limited snapshot in a separate arena entry outside `Executor::step`, then releases the arena before compiling. If the prototype has been collected or its registration changed, discard the request. Snapshot creation and result installation have their own admission/work limits and are not charged as secretly executed Lua bytecode.

Provide an explicit host preparation/service API. Automatic scheduling may use a bounded worker, but that worker only receives owned data and never owns or borrows `Lua`. Verify whether the pinned compiler can produce transferable code images; do not assume `JITModule` is `Send`. Installation and publication happen on the state's owning thread at a safe boundary.

Admission limits must include prototype size, IR blocks/instructions, outstanding requests, compilation attempts, compiler working memory where controllable, and emitted code size. Blacklist repeated compile failures per versioned region to avoid retry storms.

Structural admission bounds and a cooperative timeout do **not** provide a hard wall-clock bound on a compiler call or forcibly stop a running Rust thread. If hard compiler CPU/memory isolation is required for hostile input, use a supervised compiler process or explicitly require precompilation. Do not hide that limitation behind fuel accounting.

### 4.6 Code identity, lifetime, and cache

- Assign generation-safe identities through a private sidecar; do not add a mandatory public field to `FunctionPrototype` without evaluating API breakage.
- Do not key lifetime safety by raw allocation addresses: GC address reuse must not retrieve stale code.
- Separate prototype code identity from closure/upvalue/environment identity. Never bake a closure's mutable upvalues into shared code without guards and invalidation.
- Code-cache entries contain owned metadata and no untraced strong GC references. Weak identity registrations must be traced/cleaned through appropriate arena-owned state.
- Key installed code by prototype generation, region entry, specialization, target/features, and runtime ABI version.
- Pin executing code. Eviction, disablement, invalidation, and state destruction cannot unmap an active entry.
- Track requested and allocated executable bytes, relocation/unwind/debug metadata, snapshots, and queue memory. Distinguish accounting estimates from a hard process RSS ceiling.
- Native code is not added to `string.dump` or the Luna chunk format. Initial cache lifetime is one state/process; no loading native artifacts from scripts.
- Keep JIT code out of cached borrowed thread/frame references across mutations or reentrant calls.

### 4.7 Fuel and interruption

Native instruction counts are not Luna fuel. Charge work according to the logical bytecode path and existing helper/transition conventions.

1. Document current `run_vm` behavior, including instructions that exit before its trailing instruction increment.
2. Keep native slices within the reference VM's granularity. Budget checks occur before bounded regions and every loop backedge; long straight-line regions are split.
3. For insufficient remaining allowance, leave to the interpreter or use a shorter region. Do not skip execution while charging a whole region, or execute an unbounded region and charge afterward.
4. Count actual executed paths; eliminated/fused bytecode still has the agreed logical cost.
5. Do not charge both a bailout instruction and its interpreter retry, or remove callback/sequence/executor charges.
6. Honor interrupts before further script-visible work after the current permitted boundary.
7. Preserve `Fuel::with(0)` and minimal-progress semantics as characterized in Phase 0. Do not silently replace them with a new exact-budget API.

The scheduling contract is bounded virtual work with documented variable-cost helpers, not a real-time deadline or preemption of arbitrary host callbacks.

### 4.8 Specialization and invalidation

Use typed guards for integers/floats and explicit slow paths for coercions/metamethods. Initially materialize state and use the interpreter for complex instructions.

Table fast paths require guards for identity/layout, canonical key semantics, metatable behavior, readonly/intercept flags, weak modes, and resize/deletion/GC effects. A version counter alone is not sufficient if mutations to the metatable itself are untracked. Document dependencies and enumerate every Rust and Lua mutation path before adding inline caches.

Start with checked slot access and fresh validation at each entry. Do not retain an array/hash storage pointer across a callback, allocation, or any operation that can mutate it.

Disable unsafe assumptions under debug hooks. A callback that enables hooks must prevent the next native region from bypassing them. Locals, upvalues, and source PCs remain inspectable because exit state is canonical. Debug mutation invalidates affected assumptions even when the debug library itself executes outside native code.

### 4.9 Numeric rules

Preserve wrapping integer arithmetic, exact mixed integer/float comparison, modulo/floor division signs, zero-divisor errors, `i64::MIN / -1`, shift boundary behavior, float NaNs/infinities/signed zero, and existing string coercion rules.

Never enable unsafe floating-point reassociation or `fast-math` as a default optimization. Hardware division must be guarded against traps. A native value represented as `f64` is not a substitute for a Lua integer past `2^53`.

### 4.10 Bytecode provenance

The initial JIT compiles source-produced prototypes only. Loaded binary chunks remain interpreted unless they obtain a separately reviewed verifier proving the stronger control-flow/frame invariants needed by compilation.

Public prototype construction also needs an explicit trust/provenance policy; source provenance cannot be inferred from the chunk name, signature, or successful index checks. Use private registration with generation-safe identity. Unregistered prototypes fall back safely and report why.

Do not claim this makes crafted bytecode safe to execute in the existing interpreter. Keep its documented limitation visible. Native persistence and changing bytecode serialization are outside this plan.

## 5. Scope and proposed file organization

Only modify files needed by the active phase. New names below are proposed, not existing modules.

```text
src/jit/
  mod.rs          configuration, capabilities, statistics, dispatch facade
  abi.rs          defined-layout invocation/result descriptors
  runtime.rs      audited wrappers around canonical state operations
  ir.rs           owned validated compilation representation
  frontend.rs     bytecode/CFG analysis and region construction
  codegen.rs      Cranelift lowering and code-image production
  compiler.rs     admission, preparation, requests, and failure policy
  cache.rs        identities, executable entry ownership, pinning, eviction
  guards.rs       specialization assumptions and invalidation
  memory.rs       executable-memory protection/accounting, if required
```

Use established facilities from the pinned compiler where they satisfy the required contracts; do not duplicate its linker or allocator merely to populate this layout.

**Existing implementation files in scope:** `Cargo.toml`, `Cargo.lock`, `Makefile`, `flake.nix`, `src/lib.rs`, `src/lua.rs`, `src/closure.rs`, `src/opcode.rs`, `src/constant.rs`, `src/value.rs`, `src/fuel.rs`, `src/thread/`, `src/meta_ops.rs`, `src/table/`, `src/finalizers.rs`, `src/registry.rs`, `src/stash.rs`, and narrowly scoped `src/stdlib/debug.rs` integration where proven necessary.

**Verification/docs in scope:** `tests/`, a bounded `fuzz/` package added only in Phase 9, `examples/jit.rs`, `examples/jit_bench.rs`, `.github/workflows/tests.yml`, `README.md`, `COMPATIBILITY.md`, and this file. The former root `workflows/tests.yml` template was verified as non-active and promoted to GitHub's discovery directory; local workflow validation and remote run results remain separate evidence.

**Out of scope:** embedding LuaJIT/mlua, FFI/native module loading, a new language dialect, changing `Lua`'s thread-safety model, broad stdlib rewrites, general cleanup, a new GC, replacement public values, serialized machine code, and unrelated `util/` or `derive/` API changes. Existing workspace tests still cover those consumers.

Any scope expansion needs an explicit plan update and review.

## 6. Verification commands and environment

### Existing targets

| Command | Meaning | Expected result |
| --- | --- | --- |
| `make fmt-check` | Formatting check | Exit 0; no source changes. |
| `make check` | Workspace/all-targets typecheck | Exit 0. |
| `make check-all` | Same with all features | Exit 0. |
| `make test` | Workspace/all-targets tests plus doctests | All pass. |
| `make test-all` | Workspace/all-targets tests with all features | All pass; async and derive tests actually run. |
| `make rustdoc` | Docs with warnings denied | Exit 0. |
| `make verify` | Full existing local gate | Exit 0. |
| `make clippy` | Existing advisory lint lane | Record output; do not hide newly introduced JIT errors behind inherited lints. |

The planning session inspected these definitions but did not run the build/test baseline. Phase 0 must establish it. The observed ambient compiler was an older `1.88.0-nightly`; `workflows/tests.yml` specifies Rust `1.94.0`, and `flake.nix` provides a Rust-overlay toolchain with the musl target. Use the repository environment, for example `nix develop -c make verify`, if the ambient toolchain is unsuitable. Verify the actual chosen version rather than assuming the Nix shell matches the workflow pin.

### Proposed Makefile targets, created in Phase 1

All wrappers must propagate failures, use `$(CARGO)`, and work through the repository environment. None of these targets exists at plan creation.

| Target | Required behavior |
| --- | --- |
| `make jit-check` | Check workspace/all targets with `luna/jit`; accept `TARGET` for compile-only cross checks. |
| `make jit-test JIT_MODE=off` | Run existing integration/unit suites with JIT compiled but disabled. |
| `make jit-test JIT_MODE=auto` | Run the same suites with explicit Auto configuration. |
| `make jit-test JIT_MODE=force` | Run the same suites with forced preparation of eligible regions. |
| `make jit-test-all JIT_MODE=force` | Repeat with all optional features, including foreign-future/derive coverage. |
| `make jit-test-doc` | Run workspace doctests with `luna/jit`; native example must prove execution. |
| `make jit-rustdoc` | Build JIT API docs with warnings denied. |
| `make jit-verify` | Existing gate plus all three runtime modes, all-features native tests, and JIT docs. |
| `make jit-fuzz-smoke` | Run bounded fixed-seed verifier/differential fuzz smoke tests, failing on crash, timeout, or mismatch. |
| `make jit-fuzz FUZZ_TARGET=...` | Run a selected long fuzz campaign under explicit limits. |
| `make jit-bench` | Run checked release benchmarks with compiler settings/output recorded. |
| `make jit-bench-paired` | Alternate checked Off/Auto sample order with warmups and paired dispersion. |
| `make jit-performance` | Fail explicitly on missed frozen paired workload thresholds; other performance lanes remain separate. |
| `make jit-size` | Build matched interpreter/JIT-enabled artifacts and report their size deltas. |

Use package-qualified feature `luna/jit` in workspace wrappers. `jit-test` passes mode selection to test helpers; production constructors must not read that environment variable. Reject unknown mode strings. Wrap all optional-feature constructors used by integration tests, not only the script runner. Library unit tests that exercise execution need equivalent explicit mode coverage; doctests are separately tested and are not magically forced by this environment variable.

Configure the benchmark wrapper to use `CARGO_PROFILE_RELEASE_OPT_LEVEL=3` and record it, without globally replacing the existing size-oriented release profile. Compare interpreter and JIT within one benchmark binary wherever practical.

## 7. Phased implementation

Each phase has a correctness gate. Run `make jit-verify` after substantive changes once the proposed wrappers exist. Until Phase 1, use existing targets. Acceptance evidence must include commands, toolchain/target, test counts, native-execution counters where relevant, and any exclusions.

### Phase 0 — Establish the reference and prove backend feasibility

**Status:** IN PROGRESS. **Depends on:** none. Baseline, accounting probes, pinned backend, x86-64 helper/worker ownership probes, corpus and full GNU/musl x86-64 gates are recorded below. ARM64 native evidence and complete prerequisite review remain open.

**Files:** existing tests, `Makefile`, this document; disposable experiment artifacts under ignored `target/` or `/tmp` only.

1. Run the baseline in the repo-native toolchain. Record failures rather than treating documentation claims as current evidence.
2. Characterize instruction accounting, zero/negative fuel, interrupts, helper transitions, GC requests, and the 64-instruction VM slice. Add narrowly scoped regression tests if observations are not already covered.
3. Inventory every `Operation` variant and classify pure scalar work, reference/heap work, frame transitions, and variable-cost helpers. Maintain this inventory as machine-checkable lowering coverage later.
4. Audit caller-held borrows around `LuaFrame`, nested executors, and reentrant callbacks. Identify the precise ownership boundary for native invocation.
5. Evaluate a pinned Cranelift version against the actual toolchain and Linux x86-64/ARM64 targets. Prove scalar generation, helper calls, code-image ownership, executable protection, and destruction in a disposable experiment.
6. Record compiler/version/license dependencies and validate any proposed worker transfer API. Test instead of assuming a compiler module can cross threads.
7. Define the benchmark corpus and record interpreter results before tuning the JIT. Identify at least one representative application workload; absence remains an open acceptance item.

**Verify:** `make verify` and `make clippy`; baseline gate exits 0 or documented unrelated failures block progression. Backend feasibility is accepted only with an executed, inspected native helper-call experiment and a reviewed ownership/resource note. Expose any repeatable experiment through a Makefile target before relying on it as a project gate.

**Exit:** the architecture can support the ABI and lifetime model; no unsupported backend assumption remains hidden.

### Phase 1 — Add configuration, capabilities, and honest test lanes

**Status:** IN PROGRESS. **Depends on:** Phase 0. Optional configuration, capabilities, counters, Make gates, constructor wrappers, preparation/service APIs, and feature documentation exist. Platform/refusal and complete accounting acceptance remain open.

**Files:** manifests/lockfile, `Makefile`, `src/lib.rs`, `src/lua.rs`, `src/jit/mod.rs`, tests/common helpers, integration test constructors, `examples/jit.rs`.

1. Add optional JIT dependencies with explicit backend/version/features. Disabled builds must not pull the compiler into the runtime dependency graph.
2. Add additive configuration/capability/statistics APIs. Keep state non-`Send`; keep existing constructor behavior unchanged.
3. Add tests for unavailable targets, invalid config, enable/disable transitions, empty/core/full states, and independent state configuration.
4. Create the proposed check/test/doc/gate wrappers. Fuzz/benchmark/size targets can be added when their phases arrive; help output must distinguish them accurately.
5. Route all applicable integration-test state construction through explicit test helpers without changing library defaults. Initially forced mode can report that no eligible code exists; it must not claim native execution.
6. Record total dispatches, native entries/instructions, bailout reasons, compilation requests/failures, and code/cache bytes. Define counter meanings and avoid expensive logging on every opcode.

**Verify:** `make verify`, `make jit-check`, `make jit-test JIT_MODE=off`, `make jit-rustdoc`. All pass; compiled-but-disabled behavior matches baseline. Add a Makefile-backed dependency report proving optional compiler crates are absent when `jit` is disabled.

**Exit:** configuration and gates work; no statement of implemented acceleration yet.

### Phase 2 — Define and test the runtime boundary before optimizing

**Status:** IN PROGRESS. **Depends on:** Phase 1. Defined-layout scalar ABI and independent Rust slice model cover all emitted scalar entries/budgets. Complete transition mock/error/helper coverage remains open; the model was added after the first native experiment, so the original mock-before-emission ordering was not fully satisfied.

**Files:** `src/jit/abi.rs`, `runtime.rs`, `src/thread/executor.rs`, `thread.rs`, `vm.rs`, `tests/jit_boundary.rs`, `tests/jit_fuel.rs`.

1. Define entry/exit descriptors, canonical-state materialization, PC/error-position conventions, and ownership of error payloads.
2. Build a Rust reference implementation of the proposed slice protocol. Test it before emitting machine code.
3. Add a mock backend that exits before and after every tested instruction boundary. Ensure bailout forces interpreter progress rather than redispatching endlessly.
4. Exercise changes to stack base/size, pending results, varargs, open upvalues, close tracking, and nested callback borrows.
5. Review the future generated-code ABI, helper unwind policy, barrier access, and executable-code lifetime. Keep unsafe declarations localized and documented functionally.

**Verify:** `make jit-test JIT_MODE=off` and `make jit-test-all JIT_MODE=force`; boundary tests demonstrate equivalent results, PCs, errors, side-effect order, and bounded progress. The mock is labeled as a mock and does not increment real native-entry counters.

**Exit:** reference exits are correct; native compilation is not required for this phase.

### Phase 3 — Build validated owned IR and bounded code ownership

**Status:** IN PROGRESS. **Depends on:** Phase 2. Owned snapshots, exhaustive opcode validation/fallback classification, weak generation IDs, bounded queues/attempts, leased code, capped mappings and bounded unleased LRU eviction exist. Full CFG/effect review, complete accounting, combined-limit semantics, sparse-cache compaction and hardening remain open.

**Files:** `src/jit/ir.rs`, `frontend.rs`, `compiler.rs`, `cache.rs`, `memory.rs` if needed, `src/lua.rs`, `src/closure.rs`, `tests/jit_ir.rs`, `tests/jit_cache.rs`.

1. Build CFG/region analysis from decoded operations. Validate indices, reachable entries, successors, scalar types, helper effects, and exit snapshots.
2. Explicitly classify every opcode through an exhaustive match. Unknown/malformed input is refused; unsupported valid work exits to the interpreter.
3. Add private source-provenance registration and generation-safe code identities without breaking public prototype construction.
4. Produce owned snapshots without GC pointers. Bound snapshot, CFG, request, and specialization counts before expensive allocation.
5. Implement cache entry lifecycle, executable-memory protection, publication, pinning, invalidation, disablement, and eviction.
6. Add preparation/service APIs outside `Executor::step`. Leave automatic worker scheduling until its ownership model is demonstrated.
7. Charge JIT resources in a separate ledger and integrate enforcement with host memory configuration without changing the meaning of existing GC metrics silently.

**Verify:** `make jit-verify`; IR tests cover malformed control flow and admission refusal. Cache tests exercise address reuse/generation changes, two independent states, active-entry eviction, drop/disable, and quota exhaustion. Refusal leaves the original interpreter path usable.

**Exit:** validated snapshots, safe code ownership, and resource caps exist before native Lua work is executed.

### Phase 4 — Execute the first real native Lua slices

**Status:** IN PROGRESS. **Depends on:** Phase 3. Integrated scalar/loop native execution and exact fuel/guard tests pass. A finalized scalar/table-kernel disassembly lane now exists; helper and transition coverage remains partial, and full numeric/exit stress remains open.

**Files:** `src/jit/codegen.rs`, `runtime.rs`, executor dispatch, `examples/jit.rs`, `tests/jit_execution.rs`, `tests/jit_fuel.rs`.

1. Lower move/load, scalar guards, basic integer/float arithmetic, comparisons, branches, and bounded numeric loop regions.
2. Access GC-bearing values through helpers/canonical slots. Keep calls, allocations, metamethods, and complex frame changes on explicit interpreter exits.
3. Implement checked division/modulo/coercion or exit to the existing semantic path. No trap-prone shortcuts.
4. Charge logical work along actual paths and materialize state at every budget or guard exit.
5. Prove native code runs by asserting nonzero native entries and meaningful native logical-instruction counts in dedicated tests.
6. Add disassembly/debug artifacts for a small fixed program through a Makefile-backed diagnostic lane; do not rely solely on a timer improvement.

**Verify:** `make jit-verify`; dedicated tests pass for integer boundaries, exact mixed comparison, zero divisors, negative modulo, NaNs, branch paths, zero/small fuel, and interruption. Numeric-loop tests require actual native execution. Record native coverage and interpreted exits.

**Exit:** a correct integrated native slice exists. This is a prototype milestone, not product completion.

### Phase 5 — Preserve callbacks, coroutines, async, and unwinding

**Status:** IN PROGRESS. **Depends on:** Phase 4. Dedicated native heap tests cover reentrant callbacks, coroutine suspension, foreign futures, and close-handler error unwinding; the complete transition matrix remains open.

**Files:** runtime wrappers, thread executor/frame integration, `tests/jit_transitions.rs`, existing callback/reentrancy/async/close/error tests.

1. Accelerate work before/after calls while reusing canonical Lua call/return/tail-call transitions. Initially interpreting the transition instruction is acceptable.
2. Exercise compiled caller/interpreted callee, interpreted caller/compiled callee, and compiled caller/compiled callee.
3. Cover Rust callback sequences, nested executors reading/writing open upvalues, callbacks changing globals/metatables, and callback fuel interrupts.
4. Cover coroutine resume/yield/close and foreign futures. No live machine stack or borrowed invocation frame persists across suspension.
5. Preserve `<close>` on return, jumps, errors, coroutine close, and errors raised by the close handlers themselves.
6. Compare typed errors, caught errors, source positions, tail-recursion memory behavior, and result arity/nil handling.

**Verify:** `make jit-test-all JIT_MODE=force`; transition tests assert native execution on at least one side of each required boundary and canonical completion after it. Infinite/deep tail-call tests retain bounded step behavior and no proportional native-stack growth.

**Exit:** compiled execution is integrated with the embedding lifecycle, not just standalone numeric programs.

### Phase 6 — Add heap fast paths with collector and mutation proofs

**Status:** IN PROGRESS. **Depends on:** Phase 5. Fixed helper ABI v3 executes table/upvalue/allocation operations through existing barrier APIs. Weak tables, readonly/intercept changes, invalid keys, every-slice GC, finalizer resurrection and debug mutation have native evidence; broader weak-mode/interleaved/invalidation stress remains open.

**Files:** runtime/guards, table modules, closure/upvalue integration, `tests/jit_gc.rs`, `tests/jit_mutation.rs`, existing GC/weak/userdata suites.

1. Add safe native fast paths for common table lookup/update and upvalue operations, beginning with barrier-aware helpers.
2. Document all invalidation dependencies: table storage, metatables and their contents, readonly/intercept flags, weak-mode changes, Lua mutation, and Rust mutation.
3. Add private generations or guarded fresh checks only where their complete mutation coverage is proven. Do not add public layout/API breakage merely for cache convenience.
4. Stress GC immediately after every kind of native exit, tiny slices, allocation pressure, weak references, finalizer resurrection, and code-cache eviction.
5. Verify debug-local/upvalue mutation, userdata behavior, and interleaved executors cannot leave stale reference/scalar assumptions.
6. Only consider direct storage access after helper-backed correctness passes and its barrier/borrow/invalidation proof is reviewed.

**Verify:** `make jit-verify`; GC/mutation tests run in Off and Force and assert native fast-path entry where claimed. Cross-state/generation reuse never retrieves stale code. Readonly/intercepting/weak tables retain their exact observed behavior.

**Exit:** optimized heap access is compatible with actual Luna GC and embedding mutations.

### Phase 7 — Add hotness policy and nonblocking automatic compilation

**Status:** IN PROGRESS. **Depends on:** Phases 3 and 6. Hotness/bounded queue/explicit outside-arena service, bounded failed attempts, configuration retirement, typed refusal and bounded LRU retry with diagnostics are implemented. Scheduling failure injection, broader backoff/compaction policy and complete resource ledgers remain open.

**Files:** compiler/cache policy, Lua host service APIs, `tests/jit_policy.rs`, `tests/jit_resources.rs`, benchmark harness.

1. Add saturating per-region hotness, promotion thresholds, failed-region backoff, and specialization limits.
2. Auto mode queues work without compiling in `Executor::step`. Preparation/service or owned-data workers compile outside the arena; install only between slices.
3. Test slow compilation, queue saturation, state destruction with pending jobs, enable/disable races in scheduling, and late/stale results.
4. Bound installation work, relocation count, and code size. Reject results exceeding admission/capability limits rather than blocking a slice unpredictably.
5. Expose fallback and compilation refusal reasons to the host. A failed optional compile is not a new Lua language error.
6. Prove short-lived scripts do not repeatedly compile, and two states do not share mutable compilation state accidentally.

**Verify:** `make jit-test JIT_MODE=auto`, `make jit-test-all JIT_MODE=force`, `make jit-verify`. A deliberately blocked compiler never blocks manual stepping; another executor continues making progress. Auto-policy tests eventually execute native code on admitted hot workloads and stay within configured queue/cache limits.

**Exit:** Auto is an integrated execution policy with measured scheduling behavior.

### Phase 8 — Optimize without weakening semantics

**Status:** IN PROGRESS, NOT ACCEPTED. **Depends on:** Phase 7 and a passing performance baseline. Initialized-prefix scratch and operand-scoped helper synchronization have correctness gates; paired performance checks are implemented. Release thresholds and the phase exit remain unmet; broader optimization must not be described as a passed baseline.

**Files:** JIT IR/codegen/guards/cache, bounded runtime helpers, optimization tests and benchmarks.

1. Promote temporary scalars to SSA/native registers between canonical-state boundaries.
2. Add constant folding, redundant scalar-guard elimination, checked inline caches, and region chaining where measurements justify them.
3. Ensure folded/eliminated work retains agreed logical fuel cost and error/side-effect ordering.
4. Expand profitable instruction coverage. Maintain a lowering matrix and report execution coverage by workload, not merely opcode count.
5. Add exact deoptimization maps for every optimized exit. Bound polymorphism and invalidate safely after mutations/debug changes.
6. Consider OSR entry at hot loop headers using canonical frame slots. Validate entry contracts and any versioned assumptions before executing.
7. Do not add recursive native Lua calls or speculative cross-callback memory assumptions. Cross-function inlining/tracing remains a separately reviewed extension.

**Verify:** `make jit-verify`, `make jit-fuzz-smoke` once available, `make jit-bench`. Each optimization has differential/forced-exit tests and records its measured effect; remove or disable regressions rather than explaining them away.

**Exit:** useful application-path speedups, not just more generated machine code.

### Phase 9 — Harden and establish supported-platform evidence

**Status:** IN PROGRESS. **Depends on:** Phase 8; introduce fuzzing earlier as soon as Phase 3/4 provide targets. Seeded admission/scalar campaigns execute in supervised limited child processes with tested panic/signal/timeout propagation; allocation/protection denial and full GNU/musl x86-64 gates pass. The full heap/lifecycle corpus, coverage-guided campaigns, CPU-feature mismatch, remaining refusal/eviction cases, unsafe/Miri review and ARM64/hosted evidence remain open.

**Files:** `fuzz/`, fuzz/gate targets, JIT validation/runtime tests, workflow/environment configuration, security documentation.

1. Fuzz validated IR construction and code generation separately from execution. Reject malformed IR before unsafe code entry.
2. Differentially execute bounded valid programs in fresh Off/Force states with the same inputs; compare values, ordered side effects, catchable errors, and reference-work bounds.
3. Generate mutations around guards, callbacks, close operations, weak tables, yields, arithmetic edges, and compiled/interpreted transitions.
4. Run crash-prone native fuzz executions in disposable supervised processes with time/memory limits. A timeout, signal, or corrupted result is a test failure.
5. Test executable-memory allocation failure, protection failure, CPU feature mismatch, quota exhaustion, cache eviction, and compiler refusal.
6. Review all new unsafe blocks, helper boundaries, borrow assumptions, W^X transitions, code reclamation, and error handling. Use Miri for compatible Rust-only components, not as claimed validation of generated machine code.
7. Execute native suites on Linux x86-64 GNU, x86-64 musl, and ARM64. Cross-compilation alone does not establish native runtime correctness. Unsupported/permission-denied environments exercise interpreter fallback.
8. Record compiler crate versions and a security-update policy. Preserve the explicit untrusted-binary policy.

**Verify:** `make jit-verify`, `make jit-fuzz-smoke`, and the same native gate on every declared release platform. Long `make jit-fuzz FUZZ_TARGET=...` campaigns produce an evidence record with seeds/corpus, duration, limits, and findings; no finite campaign is described as proof of absence of bugs.

**Exit:** runtime safety review and platform-specific integrated evidence are complete. Missing platform evidence is a release blocker for that platform, not a reason to claim universal support.

### Phase 10 — Publish a complete, accurately documented feature

**Status:** IN PROGRESS, NOT ACCEPTED. **Depends on:** Phase 9 and approved workload acceptance. Active native workflow wiring and a prepared example exist; executed hosted/ARM64 results, complete hardening, frozen performance controls and shipping/size reporting remain open.

**Files:** docs/examples, benchmark/size tooling, actual CI integration, this document.

1. Document enabling/configuring JIT, host compilation service/worker behavior, capabilities, metrics, resource ceilings, fallback reasons, and debug policy.
2. Publish measured cold/warm/steady-state results, compilation cost, cache memory, scheduling behavior, and matched binary-size deltas.
3. Add a working example that compiles an eligible function, executes it, and verifies native counters alongside its result.
4. Run the original corpus under all modes; document legitimate exclusions precisely. Doctests, derive, and foreign-future examples remain covered.
5. Integrate the full native gate into the workflow that actually runs in CI. Verify workflow wiring, feature matrix, toolchains, and target runners.
6. Keep JIT optional and runtime opt-in. Changing constructor defaults to Auto requires a separately reviewed evidence-based decision.
7. Update the status/evidence ledger below. Do not mark the overall plan complete while representative workloads, native platforms, or hardening items remain unverified.

**Verify:** `make jit-verify`, `make jit-fuzz-smoke`, `make jit-bench`, `make jit-size`, and actual native-platform CI results. Documentation examples execute successfully; performance and safety claims match recorded evidence.

**Exit:** the supported JIT feature is complete under the acceptance contract below.

## 8. Detailed test matrix

| Category | Required cases and evidence |
| --- | --- |
| Scalar execution | Every lowered opcode/path; wrapping boundaries; exact integer/float comparison; NaN, infinities, signed zero; shifts; zero divisors; negative floor division/modulo; coercions. |
| Control flow | Branches, forward/backward jumps, loops, live locals across exits, varargs, nil-sensitive return counts, closures sharing upvalues, `_ENV` mutation. |
| Transition correctness | Exit before/after each effectful instruction; compiled/interpreted call combinations; no repeated store/hook/callback after bailout; guard-failure forward progress. |
| Fuel | Zero/negative/tiny/large fuel, refill, interrupt from callback, bounded native backedges, helper/transition accounting, repeated resume after every boundary. |
| Stackless lifecycle | Deep calls under configured depth, unbounded tail calls with bounded slices, coroutine yield/resume/close, nested executors, pending futures. |
| Errors and cleanup | `pcall`/`xpcall`, source positions, typed Rust errors, callback errors, `<close>` order and replacement errors, finalizer failures. |
| Debug | Hook activation/deactivation mid-run, count/line behavior, locals and upvalues read/write, upvalue joining, traceback across tier boundaries. |
| Mutation | Globals/metatables changed from Rust/Lua, resizing/deletion, readonly/intercept flags, weak-mode changes, unrelated state mutations. |
| GC | Force collection after all exits, open/closed upvalues, weak keys/values, finalization/resurrection, manual pacing, dropped stashed handles, collected prototypes. |
| Ownership/resources | Active code eviction, state drop with queued work, generation/address reuse, allocation/protection failure, limits, retry storms, denied JIT capability. |
| Binary policy | Source-produced provenance versus dumped/loaded/manually constructed prototypes; loaded/unregistered chunks never compile silently. |
| Workspace compatibility | Existing examples, serde utilities, conversions/derive, docs, optional async. |

Every native-specific test asserts actual native execution for the operation/path it claims to test. A passing Force suite that merely bails out to the interpreter everywhere is not acceptance. Legitimately interpreted operations report their reason and exercise native code on the surrounding eligible path where applicable.

Differential comparison must not compare raw addresses, assume a particular `pairs` order, or serialize away object identity relationships that matter. Compare ordered side effects explicitly; compare unordered table contents canonically only where the program contract permits it. Use controlled seeded inputs, not backend-specific PRNG output, as a language oracle.

## 9. Performance acceptance and measurement

Build a deterministic harness under `examples/jit_bench.rs` with wrappers in the Makefile. The same binary runs Off, explicit preparation/Force, and Auto for comparable work; each timed mode verifies its results.

Required workloads:

1. Integer and floating numeric loops, including mixed-type exits.
2. Array/hash table reads and writes with stable and changing shapes.
3. Closure/upvalue loops, calls, and proper tail calls.
4. Metamethod-heavy and polymorphic code, including intentionally poor JIT candidates.
5. Rust callback-heavy code and coroutine/async workloads.
6. Allocation/GC-heavy programs and native cache churn.
7. Cold one-shot configuration-like scripts.
8. At least one real consuming application's workload. The maintainer delegated selection; use Oslo's actual compile-once/per-row `free < 1e9` predicate as the initial consumer-derived case. A standalone reproduction is not end-to-end Oslo migration evidence.

Report compiler/version/features, target and CPU features, profile settings, sample counts and dispersion, cold compile time, time-to-native, steady-state execution, fallback counts, compiled coverage, code/cache/queue memory, binary size, and maximum observed slice work/latency. Record the full command and output artifact under ignored `target/jit-evidence/`.

Do not promise LuaJIT-equivalent performance. That is an empirical comparison, not an architectural consequence. Optional comparison with LuaJIT is restricted to the compatible language subset and identical workload results; label differences clearly.

The delegated initial thresholds are frozen in Section 14: 2x integer/float, 1.25x stable table/upvalue, at most 20% mixed-workload regression, 15% cold overhead and 5% compiled-but-Off overhead. `make jit-performance` enforces the measured paired controls; Oslo remains observational without a separately frozen numerical threshold. Do not relax these controls after seeing failures. Shipping-profile, compiled-disabled, size, memory and platform evidence remain required independently. The initial feasibility gate is a repeatable meaningful speedup on at least one integrated hot workload with all correctness controls enabled.

The size-oriented existing `opt-level = "s"` can influence interpreter layout. Measure at explicit `opt-level = 3` for performance attribution and separately publish shipping-profile numbers. Do not silently trade away the existing binary-size policy.

## 10. Final done criteria

All applicable criteria must be checked with evidence; no partial phase substitutes for completion.

- [ ] Existing API and Lua 5.4 regression behavior remain compatible.
- [ ] `make verify` succeeds without JIT enabled by default.
- [ ] `make jit-verify` succeeds in Off, Auto, and Force, including optional async/derive and doctests.
- [ ] Eligible integrated workloads actually execute native instructions; counters and coverage substantiate this.
- [ ] No proportional native-stack growth from Lua recursion/tail calls or suspension.
- [ ] Small-fuel/interrupt/GC-request tests preserve reference scheduling behavior.
- [ ] Callbacks, reentrancy, coroutines, async futures, close handlers, and errors pass mixed-tier tests.
- [ ] GC roots, barriers, weak references, finalizers, and code lifetimes pass integrated stress tests and review.
- [ ] No synchronous compilation occurs inside `Executor::step`.
- [ ] Queue/cache/native-memory limits, compiler failure/backoff, and active-entry eviction are tested.
- [ ] Debug hooks and debug mutation retain correct behavior.
- [ ] Binary/prototype provenance policy is enforced and documented.
- [ ] Executable memory is never deliberately mapped writable and executable simultaneously; allocation/protection failure falls back or reports capability failure safely.
- [ ] `make jit-fuzz-smoke` passes; longer campaign evidence and unsafe-boundary review are recorded.
- [ ] Native platform tests actually execute on each advertised release target.
- [ ] Unsupported or denied-JIT environments preserve interpreter functionality.
- [ ] `make jit-bench` meets preapproved numerical workload thresholds with controls enabled.
- [ ] `make jit-size` records interpreter/JIT feature costs and compiled-but-disabled overhead.
- [ ] Docs, examples, resource/security caveats, and actual CI wiring match behavior.
- [ ] All deviations/exclusions are approved and recorded; phase/evidence statuses are accurate.
- [ ] `git diff --check` passes and changes stay within the phase's approved scope.

## 11. STOP conditions

Stop and report before proceeding if:

1. Preserving the design appears to require swapping in LuaJIT, changing the language version, replacing the GC, or breaking the public embedding API.
2. The selected compiler/toolchain cannot implement the entry/exit, relocation, executable-memory, or owned compilation-result model.
3. Implementation requires extending `'gc`, moving `Lua` to a worker, or retaining unrooted arena pointers in installed code/queues.
4. A compiled path bypasses barriers, readonly/intercept rules, weak-reference behavior, or reentrancy borrow invariants.
5. A fallback repeats/skips effects, loses frame/close state, or makes no progress indefinitely.
6. Native loops exceed the reference work bound, ignore interrupts, or compilation blocks a manual execution slice.
7. Errors unwind through generated frames unexpectedly, become hardware traps, or lose catchable/typed semantics.
8. Cache identity/address reuse, eviction, or state destruction can expose stale or unmapped code.
9. Fuel or memory claims require guarantees the actual implementation cannot deliver.
10. Native tests pass without proving native execution on the claimed paths.
11. A reproducible differential mismatch, process signal/crash, hang, or unsafe-code finding appears. Fix and add a regression before adding optimizations.
12. Source/IR/bytecode trust assumptions differ from this plan, or an unregistered prototype enters native compilation.
13. A step requires unapproved scope expansion or changes an agreed performance threshold.

Do not disable tests, lower safety guarantees, catch arbitrary crashes as successful fallback, or rename a partial prototype to satisfy the completion checklist.

## 12. Maintenance and review rules

- New opcodes must update the exhaustive JIT classification, effect model, PC/fuel semantics, and differential tests. Safe interpreter fallback remains available.
- New table/upvalue/debug mutation APIs must update guard invalidation proofs before enabling caches that depend on them.
- Changes to frame layout, close tracking, error positions, GC barriers, or executor accounting require mixed-tier regression review.
- ABI/cache version changes invalidate installed artifacts. Never reuse incompatible code because its source hash happens to match.
- Compiler upgrades require full correctness/native-platform gates, fuzz smoke, and representative benchmarks; pin dependencies reproducibly.
- New native targets need executed platform evidence, not only successful compilation.
- Security docs must distinguish source execution, crafted bytecode, compiler isolation, variable-cost host callbacks, GC memory, and native/compiler memory.
- Reviewers inspect every new unsafe block and the invariants at both sides of each machine-code boundary.
- Background profiling, compiler workers, cache sharing, native persistence, tracing, or cross-function inlining are separately reviewed extensions, not implicit follow-ups.

## 13. Evidence ledger

| Phase | Status | Required evidence | Recorded result |
| --- | --- | --- | --- |
| 0: reference/backend feasibility | IN PROGRESS | Baseline gates, accounting characterization, executed ABI experiment, pinned backend decision | Nix `make verify`, fuel probes, RX helper call and worker-transfer probes passed on x86-64 Linux; Cranelift 0.136.1/Rust 1.97.1 pinned. `make clippy` still fails on two inherited `never_loop` errors in `src/meta_ops.rs` (139 warnings). Other native targets not executed. |
| 1: configuration/gates | IN PROGRESS | Optional dependency isolation, capability tests, explicit mode wrappers | Optional dependency tree checked without compiler crates; Off constructors and explicit config tests pass. Force wrappers now prepare outside arena entries; same-entry execution is disclosed rather than falsely claimed forced. |
| 2: runtime boundary | IN PROGRESS | PC/effect/fuel/rooted-state mock/reference tests | `make jit-boundary`: three unit tests passed, including per-PC/per-budget Rust-model agreement and pinned-entry retirement. `make jit-native`: nine integrated tests passed, including small/interrupted fuel and side-effect-preserving guard bailout with GC between slices. Complete transition mock coverage pending. |
| 3: IR/code ownership | IN PROGRESS | Verifier/admission/cache/lifetime/resource tests | Backend admission revalidation and malformed refusal pass. Shared fallible allocators charge registry/tracking/code-index/queue/preparation/snapshot and persistent backend entry/mapping-record layouts, including retained capacity and transient growth. Refusal, allocation/protection denial, partial cleanup and lower-limit retirement/lease tests pass. Fixed-owner/compiler and combined-host accounting remain incomplete. |
| 4: native slices | IN PROGRESS | Actual native counters, numeric/fuel correctness | Explicit example returned 5000050000 with 200007 native logical instructions; nine native tests passed. Scalar operations, numeric loops, guarded comparison, and interpreter fallback integrated. Helper-backed heap operations now execute natively; broader numeric/error coverage remains open. |
| 5: lifecycle integration | IN PROGRESS | Mixed-tier callbacks, async/coroutines, errors and close tests | Eleven dedicated heap tests (twelve with async) cover reentrancy, coroutine/foreign suspension, close/error unwinding, panic materialization, debug mutation and finalizer resurrection. Full-feature Force passes; complete mixed-tier transition matrix remains open. |
| 6: heap/GC integration | IN PROGRESS | Native heap paths, barriers, GC/mutation/invalidation stress | Fresh helper guards preserve weak/readonly/intercept/invalid-key behavior. Every-slice GC, open/closed upvalues, pending-scalar panic inspection, debug local/upvalue join and finalizer-only native upvalue writes pass. Broader interleaved executors, mode mutations and exhaustive guard coverage remain open. |
| 7: Auto policy | IN PROGRESS | Nonblocking stepping, owned compile work, limits/backoff, hot promotion | Bounded hot requests and explicit outside-arena service; Off/quota-shrink retirement, queue/attempt reductions, typed quota refusal and exhausted-attempt reset tests pass. Eviction, injected blocked compiler and full diagnostics/resource ledger remain open. |
| 8: measured optimization | IN PROGRESS | Differential exits, coverage, approved workload performance | Slice-local leases, fixed helper ABI v3, operand-scoped synchronization and outlined tiered scratch pass full correctness and every-tier reference tests. Rust assembly verifies the VM scratch probe was removed. Opt-level-3 loop controls pass but four mixed/heap controls fail. Repeated shipping and matched feature-size/Off-cost lanes now exist; the latter fails frozen 5% controls at both profiles. Perf is permission-denied; profitable mixed-path optimization, cold compile/latency and broader fuzz evidence remain open. |
| 9: hardening/platforms | IN PROGRESS | Fuzz artifacts, unsafe review, native target executions | Admission/scalar campaigns execute in limited supervised child processes; panic/signal/timeout failures are tested, and workers verify inherited limits. Five-seed 5120-kernel campaign passes with exact exit/slot/reclamation comparisons. Full GNU/musl x86-64 gates and prepared examples execute natively; allocation/protection denial is injected and tested. Heap/lifecycle and coverage-guided fuzz, complete unsafe/Miri review and native ARM64/hosted evidence remain open. |
| 10: release acceptance | IN PROGRESS | Complete gates, thresholds, docs/examples, actual CI | Prepared example and resource/security documentation exist. Active workflow wiring runs full GNU/musl x86-64 and GNU ARM64 gates, builds matched shipping artifacts and uploads evidence. Workflow lint/local musl integration pass; repeated local shipping/size/disabled-cost evidence is recorded. Actual hosted/ARM64 results, complete hardening and both native/disabled performance acceptance remain missing. |

Status values: TODO, IN PROGRESS, COMPLETE, or BLOCKED with a concrete reason. Attach toolchain, platform, commands, counts, exclusions, and evidence paths when updating a row. COMPLETE requires the stated phase exit, not a percentage estimate.

## 14. Open decisions to settle before their dependent phases

### Implementation decisions and measured foundation (2026-09-30)

- Pinned Cranelift `0.136.1` requires Rust 1.96; the repository Nix environment provides Rust/Cargo 1.97.1. The optional compiler is target-gated to Linux x86-64/aarch64; native release coverage remains unproven on ARM64.
- Host and arena share a `#[collect(require_static)]` `Rc<RefCell<Manager>>`; the manager may contain only owned, unbranded data. Weak source-prototype registrations belong to a separately traced arena sidecar. A code lease releases the manager borrow before invocation.
- Initial host APIs will be `prepare_jit` (bounded source preparation), `service_jit` (bounded queued work), and `clear_jit_cache`. Compilation occurs after snapshot arena mutation returns, never in `Executor::step` or implicitly in `Lua::enter`. Convenience execution can service between slices; manual executors explicitly call the service.
- Initial machine ABI exchanges `#[repr(C)]` scalar slots (tag/bits), entry PC, bounded instruction budget, and a defined-layout exit. GC references remain in canonical, traced Lua registers and never appear in generated code or snapshots. Unsupported operations and failed guards resume before the instruction in the same reference slice.
- Baseline `make verify` and foundation `make jit-verify` both passed in the Nix environment. The latter currently proves configuration-mode regressions, not native Lua execution. Component tests execute RX, non-writable code, call a Rust helper, transfer worker-owned code to the invoking thread, and explicitly reclaim its memory.
- Fuel probes establish that an endless loop runs 64 VM instructions plus 4 executor fuel even with zero/negative/interrupted input. `return 42` consumes 10 fuel (load, return transition, returned value, executor step). Preserve these actual semantics.
- The maintainer delegated workload and threshold selection: do not request application choices again. Oslo's compile-once/per-row `free < 1e9` predicate is a real consumer-derived case; its current dependency remains the older git tag, so this is not a claim that Oslo itself was migrated or benchmarked end to end.
- Freeze initial performance targets before optimization: 2x median speedup on integer/float hot loops, 1.25x on stable table/upvalue cases, no more than 20% median regression on callback/metamethod/GC cases, 15% cold overhead, and 5% compiled-but-Off overhead. Compare at explicit opt-level 3 with repeated interleaved runs; publish dispersion and shipping-profile results separately. These are demanding acceptance targets, not achieved results or permission to relax controls.
- Interpreter-only opt-level-3 baseline (`make jit-bench ARGS='--mode off --samples 11'`): integer loop 229302 ns, float loop 747350 ns, array table 234610 ns, closure/upvalue 648560 ns, metamethod 259472 ns, callbacks 301930 ns, allocation/GC 224468 ns, Oslo predicate 2699680 ns, cold config 87178 ns (medians). All native counters were zero. Repeated paired measurements are still required.
- Foundation logs: `/tmp/luna-jit-baseline.log`, `/tmp/luna-jit-phase0.log`, `/tmp/luna-jit-off.log`, `/tmp/luna-jit-force-config.log`, `/tmp/luna-jit-foundation-verify.log`, `/tmp/luna-jit-interpreter-bench.log`; collect them under ignored `target/jit-evidence/` with `make jit-evidence`.
- Integrated scalar gate: `nix develop -c make jit-verify` exited 0 (`/tmp/luna-jit-integrated-verify.log`), 2218 passed test executions over 367 suite invocations, including repeated modes/docs rather than unique tests. The full-feature prepared Force suite also exited 0 (`/tmp/luna-jit-prepared-force.log`). A final gate after the latest documentation/counter changes is still required.
- `make jit-example jit-boundary jit-native` exited 0 (`/tmp/luna-jit-scalar-final.log`): example result 5000050000, 200007 native instructions, 40960 mapped bytes across six prepared prototypes; three boundary/model tests and nine integration tests passed. `Lua::core()` itself registers five source-defined standard-library prototypes, so lifetime/provenance isolation tests use `Lua::empty()` rather than assuming the core starts with no registrations.
- The independent scalar boundary model was introduced after the first native experiment. It validates every entry PC against varied budgets/tags/extreme numeric inputs, but does not replace the planned full frame-transition mock or external unsafe review. `JIT.md` records the current unsafe boundary and all resource caveats.
- First Auto scalar measurements at opt-level 3 (`/tmp/luna-jit-native-bench.log`, 11 samples) showed medians: integer 197082 ns, float 211418 ns, table 878393 ns, closure/upvalue 4276062 ns, metamethod 912430 ns, callbacks 1091370 ns, allocation/GC 609311 ns, Oslo predicate 4959090 ns, cold config 86599 ns. These are separate runs, not paired acceptance data. The float loop improved, but most mixed/heap cases regressed substantially: performance acceptance is **not passed**. A subsequent static unsupported-entry bypass avoids invoking machine code merely to exit at an opcode known to be interpreted; remeasurement is pending.
- Cranelift compilation now explicitly enables `opt_level=speed` and its verifier. Logical instruction budgets, guard exits, wrapping integers, and fresh per-invocation scalar materialization are retained. Unsupported-entry skipping is a dispatch optimization, not native coverage for the skipped operations.
- No compiler work runs inside `Executor::step` or ordinary `Lua::enter`. Convenience finish/async finish service one queued prototype between slices; manual hosts use explicit preparation/service. No worker, hard compiler timeout, or persisted native cache has been implemented. Heap helpers are implemented in the following stage; user callbacks still run through the interpreter transitions.
- Final scalar-stage validation after the counter/docs/example changes: `nix develop -c make fmt jit-verify` exited 0 (`/tmp/luna-jit-scalar-verify.log`), 2218 passed executions over 372 suite invocations (not unique test counts). `git diff --check` passed. The branch remains `feat/native-jit`; no commits or release have been made. The current feature is explicitly documented as an experimental scalar tier, not completion of this plan.
- Repeated Auto measurements after static unsupported-entry skipping and corrected slice diagnostics (`/tmp/luna-jit-scalar-auto-bench.log`) still miss mixed-workload acceptance: integer 164545 ns (121987-202086 ns range), float 117652 ns, table 855123 ns, closure/upvalue 3231499 ns, metamethod 566824 ns, callbacks 1057779 ns, allocation/GC 473632 ns, Oslo 4356620 ns, cold config 85684 ns (11-sample medians). All warm cases report real native instructions and interpreter/guard counts; cold Auto requests no compilation. These remain unpaired runs, not release performance evidence.

### Implementation decisions and next handoff

#### Outlined tiered scratch decision

**Post-gate repeated timing:** standalone `nix develop -c make jit-performance` exits 2 because the same four controls fail (`/tmp/luna-jit-lease-v3-tier-repeat-performance.log`): integer 2.6476, float 4.8486, array 1.1631, upvalue 0.5849, metamethod 0.7793, callbacks 0.7242, allocation 0.9988, Oslo 0.8481, cold 0.9999. All cases verify results and real warm native coverage; cold Auto still makes no compile request. Current loop/stack correctness evidence does not replace the failing application/heap controls, shipping/disabled-feature/size lanes or remaining full-plan work.

**Complete gate and final assembly:** `nix develop -c make fmt jit-verify jit-rust-assembly` exited 0 (`/tmp/luna-jit-lease-v3-tier-final-verify.log`): 2358 passed test executions over 390 suite invocations, including repeated modes/docs rather than unique tests. All six outlined bodies are captured. Their stack reservations are `0x118/0x198/0x298/0x498/0x898/(0x1000+0x98)` for capacities 8/16/32/64/128/256; only the maximum tier contains the scratch probe page. The VM itself reserves `0x648`, not the prior `0x1628`. `git diff --check` passes. Separate paired timing is repeated after this gate; no sampling permission or kernel configuration was changed.

**First tiered timing:** standalone `make jit-performance` still fails four controls (`/tmp/luna-jit-tiered-performance.log`): integer 2.4886, float 4.7341, array 1.1320, upvalue 0.5841, metamethod 0.8022, callbacks 0.7083, allocation 0.9895, Oslo 0.8423, cold 0.9948. Stack-boundary/layout correctness is verified, but tiering alone does not establish the missing application gains. The Rust ASM selector was corrected to the actual demangled `<luna::jit::Runtime>::invoke::<N>` names; repeat extraction/full gates and retain separate timing evidence. No performance threshold is waived.

**Boundary/assembly evidence:** `make jit-native jit-boundary` passed after adding reference-return programs at register prefixes 8/9, 16/17, 32/33, 64/65, 128/129 and 255/256, each compared with Off and followed by collection (`/tmp/luna-jit-tier-boundaries.log`, twelve native integration and ten combined unit tests). Symbol-retained assembly now shows `run_vm` reserving `0x648` bytes without the extra `0x1000` probe/reservation (`/tmp/luna-jit-tiered-rust-assembly.log`), versus the previous `0x1628` frame. This verifies scratch was outlined off the VM entry path, not that every performance threshold is passed. The ASM selector still needs confirmation that all generic invoke bodies are captured; timing/full gates remain required.

**Initial tier gate passed:** `nix develop -c make fmt jit-check jit-boundary jit-native jit-heap jit-registers` exited 0 (`/tmp/luna-jit-tiered-scratch.log`). Existing per-PC/budget model, eleven native integration tests, eleven heap tests and default/native register-255/stack-256 gates remain green. Add an explicit source-generated reference-result test for both sides of every capacity boundary before disassembly/performance/full acceptance.

Rust-side assembly exposes a non-obvious cost: LLVM inlined the 256-slot native scratch into `run_vm`, whose prologue reserves `0x1000 + 0x628` stack bytes and touches a probe page before it knows whether JIT is active (`target/jit-evidence/rust-assembly.log`, `/tmp/luna-jit-helper-v3-rust-assembly.log`). This is not sampled hotspot proof, but it contradicts the intended cheap compiled-Off/native-fragment boundary. Keep entry/bounds checks in the small dispatcher; move scratch/helper invocation into non-inlined Rust routines with capacities 8/16/32/64/128/256, selecting the smallest tier that fits the admitted register prefix. All used slots remain initialized; no heap allocation, GC-layout assumption or >256 scratch is added. Canonical repacking, code leases, fuel and panic semantics stay unchanged. Check every tier boundary including 256, verify the VM no longer reserves a probe page for scratch, and measure paired controls before acceptance. Increased Rust specialization/size must remain visible in later size gates.

#### Per-VM-slice code lease decision

**Measured result:** actual lookup leases survive clear/Off/quota shrink (`/tmp/luna-jit-slice-lease-retirement.log`, nine unit tests). Standalone paired `make jit-performance` still fails four controls (`/tmp/luna-jit-slice-lease-performance.log`): integer 2.2890, float 5.4165, array 1.1322, upvalue 0.5797, metamethod 0.8003, callbacks 0.7122, allocation 1.0303, Oslo 0.8411, cold 0.9762. Allocation shows 28940 native invocations with 3227 leases over 3263 lookups, demonstrating actual fragment reuse; short-function/frame cases still require almost one lease per invocation. This optimization does not solve the remaining callback/upvalue costs.

#### Monomorphic helper ABI v3 decision

**Profiler limitation:** `nix develop -c make jit-profile` built the symbol-retained checked workload, but `perf record` was denied by the host's `perf_event_paranoid=4` (`/tmp/luna-jit-helper-v3-profile.log`). No kernel setting or capability was changed. Add `make jit-rust-assembly` to inspect dispatch/helper code without privileged sampling. This is Rust-side assembly evidence only, not a native JIT disassembly/profiler or a claim about measured hotspot attribution.

**First measurement/profiling prerequisite:** standalone v3 `make jit-performance` still fails the same four controls (`/tmp/luna-jit-helper-v3-performance.log`): integer 2.5812, float 4.5909, array 1.0414, upvalue 0.5689, metamethod 0.7622, callbacks 0.6896, allocation 0.9974, Oslo 0.8424, cold 1.0244. Eliminating the indirect helper gateway is not yet a demonstrated acceptance gain. Add a repo-native `jit-profile` lane (opt-level 3, symbols retained, same checked 1000-sample upvalue workload) before attributing the remaining overhead to guesses. Do not change kernel perf permissions if collection is denied; use available disassembly/controlled experiments and record the limitation. The symbol-retained build is profiling-only, not a change to the shipping release profile or acceptance timing lane.

**Focused registry gate passed:** `nix develop -c make fmt jit-check jit-boundary jit-native jit-heap` exited 0 (`/tmp/luna-jit-helper-v3-registry.log`): ten combined unit tests, eleven native integration tests and eleven heap integration tests. Fixed symbol keys are unique; all nine null-host entries decline safely. The new environment/reference case returns 42 with native table reads/writes, upvalue reads/writes and allocation. Panic materialization/diagnostics, debug mutation, weak tables, per-slice GC, and finalizer resurrection still pass. Full feature/complete gates and independent paired timing remain required.

**Registry convention:** use one const-generic symbol constructor to bind the helper kind, static import name and specialized C entry. Compiler lookup matches explicit kind keys rather than relying on array position; array order is not an ABI. A uniqueness/null-host test covers all nine registered entries, and a dedicated native program exercises global environment writes/reads, reference constants/moves, table allocation/access and captured upvalue mutation. Initial v3 check/model/native/heap/policy gates passed (`/tmp/luna-jit-helper-v3.log`); repeat after this registry hardening and measure separately.

Replace the v2 runtime-kind gateway/indirect host callback with nine fixed helper symbols. Each imported symbol has the same six-argument C signature (opaque host, scratch pointer, a/b/c operands, PC), and its Rust implementation specializes a const helper kind. The opaque host carries only the scoped frame data pointer. No code examines Rust/GC layouts or chooses a callback address from Lua input. Null-host model calls still decline without touching state. Preserve operand synchronization, fresh guards, exact single-op accounting, PC-before-effects, caught panic transport, pinning and all lifecycle semantics. Bump private kernel/helper names to v3; there is no persisted native cache or public ABI to migrate. Validate all helper kinds and failure paths, then measure separately before accepting the cost/size tradeoff. The original v2 design remains historical evidence, not the current helper contract after this change.

**Focused gate passed:** `nix develop -c make fmt jit-check jit-native jit-boundary jit-heap jit-policy` exited 0 (`/tmp/luna-jit-slice-lease.log`): ten native integration tests, nine combined unit tests, eleven heap tests and policy gates. A dedicated array-construction case crosses interpreted `SetList` fragments with one lookup/lease and multiple native invocations in the same zero-fuel VM slice, then returns the exact expected result. Canonical state is repacked after interpreted work, and existing per-opcode hotness observations remain unchanged. Pin retirement tests are being routed through the actual lookup lease; paired measurements/full gate remain required.

The prior goal turn made concrete progress: operand-scoped helpers, retirement/quota fixes, panic diagnostics, lifecycle proofs and independent correctness/performance evidence. Current source still looks up and clones installed code before each native fragment and each known-interpreted opcode. Acquire one owned code lease when entering a source-eligible `run_vm` frame slice, release the manager borrow immediately, and reuse that lease until the slice/frame transition ends. Keep the existing per-opcode hotness observations when no code is installed so Auto promotion semantics do not drift. Do not retain scratch descriptors across interpreted operations: repack canonical state for every native invocation. Frame transitions/callbacks/suspension remain outside generated frames; slice-local leases drop on return/error/unwind. Add actual lookup/lease counters and a multi-fragment slice test, then run focused/full gates and paired timing before accepting the optimization. This does not close the still-failing thresholds or the remaining full-plan requirements.

#### Configuration retirement and quota diagnostics decision

**Lease/allocation evidence:** `nix develop -c make fmt jit-boundary jit-policy` passed (`/tmp/luna-jit-policy-lifetime.log`): nine combined unit tests and three policy integration tests. Pinned code remains executable after clear, Off, or quota shrink and frees only on final lease drop. Provider tests verify page-rounded partial-allocation refusal/drop reclamation, size-overflow refusal before reservation, and idempotent explicit reclamation. The initial test used an incorrect Cranelift enum spelling; corrected it against the pinned local dependency source (`JITMemoryKind::Executable`) before the successful gate.

#### Operand-scoped helper synchronization decision

**Final correctness gate:** `nix develop -c make fmt jit-verify` exited 0 after the panic diagnostics fix (`/tmp/luna-jit-panic-final-verify.log`), covering baseline, compiled Off/Auto/prepared Force, full-feature Force, doctests and warnings-denied API documentation. No code was edited while that gate ran. `git diff --check` passes; `feat/native-jit` remains uncommitted and experimental. The performance gate is being repeated afterward without concurrent validation; it remains a separate acceptance requirement.

**Fixed panic diagnostics:** moved native/helper aggregation before Rust panic resumption while leaving the helper's advanced PC intact and dropping the manager borrow before unwinding. `nix develop -c make fmt jit-heap jit-boundary` passed (`/tmp/luna-jit-panic-diagnostics.log`): 11 heap integration tests and nine combined unit tests. Pending-scalar inspection now also proves one real native entry, completed native/helper work, and a non-completed final helper attempt. The previous complete gate passed with 2338 executions over 390 suite invocations (`/tmp/luna-jit-operand-final-verify.log`), before this diagnostics-only correction; a new full gate is required. Paired timing artifacts also predate this correction and are not fresh release acceptance.

**Panic diagnostics discovery/decision:** the helper correctly transports panic and materializes state, but `Runtime::run` resumed the payload before aggregating the invocation counters. That omitted real native entry/helper work from diagnostics on panic. Aggregate completed logical work and helper attempts after the native frame returns, preserve the helper's advanced canonical PC instead of the panic exit's pre-op PC, release the manager borrow, then resume the original Rust payload. Extend the direct pending-scalar panic test to require actual native entry/helper counts and repeat focused/full gates. No failing helper is counted as a completed bytecode.

**Lifecycle gate passed:** `nix develop -c make fmt jit-heap jit-test-all JIT_MODE=force` exited 0 (`/tmp/luna-jit-operand-lifecycle.log`), including 11 synchronous heap tests and 12 with async. These supplement the existing full-feature suite and dedicated pending-scalar/debug/finalizer proofs; they do not close the complete transition matrix. The paired benchmark extension was edited after its example compilation in this run and needs its own check/gate.

**Focused implementation evidence:** helper operands now decode scratch scalars/canonical references and synchronize only destinations; panic catch materializes all pending scalar writes before Rust unwinds. `make jit-check jit-boundary jit-heap jit-policy jit-registers` passed (`/tmp/luna-jit-operand-helper.log`). Expanded heap tests passed (11 synchronous) with direct pending-scalar inspection after panic, debug local/setupvalue/join mutation, and finalizer resurrection; finalizer-only upvalue writes prove its handler actually used native helpers. Full-feature Force and the final complete gate are pending (`/tmp/luna-jit-operand-lifecycle.log`).

#### Paired benchmark decision

**Post-correction performance gate:** standalone `nix develop -c make jit-performance` again exits 2 through Make (`/tmp/luna-jit-panic-paired-performance.log`), with 11 checked alternating pairs and no concurrent gate. Median speedups: integer 2.5360, float 4.4367, array 1.0893, upvalue 0.5660, metamethod 0.8112, callbacks 0.7099, allocation 0.8916, Oslo 0.8135, cold 1.0142. The same four frozen thresholds fail; Oslo remains observational. The full correctness gate passes independently, so the next work is performance/resource/lifecycle hardening, not weakening tests or declaring completion.

**Repeated ratios (Off/Auto median, higher is better):** both standalone runs verify identical results and genuine native warm coverage. First threshold-check command exits 2 via Make because four workloads fail; the observational repeat exits 0 without hiding printed failures.

| Workload | First paired run | Repeat paired run | Frozen threshold | Result |
| --- | ---: | ---: | ---: | --- |
| Integer loop | 2.5621 | 2.4449 | 2.0000 | passes this lane |
| Float loop | 4.1912 | 4.5614 | 2.0000 | passes this lane |
| Array table | 1.0445 | 1.0807 | 1.2500 | fails |
| Closure/upvalue | 0.5868 | 0.5818 | 1.2500 | fails |
| Metamethod | 0.8094 | 0.7855 | 0.8333 | fails |
| Rust callbacks | 0.6931 | 0.7067 | 0.8333 | fails |
| Allocation/GC | 0.8832 | 0.8848 | 0.8333 | passes this lane |
| Oslo predicate | 0.8344 | 0.8271 | unscored | observational regression |
| Cold config | 1.0137 | 1.0051 | 0.8696 | passes this lane |

Artifacts: `/tmp/luna-jit-operand-paired-performance.log`, `/tmp/luna-jit-operand-paired-repeat.log`; Nix Rust/Cargo 1.97.1, Linux x86-64, opt-level 3, 11 alternating pairs per case after two warmups, compiler flags speed/verifier. Native loop speedups are meaningful but do not compensate for the failing application/mixed cases. Next performance work should target repeated short-fragment dispatch/upvalue-helper overhead and the predicate's mixed numeric guard exits, retaining exact numeric semantics. Full verification after these changes is running separately, not concurrently with benchmark sampling.

**First paired gate:** standalone `nix develop -c make jit-performance` failed as intended on four frozen workload thresholds (`/tmp/luna-jit-operand-paired-performance.log`, opt-level 3, 11 interleaved pairs, no concurrent gate). Upvalue speedup 0.5868, metamethod 0.8094, callbacks 0.6931 miss their thresholds; the table threshold also fails. Allocation speedup 0.8832 and cold 1.0137 pass their individual controls. Oslo 0.8344 is observational. This is meaningful native execution with checked results, not release acceptance. Record complete/repeated ratios before choosing the next optimization; keep all failures visible.

Add same-process Off/Auto sample pairs with alternating first-run order, two untimed warmups, checked identical results, actual native counters, ratio-of-medians and median paired ratios. Check the already frozen integer/float, table/upvalue, mixed, and cold thresholds without relaxation; report Oslo as observational because no numerical threshold was frozen for that individual predicate. A failing performance target must exit nonzero only when explicitly requested, after printing all cases. This is one opt-level-3 lane, not shipping-profile/disabled-feature/platform acceptance. Do not collect benchmark timing concurrently with validation/build workloads; repeat paired runs before drawing conclusions.

Replace full-register materialize/refresh on every non-callback heap operation with operand-scoped reads and destination synchronization. Scalar inputs decode tag/bits from scratch; reference-tag inputs use the same canonical traced slot. Every helper destination writes both canonical `Value` and its scratch descriptor. Table/upvalue setters still use existing barrier APIs. Generated scalar writes may keep old canonical references rooted until exit, but collection cannot occur within this arena mutation, so weak/collection timing does not change. Existing upvalue access only observes a closed cell, another stack, or registers above the current frame; its same-stack assertion excludes current-frame aliasing. Helpers cannot invoke users, debug hooks, collection, or frame changes. Materialize the whole prefix on every native exit and before resuming a caught panic; keep PC advancement before effects. If a future helper permits callbacks/safepoints/current-frame inspection, it must restore full synchronization before those operations. Validate scratch-to-helper-to-scalar chains, panic materialization, every-slice GC, reentrancy and full gates, then compare paired workloads before accepting this optimization.

**Implemented and focused validation:** `Manager::configure` now retires code/work on Off or lower code/snapshot/prototype limits and trims pending queue/attempt limits without stranded flags. The memory provider reports typed quota refusal via a per-compilation atomic flag. `nix develop -c make fmt jit-policy jit-boundary jit-native` passed (`/tmp/luna-jit-policy-initial.log`): policy unit 3, policy integration 3, combined boundary 6, native integration 9. Tests prove suspended Lua state survives disable, lower mapped quota frees installed code, typed refusal remains interpretable, and exhausted attempts require an explicit cache reset before retry. Broader lease/partial-allocation tests and full validation are next; metadata/compiler accounting is still incomplete.

**Pre-change gate:** `nix develop -c make fmt jit-verify` passed after initialized-prefix scratch and register fixes (`/tmp/luna-jit-register-verify.log`); `git diff --check` passed on `feat/native-jit`. An exploratory Auto benchmark ran concurrently with that gate (`/tmp/luna-jit-prefix-bench.log`): integer 95928 ns, float 88193 ns, table 251637 ns, upvalue 1301260 ns, metamethod 381110 ns, callbacks 439863 ns, allocation 244802 ns, Oslo 3198541 ns, cold 85451 ns. These unpaired, contended measurements suggest the prefix optimization helped short entries but are not performance acceptance evidence; mixed/upvalue thresholds still fail against the historical baseline.

Apply validated configuration transactionally. Switching Off retires queued and installed code; lowering code/snapshot/prototype admission limits conservatively retires installed code and resets admission state. Queue-only reductions retain the oldest requests within the new count/attempt limits and clear dropped requests' queued flags. Active code leases remain executable and charged until released, even if a new limit is below their live usage. Metadata registration shrinking remains a separate, unimplemented accounting requirement. Distinguish mapping-quota refusal from compiler failure using a per-compilation provider flag, not error-string matching, so explicit hosts receive `JitError::ResourceLimit` and Force preparation does not misclassify expected refusal as compiler corruption.

#### Initial heap-helper ABI decision (implemented v2)

**Register-boundary fix:** the allocator documents registers 0..255 and stack size 256, but `push` and interpreted `LoadNil` computed exclusive range ends in u8. A dedicated reference test reproduced a debug overflow at `src/thread/vm.rs` when clearing register 255 (`/tmp/luna-jit-register-boundary-before.log`). Empty explicit returns also converted stack top 256 to u8 with `unwrap`. Fixed range arithmetic in `src/compiler/register_allocator.rs` and `src/thread/vm.rs`; `src/compiler/compiler.rs` now emits empty returns without converting stack top, and reports register exhaustion rather than panicking for nonempty returns. `nix develop -c make fmt jit-registers jit-boundary jit-heap` passed (`/tmp/luna-jit-register-boundary-after.log`): default integration 2, allocator unit 1, native integration 2, boundary 3, heap 8. The source regression proves genuine native execution with a 256-register prefix; the manual register-255 LoadNil remains deliberately interpreted. Full validation after these fixes is pending.

**Scratch storage decision:** reserve at most 256 slots on the Rust stack but initialize only the admitted prototype's register prefix. Form a typed slice only after every slot in that prefix has been written; generated operands and helper slot counts are verified against that prefix. The unused `MaybeUninit` suffix is never read or dropped as `Slot`. This removes a 4 KB initialization on every short invocation without weakening scalar initialization, GC rooting, or slice budgets. Review the new prefix cast and repeat model/heap/full gates.

The private entry ABI advanced to v2 with an opaque, invocation-scoped host pointer. Generated code calls one registered non-unwinding helper gateway with a fixed helper kind and validated immediate operands, not a generic bytecode interpreter or arena pointer baked into code. The host pointer refers to a Rust-owned frame borrowing the existing `LuaRegisters`, `Context`, and active closure; it never escapes the synchronous invocation. The first implementation materialized/refreshed all registers around every helper; the operand-scoped decision above supersedes that synchronization strategy. Reference values stay in canonical, traced slots. Implemented helpers perform reference move/constant load, plain table read/write, new-table allocation, and upvalue reads/writes through existing APIs. They never invoke Lua or Rust user callbacks, change Lua frames, close values, collect the arena, or enter the compiler.

Metamethod/intercept/readonly/invalid-key guards decline before script-visible effects and let the original VM perform the opcode. Fresh table/upvalue checks replace cached shape assumptions. Helpers catch Rust panic payloads, return an explicit panic exit through generated code, and resume the panic only from Rust after the native frame has returned; no panic is swallowed as successful fallback. The old scalar-only entry remains available in the boundary-model tests through a declining/null helper host. Dedicated counters and mutation/GC tests must prove successful native heap helpers, not merely scalar prefix execution. Repeat the unsafe review and full gate after this change.

**Goal:** fully implement this plan on the new branch, not stop at the scalar milestone.

**Instructions:** choose workloads autonomously; use Make targets and patch tools; preserve default interpretation, exact observed slice accounting, source-only provenance, and arena lifetimes. Do not relax frozen thresholds or describe native fallback as heap acceleration.

**Accomplished:** optional pinned compiler/configuration, explicit outside-arena service, source weak generation IDs, validated snapshots, W^X page-accounted ownership/leases, scalar/loop and heap/upvalue native execution, prepared Force wrappers, independent scalar boundary model, twelve native integration tests, eleven heap tests (twelve with async), panic/GC/debug/finalizer proofs, register-255/stack-256 fixes, retirement/typed quota refusal and policy/allocation tests. This turn adds one code lease per VM slice, explicit unique helper symbols/ABI v3, outlined 8..256 scratch tiers and reference-return boundary tests. The complete gate plus Rust assembly passes: 2358 executions over 390 suite invocations (`/tmp/luna-jit-lease-v3-tier-final-verify.log`). Separate repeated timing still fails four controls (`/tmp/luna-jit-lease-v3-tier-repeat-performance.log`). Evidence is collected with `make jit-evidence`; `feat/native-jit` remains uncommitted/experimental, and the full goal stays active.

**Discoveries:** core states contain five source library registrations; VM transitions charge differently; modules need explicit reclamation; register range ends require wider arithmetic. Current helpers permit operand-scoped synchronization with full exit/panic materialization. LLVM inlined the 4 KB scratch into `run_vm` even for compiled-Off entry; outlining changes its assembly stack reservation from `0x1628` to `0x648`, and all six native tiers are separately visible. Short-frame/upvalue/callback workloads still regress; exact hotspot attribution remains unproven because host perf is denied at `perf_event_paranoid=4`. Fixed helper symbols eliminate runtime-kind/indirect-host dispatch but do not alone solve acceptance. Quota diagnostics use a provider flag, not error-string matching.

**Next steps:** investigate source-identity/cache routing and short-frame/upvalue costs with controlled evidence; add exact mixed comparison handling without weakening numeric semantics or thresholds. Keep performance failures visible and check specialization/code-size tradeoffs. Complete boundary mock/IR/diagnostic/resource tests, broader lifecycle/weak-mode/interleaved stress, eviction/backoff/full ledgers and fuzz/platform/CI acceptance. Native JIT disassembly (not merely Rust assembly), ARM64/musl execution, denied-exec-memory tests, compiler/metadata ceilings, compiled-disabled/shipping-profile lanes and active CI remain open. Host perf permission and inherited clippy failure are recorded, not changed or suppressed.

**Relevant files:** `src/jit/{mod,abi,helpers,ir,registry,backend,model}.rs` own the tier; `src/lua.rs` owns configuration/service; `src/closure.rs` registers provenance; `src/thread/{thread,vm}.rs` share canonical registers and mixed dispatch; `src/compiler/{compiler,register_allocator}.rs` fix register boundaries; `tests/jit_{native,heap,policy}.rs`, `tests/register_boundaries.rs`, and `tests/common/mod.rs` establish native/reference acceptance controls; `examples/jit{,_bench}.rs`, `Makefile`, `JIT.md`, and this plan carry reproducible evidence.

#### Latest session handoff: exact mixed numeric comparison

**Goal:** continue the full plan on `feat/native-jit`; the goal remains active and the feature remains experimental/uncommitted.

**Instructions:** workload selection is delegated. Do not request consumer choices again, relax frozen thresholds, run benchmarks alongside build/test gates, or equate a native numeric matrix with complete fuzz/platform acceptance. Use Nix/Make and patch tools.

**Discoveries:** the existing shared comparator missed negative fractional ties, producing false equality and incorrect ordering. Fixing only the native tier would preserve a wrong reference or create inconsistent tiers. Saturating Cranelift conversion plus exact integral/tie/bound/NaN checks permits direct native mixed comparison without helper calls or a private ABI change. Oslo's predicate now records zero guard exits and substantially fewer interpreted instructions; short-frame/dispatch costs still dominate its total workload.

**Accomplished:** reproduced and fixed the reference bug; added nineteen explicit integration cases and a 122850-invocation mixed numeric boundary matrix; added `make jit-numeric`; completed the full gate (2374 executions / 390 suites) and two standalone checked paired benchmark runs. `git diff --check` passes. See the comparison decision below for exact commands/logs and the unchanged four failing controls.

**Next steps:** harden compiler admission/IR verification and add bounded supervised fuzz-smoke coverage before continuing unverified micro-optimizations. Then finish the complete memory ledgers, failure injection, lifecycle/cache stress, native diagnostics, real CI and platform evidence. Performance still requires profitable short-frame/upvalue/table execution and all missing release lanes; no phase is marked complete based only on these numeric tests.

**Relevant files:** `src/constant.rs` corrects shared fractional ordering; `src/jit/backend.rs` emits exact mixed scalar comparisons; `src/jit/model.rs` validates boundary behavior; `tests/numeric_semantics.rs` reproduces the reference bug; `tests/jit_native.rs` proves exact outcomes, fuel/GC behavior and zero guard exits; `Makefile`, `JIT.md` and `PLAN_JIT.md` expose and document the gates.

| Decision | Deadline | Default direction / required evidence |
| --- | --- | --- |
| Exact Cranelift version and Rust baseline | Phase 0 | Selected Cranelift 0.136.1, dependency MSRV 1.96, verified Nix toolchain 1.97.1; remaining platform evidence is explicit. |
| Public configuration/service signatures | Phase 1 | Implemented additive config/stats/capabilities and `prepare_jit`, `service_jit`, `clear_jit_cache`; interpreter defaults and step signature unchanged. |
| Native invocation/helper ABI | Phase 2 | Implemented repr(C) scalar slot/exit exchange and fixed-symbol opaque frame ABI v3, with tiered outlined scratch and one code lease per VM slice; panic transport returns to Rust before unwinding. Broader transition/unsafe review remains open. |
| Provenance/identity sidecar mechanism | Phase 3 | Implemented traced weak registrations with checked monotonically allocated u64 IDs; raw address is only a lookup aid verified through weak upgrade and identity. |
| Compiler service versus worker/process model | Phase 3, finalized Phase 7 | Explicit host preparation first; bounded owned-data worker only after proof. Process isolation for hard compiler resource boundaries. |
| Memory ledger semantics and limits | Phase 3 | Separate collector/native/compiler accounting; precise documentation of hard versus coarse limits. |
| Invalidation strategy | Phase 6 | Fresh guards first; tracked dependency generations only after complete mutation audit. |
| Performance thresholds and representative consumer | Before Phase 8 | Maintainer delegated selection; frozen thresholds and Oslo-derived predicate are recorded above. Unmet controls and missing release lanes remain completion blockers, not unanswered workload-selection questions. |
| Supported first-release targets | Phase 9 | Linux x86-64 GNU/musl and ARM64 only with executed evidence; explicitly narrow if approved. |

These decisions require experiments/review, not assumptions filled in by an executor. Record the chosen outcome here before implementing a dependent phase.

### Exact mixed comparison decision and reference regression

The reference comparator incorrectly treated an integer equal to the truncated part of a negative fractional float as equal to that float (`-1 == -1.5`, `0 == -0.5`). A default-feature regression reproduces this before the fix (`/tmp/luna-jit-negative-fraction-before.log`). Correct the negative fractional tie in `src/constant.rs` before deriving native behavior; add explicit expected-result tests, not only differential agreement.

The reference fix now passes all sixteen default numeric tests. The dedicated native test checks nineteen explicit six-result tuples, both operand orientations, zero/one-fuel slices and collection after every slice; all pass with zero native guard exits. The initial expanded model build exposed an incorrect test type name (`ConstantIndex` versus the actual `ConstantIndex8`), corrected before running that model. The expanded model checks 122850 invocations across three operators, both skip polarities, both numeric orientations, register/constant operands, five budgets, thirteen integer boundaries and 157 float patterns (including 128 deterministic bit patterns). It matches exit PC, counts, reasons and exact slots, and checks complete mapping reclamation after each kernel.

`nix develop -c make fmt jit-numeric jit-boundary jit-native` passes (`/tmp/luna-jit-mixed-numeric-focused.log`): numeric 16, native focused 2, comparison model 1, combined boundary 11 and native integration 13. The complete `nix develop -c make jit-verify` also passes (`/tmp/luna-jit-mixed-numeric-verify.log`): 2374 executions across 390 suite invocations, including repeated modes/docs rather than unique tests. `git diff --check` passes. Standalone paired timing is collected only after that gate finishes; correctness does not imply performance acceptance.

Two standalone `nix develop -c make jit-performance` runs after validation both exit 2 through Make, keeping all four missed frozen controls visible (`/tmp/luna-jit-mixed-numeric-performance.log`, `/tmp/luna-jit-mixed-numeric-repeat-performance.log`). Each has eleven checked alternating Off/Auto pairs after warmup, actual native coverage and no concurrent build/test gate. Ratios are Off/Auto medians (higher is better):

| Workload | First | Repeat | Acceptance |
| --- | ---: | ---: | --- |
| Integer | 2.6298 | 2.3906 | passes 2x lane |
| Float | 4.7009 | 3.6829 | passes 2x lane |
| Table | 1.1310 | 1.1559 | fails 1.25x |
| Upvalue | 0.5860 | 0.5898 | fails 1.25x |
| Metamethod | 0.7846 | 0.8066 | fails 0.8333 minimum |
| Rust callback | 0.7000 | 0.7138 | fails 0.8333 minimum |
| Allocation/GC | 0.9972 | 0.9805 | passes 0.8333 lane |
| Oslo predicate | 0.8949 | 0.9039 | observational; still slower than Off |
| Cold configuration | 1.0255 | 0.9653 | passes 0.8696 lane |

Oslo now has zero guard exits, 454947 native logical instructions and 53 interpreted instructions over the run; eliminating the mixed-comparison exit is proven by counters, not a claim of end-to-end consumer acceptance or a controlled old/new timing experiment. Size/shipping/compiled-disabled/platform controls remain unmeasured. The full goal remains active.

Implement mixed comparison directly in generated scalar code without a new helper ABI. Saturating float-to-i64 conversion cannot trap; compare the integer against that integral value and break ties using the float versus the converted-back integral value. Explicit +/-2^63 bounds and ordered/NaN checks preserve equality and ordering at saturation boundaries. Both numeric tags are guarded before execution, and both operand orientations use the same normalized integer/float path. Check all operators and skip polarities, exact slice budget/PC, extreme values and independent expected outcomes. Keep existing performance thresholds unchanged.

### Compiler admission and supervised fuzz decision

Revalidate snapshots at the backend entry before constructing compiler state or allocating mappings. Validate register capacity and canonical scalar descriptors in addition to opcode operand/control-flow bounds; invalid IR must return a typed compilation refusal with unchanged mapped-memory usage. Keep source provenance restrictions unchanged: fuzzed internal snapshots do not become a public binary/IR loading API.

Add deterministic seeded admission and scalar native/model campaigns as ignored library tests, launched through Make. A Rust supervisor starts one disposable worker process per seed, enforces CPU/address-space/core/file-size limits before exec and a parent wall-clock deadline, captures seed-specific logs, and treats signals, nonzero exit, timeout or mismatch as failures. The smoke gate uses a fixed small corpus; a separate bounded long-campaign target exposes seed/case/target controls and reproduction commands. Test supervisor failure handling independently. This is an initial admission/scalar campaign, not coverage of all heap/lifecycle mutations, a coverage-guided fuzzer, Miri, or supported-platform certification.

The first supervised seed exposed a boundary-model precedence error: unknown entry PC with zero budget returned exhausted in the model, while generated dispatch correctly declined that unknown entry before executing any opcode. Existing model tests only entered valid PCs. Corrected `src/jit/model.rs` and added explicit unknown/end/usize::MAX entry tests across zero/nonzero budgets; no production behavior or threshold was changed. The original child failure is retained in `target/jit-evidence/fuzz/1790765317483769638-2106826/seed-0-all.log` and `/tmp/luna-jit-fuzz-initial.log`. This finding confirms that the supervisor propagates real child assertion failures; it is not evidence that the full fuzz scope is complete.

**Implemented evidence:** malformed mutation/unit gates pass, all workers verify actual CPU/address-space/core/output limits, and injected panic/SIGTERM/wall-timeout tests pass. `make jit-verify` now includes supervised smoke on supported Linux hosts (unsupported hosts explicitly skip this native-only lane). `nix develop -c make fmt jit-check jit-fuzz-smoke jit-fuzz FUZZ_CASES=1024 FUZZ_SEEDS=0,1,42,0xdeadbeef,0xffffffffffffffff` passes (`/tmp/luna-jit-fuzz-counted-campaign.log`). The larger campaign runs 5120 generated kernels in five sequential worker processes: 2055585 kernel invocations and 5969025 native logical instructions, comparing exact PCs/counts/reasons/tags/scalar bits (NaN payloads may differ), plus malformed admission refusal and final zero mapping usage. It completes in 70.30 seconds in the debug/JIT x86-64 Linux build. This finite deterministic corpus is not proof of absence of bugs.

Artifacts: `target/jit-evidence/fuzz/1790765695878204589-2110108/{campaign,runtime}.txt` records target/settings/seed list and the exact replay command; the five `seed-*-all.log` files contain individual progress, completion counters and results. `/tmp/luna-jit-fuzz-campaign.log` preserves the earlier uncounted repeat (its old `native_invocations` label means entry attempts, not logical instructions). The current harness separately names `kernel_invocations` and `native_instructions`, so zero-budget/unknown-PC entry attempts are not misreported as executed native work.

#### Latest session handoff: admission and supervised fuzz

**Goal:** fully implement the original plan on `feat/native-jit`; keep the goal active until all release criteria are proven.

**Instructions:** use Nix/Make and patch tools; preserve interpreter defaults, source provenance and existing thresholds. The maintainer delegated corpus selection. No commits, release or kernel-permission changes were made.

**Discoveries:** the old scalar model checked budget before rejecting unknown entry PCs, unlike native dispatch. The new seeded corpus exposed this; a focused regression now covers unknown/end/usize::MAX PCs under zero and nonzero budgets. Entry attempt counts include zero-work cases and must not be labeled native logical work. Fuzz worker process limits are asserted after exec, not inferred from parent configuration.

**Accomplished:** backend revalidation and canonical descriptor/capacity checks; twelve malformed admission cases with zero mapped usage; explicit unknown-entry model regression; deterministic scalar/admission campaigns with tested child panic/signal/wall-timeout propagation, CPU/address-space/output/core ceilings, log/seed/replay artifacts and mapping reclamation. A five-seed 5120-kernel campaign passes, as does the full `nix develop -c make jit-verify` (`/tmp/luna-jit-admission-fuzz-verify.log`): 2386 executions / 392 suite invocations, including repeated modes/docs and two supervisor tests. The final smoke uses four seeds, 96 kernels, 38073 entry attempts and 120597 native logical instructions (`target/jit-evidence/fuzz/1790765948177971563-2147880`). `git diff --check` passes.

**Next steps:** complete metadata/snapshot/compiler/combined-host accounting and refusal/eviction/backoff tests; extend the isolated corpus to real Lua effects, guards, heap/lifecycle mutation and coverage-guided/Miri lanes. Add generated-code diagnostics and executed musl/ARM64/active-CI evidence. The existing four performance failures and missing shipping/disabled/size lanes remain open; no release claim or full-plan completion follows from the seeded campaign.

**Relevant files:** `src/jit/ir.rs` validates descriptors/capacities; `src/jit/backend.rs` revalidates before compiler setup; `src/jit/model.rs` covers unknown entry semantics; `src/jit/fuzz.rs` generates/verifies IR and supervises limited workers; `src/jit/mod.rs` includes the test-only supported-target harness; `Makefile` provides smoke/campaign/full-gate targets; `JIT.md` and this plan record scope, reproducibility and remaining gaps.

### Owned-container allocation ledger decision

Replace the registration count guessed from `max_snapshot_bytes` with an independent positive `max_metadata_bytes` budget. Use a static shared allocation ledger and `allocator-api2` allocator for the traced weak registry, manager tracking/code-index maps, pending identity buffer and owned snapshots. Reserve requested allocation layouts before allocating; quota refusal returns fallibly and releases partial state. Count retained capacity and transient old/new growth allocations, not merely live element counts. Use allocator-backed hashbrown maps with the existing standard keyed hasher; never weaken generation/weak-identity verification or embed GC values in the static manager.

Report live/peak requested container bytes separately from page-rounded native mappings and collector metrics. Lowering the metadata ceiling conservatively retires all registrations/code/work and drops container capacity before installing the smaller ceiling; live Lua closures remain usable through interpretation. Empty retired containers release storage, and all configured metadata/queue/snapshot refusal paths must remain non-aborting. This allocator ledger is not process RSS: fixed owner/Arc/Rc headers, allocator overhead, Cranelift internal/transient/retained allocations and host callbacks are not magically measured by it. Complete compiler/combined-host accounting remains a separate open requirement; do not relabel this milestone as the entire resource contract.

**Implemented container gates:** `nix develop -c make fmt jit-resources jit-check` passes (`/tmp/luna-jit-container-ledger-final-focused.log`), with eight allocation/policy unit tests and five integration tests. The tests cover successful/failed growth and retained capacity, existing-owner quota reduction, checked overflow, injected underlying allocation failure, partial snapshot rollback, queue refusal without stranded flags, mapping cleanup after code-index refusal, registration/snapshot budget independence, conservative retroactive retirement and last-source reclamation. Initial constructor wiring required ending the manager `Ref` before the root struct's final expression; a local allocator clone resolves the borrow without widening lifetimes.

The full `nix develop -c make fmt jit-verify` passes (`/tmp/luna-jit-container-ledger-verify.log`): 2451 executions / 398 suite invocations, including repeated modes/docs and the supervised smoke. A separate five-seed 5120-kernel campaign passes (`/tmp/luna-jit-container-ledger-campaign.log`, `target/jit-evidence/fuzz/1790767952930566515-2216708`), with a new zero-snapshot-charge assertion after every case and mapping reclamation checks unchanged. Existing code leases remain covered by the model tests. `git diff --check` passes. These gates prove the stated container milestone, not complete resource/release acceptance.

**Remaining resource scope at the container checkpoint:** backend entry flags and provider record buffers were not yet charged here; the next section records their completed implementation. Fixed owner overhead, compiler internal/retained data, compiler CPU/working-memory isolation or precompilation policy, full combined-host enforcement, cache eviction/backoff and failure diagnostics remain open. Keeping these explicit prevents a container-byte metric from being mistaken for process RSS or a finished compiler ceiling.

**Paired performance remains unaccepted:** two standalone `nix develop -c make jit-performance` runs after all validation both exit 2 through Make (`/tmp/luna-jit-container-ledger-performance.log`, `/tmp/luna-jit-container-ledger-repeat-performance.log`). Eleven alternating checked pairs, with no concurrent gates, still fail the same four controls. Off/Auto median ratios (first / repeat): integer 2.5134 / 2.4656; float 4.3378 / 4.4850; table 1.0803 / 1.1034; upvalue 0.5479 / 0.5494; metamethod 0.7961 / 0.7813; callback 0.6952 / 0.7060; allocation/GC 0.9953 / 0.9993; Oslo 0.8732 / 0.8947 (unscored); cold 1.0066 / 0.9529. Upvalue is slower than the historical ratios, but separate revisions/runs are not a controlled attribution experiment. Do not accept the optimization phase or loosen controls. Reports now include current/peak metadata/snapshot charges and refusals; ordinary benchmark workloads report zero metadata/registration refusals and zero live snapshots after execution.

#### Latest session handoff: requested container quotas

**Goal:** continue full `PLAN_JIT.md` implementation on `feat/native-jit`; full release acceptance remains unproven and the goal stays active.

**Instructions:** Nix/Make and patch tools only; source-only weak/generation checks, interpreted defaults and frozen performance controls remain unchanged. No commits or release actions were taken.

**Discoveries:** element-count caps do not measure retained capacity or transient growth. Default allocator growth reserves the entire new block while the old one remains charged, making admission accurate for requested layouts. Static allocator handles can be traced through gc-arena's hashbrown implementation without containing branded GC data. Snapshot vectors now charge their actual lifetime rather than manually set/reset stats. Explicitly distinguish these container charges from missing backend/compiler/RSS accounting.

**Accomplished:** independent positive metadata configuration, shared static fallible allocation ledgers, live/peak/refusal diagnostics, traced weak registry and manager/queue/preparation container charging, owned snapshot charging, conservative retroactive metadata reduction, reclamation on collection and partial failure, eight unit/five integration tests, full gate (2451 executions / 398 suites), five-seed long campaign (5120 kernels, 2055585 entries, 5969025 native logical instructions) and two standalone failed performance gates. Commands/logs/artifact directories are recorded above. `git diff --check` passes.

**Next steps:** first account persistent backend entry flags/allocation records and prove their charges survive active leases until final reclamation. Then complete compiler internal/fixed-owner and combined-host accounting/enforcement, eviction/backoff/failure diagnostics and full lifecycle/fuzz/platform/CI/size lanes. Four workload thresholds remain failed; check the short-frame regression with controlled profiling/evidence rather than attributing it speculatively or relaxing acceptance.

**Relevant files:** `src/jit/resources.rs` owns quota allocator/high-water/refusal tests; `src/jit/mod.rs` configures owned ledgers and manager containers; `src/jit/registry.rs` traces budgeted weak maps and retires registrations; `src/jit/ir.rs` owns budgeted snapshots; `src/lua.rs` wires limits/stats without changing collector metrics; `src/jit/{model,fuzz}.rs` adapt fixtures and check snapshot reclamation; `tests/jit_resources.rs` and `tests/jit_config.rs` test pressure and validation; `examples/jit_bench.rs`, `Makefile`, `JIT.md` and this plan publish evidence and scope.

### Persistent backend metadata and memory-failure decision

Charge `Code::entries` and the native provider's allocation-record vector to the same persistent metadata allocator used by the owning runtime. Allocate the entry flags fallibly before compiler setup and reserve each mapping record before allocating a native segment. Keep these reservations alive with the code lease, not the cache index; final reclamation must release both metadata and mappings. A metadata refusal remains typed `ResourceLimit("JIT metadata")`, separate from the page quota. Partial cleanup must affect only the failed module.

Add test-only backend allocation/protection failure injection, never a script/public configuration option. Explicit service reports native allocation/protection denial as capability unavailability; generated code is never published after failed protection. Exercise real source loading, preparation refusal, interpreter fallback, bounded attempts and recovery, including preservation of another installed module. This tests backend denial handling, not a claim that the current host denies executable memory or that arbitrary memory corruption can be recovered.

**Focused implementation evidence:** `nix develop -c make fmt jit-check jit-boundary jit-resources jit-native` passes (`/tmp/luna-jit-persistent-backend-focused.log`): twenty combined boundary/resource tests, eight resource units, five resource integrations and thirteen native integrations. Entry flags and outer provider records use the owning metadata allocator. The existing lease test now uses that same allocator and proves live metadata/mappings survive all four retirement paths (clear, Off, lower page quota, lower metadata quota), remain executable, and return to zero after the last lease drops. Metadata record refusal leaves old segments intact; explicit free is idempotent and releases record capacity. Real-source denial tests cover both allocation and protection failure, zero new installed regions/entries, snapshot cleanup, another module's native execution, interpreted execution of the refused source, recovery after reset and final zero charges.

The code-index refusal test now injects underlying allocation failure after the entry-vector and mapping-record allocations, rather than setting a quota that would reject earlier in the expanded backend. This keeps its intended post-generation cleanup coverage.

**Full validation:** `nix develop -c make jit-verify` passes (`/tmp/luna-jit-persistent-backend-verify.log`): 2466 passed executions across 398 suite invocations, including repeated feature/mode runs, documentation and supervised smoke. These are execution counts, not unique tests. Subsequently, a test-only backend-metadata reclamation assertion was added to the seeded harness; `nix develop -c make fmt jit-fuzz-smoke jit-fuzz FUZZ_CASES=1024 FUZZ_SEEDS=0,1,42,0xdeadbeef,0xffffffffffffffff` passes with that assertion (`/tmp/luna-jit-persistent-backend-campaign.log`). Its five-seed campaign covers 5120 kernels, 2055585 entry attempts and 5969025 native logical instructions in 70.11 seconds; artifacts and replay command are under `target/jit-evidence/fuzz/1790769642319453408-2285940`. Each case checks exit/PC/count/scalar state and native, persistent backend metadata and snapshot reclamation. This remains a bounded deterministic scalar/admission corpus, not full Lua lifecycle or coverage-guided certification.

**Performance remains unaccepted:** two standalone `nix develop -c make jit-performance` runs both exit 2, with no concurrent validation/campaign jobs (`/tmp/luna-jit-persistent-backend-performance.log`, `/tmp/luna-jit-persistent-backend-repeat-performance.log`). Eleven alternating checked pairs still fail table, upvalue, metamethod and callback controls. Off/Auto median ratios (first / repeat): integer 2.4453 / 2.4380; float 4.1927 / 4.1340; table 1.0305 / 1.1212; upvalue 0.5455 / 0.5516; metamethod 0.7567 / 0.7625; callbacks 0.6517 / 0.6486; allocation/GC 0.9898 / 0.9809; Oslo 0.8534 / 0.8812 (unscored); cold 1.0118 / 1.0000. The Oslo-derived predicate is observational, not an end-to-end migrated consumer. No threshold or scope was relaxed, and no causal performance attribution follows from separate revision runs.

#### Latest session handoff: persistent backend quotas and denial

**Goal:** fully implement this plan on `feat/native-jit`; the full goal remains active and release acceptance incomplete.

**Instructions:** workload selection is delegated to the implementer. Use Nix/Make and patch tools, preserve interpreter defaults, source provenance and frozen performance controls. No commits, releases or kernel-permission changes were made.

**Discoveries:** backend-owned entry flags and provider bookkeeping must outlive cache retirement while a native lease exists. Reserving a mapping record before page allocation prevents a metadata refusal from creating an untracked segment. Post-generation refusal injection must account for earlier backend allocations. Allocation/protection denial is a capability failure, distinct from requested-byte quota refusal.

**Accomplished:** persistent backend layout charging; fallible pre-compilation flags and pre-mapping records; lease-retained charges and final reclamation; typed denial and real-source fallback/recovery tests preserving an unrelated installed module; focused gates, full validation, five-seed campaign and two performance measurements recorded above.

**Next steps:** complete fixed-owner/compiler and combined-host accounting/enforcement, eviction/backoff and resource scheduling diagnostics. Resolve the four frozen workload failures with controlled profiling. Complete broader lifecycle/fuzz/unsafe review, generated-code diagnostics, actual musl/ARM64 execution, active CI and shipping/disabled-feature/size lanes. Container layout bytes are not RSS or a hard compiler ceiling.

**Relevant files:** `src/jit/backend.rs` owns charged flags/provider records and denial injection; `src/jit/mod.rs` passes the owning allocator; `src/jit/model.rs` proves four retirement paths with live leases; `src/jit/resources.rs` tests post-generation installation refusal; `src/jit/fuzz.rs` checks backend metadata reclamation; `JIT.md` and this plan document the contract and evidence.

### Native platform and active-CI decision

Promote the existing test template into `.github/workflows/tests.yml` rather than maintaining two divergent test workflows. No deployment hook for the root template was found in the Makefile, flake, submodules or README; GitHub discovers the [root `.github/workflows` directory](https://docs.github.com/en/actions/concepts/workflows-and-actions/workflows). Preserve baseline checks; add a fail-independent GNU/musl x86-64 and GNU ARM64 matrix with Rust 1.97.1, commit-pinned actions, target-specific caches, a 45-minute deadline and always-uploaded native logs/campaign artifacts. Push, pull-request and manual-dispatch events cover the implementation branch without requiring a release. Use `ubuntu-24.04-arm` for actual ARM64 execution, not emulation or cross-compilation; this runner label is documented in the [GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners). Explicit toolchain inputs follow the [toolchain action interface](https://github.com/dtolnay/rust-toolchain/blob/master/README.md). Hosted execution has not occurred in this uncommitted worktree and must not be claimed.

Forward `TARGET` through baseline/native checks, tests, doctests, documentation and recursive fuzz recipes. The `jit-platform` wrapper refuses undeclared triples and mismatched host architecture before invoking the full gate and prepared example. Record GNU versus musl in each fuzz runtime/replay artifact. Add actionlint to the Nix environment and expose `make ci-check`; this is structural validation, not evidence that a remote run completed.

**Executed musl evidence:** `nix develop -c make jit-native jit-boundary jit-heap jit-policy jit-resources jit-example TARGET=x86_64-unknown-linux-musl` passes (`/tmp/luna-jit-musl-focused.log`). It executes thirteen native tests, twenty-four non-ignored boundary tests, eleven heap tests, three policy units/three policy integrations, eight resource units and five resource integrations. The static-PIE musl example returns 5000050000 with 200007 native logical instructions and 61440 mapped bytes. This is native runtime evidence on the x86-64 host, not merely a successful cross-build.

The full `nix develop -c make jit-verify jit-example TARGET=x86_64-unknown-linux-musl` passes (`/tmp/luna-jit-musl-verify.log`): 2466 passed executions across 398 suite invocations, including default/compiled-Off/Auto/Force/all-feature Force, doctests, warnings-denied rustdoc and supervised smoke. The smoke artifacts under `target/jit-evidence/fuzz/1790770819459537653-2331425` record the musl binary and exact target-qualified replay command: 96 kernels, 38073 entries and 120597 native logical instructions. Recipe inspection confirms baseline/JIT test/check/doc commands do not silently fall back to the host target. Local executable-memory allocation/protection-denial tests run within this gate; it does not establish a real host-permission-denied deployment.

**Workflow/refusal verification:** `nix develop -c make fmt ci-check` passes using actionlint 1.7.12 (`/tmp/luna-jit-ci-wiring.log`); final workflow lint and architecture/unknown-target rejection assertions pass (`/tmp/luna-jit-platform-refusal.log`). The ARM64 target is rejected on this x86-64 host before any build, as is an undeclared wasm target. The positive `nix develop -c make jit-platform TARGET=x86_64-unknown-linux-gnu` wrapper also passes (`/tmp/luna-jit-gnu-platform-verify.log`): 2466 passed executions / 398 suite invocations and the expected example result with 200007 native logical instructions. Its fuzz artifacts under `target/jit-evidence/fuzz/1790771075666122478-2385339` identify the GNU binary and target-qualified replay command. Actual ARM64 execution and hosted run results remain absent; platform/release phases are not complete.

**Extended musl campaign:** `nix develop -c make jit-fuzz TARGET=x86_64-unknown-linux-musl FUZZ_CASES=1024 FUZZ_SEEDS=0,1,42,0xdeadbeef,0xffffffffffffffff` passes (`/tmp/luna-jit-musl-campaign.log`, `target/jit-evidence/fuzz/1790771096736888167-2385896`). Five sequential workers complete 5120 kernels, 2055585 entry attempts and 5969025 native logical instructions in 87.09 seconds, within their individual enforced CPU/address-space/wall/output limits. This tests the same bounded deterministic scalar/admission corpus on a musl executable, including mapping/backend metadata/snapshot cleanup; it is neither coverage-guided nor a full heap/lifecycle fuzz result. No performance timing ran concurrently with validation or this campaign.

#### Latest session handoff: native musl and CI wiring

**Goal:** continue full implementation on `feat/native-jit`; full acceptance remains unproven and the goal stays active.

**Instructions:** use Nix/Make and patch tools, preserve the original scope and frozen controls. Workload selection is delegated. No commits, pushes, releases, remote CI dispatches or kernel-permission changes were made.

**Discoveries:** the root workflow template was not in GitHub's discovery directory. Forwarding a target only through `cargo check` does not establish that recursive test/doc/fuzz recipes execute it. Static-PIE musl execution supports the current native/helper/collector boundary and process-limit supervisor on this host. Target-qualified replay commands prevent platform evidence from silently changing libc on reproduction.

**Accomplished:** promoted and linted the active workflow; added commit-pinned GNU/musl x86-64 and ARM64 jobs, target-specific caches and failure evidence uploads; propagated targets through validation; added declared-platform/architecture guard; executed full GNU and musl correctness gates (2466 executions / 398 suites each), prepared native examples and a five-seed extended musl campaign. Commands/artifacts/results are recorded above. Formatting and patch-whitespace gates pass.

**Next steps:** execute ARM64 and hosted CI when a committed/pushed workflow is available; do not substitute cross-compilation or local lint for those results. Continue fixed-owner/compiler/combined-host limits, eviction/backoff/scheduling diagnostics, broader lifecycle/heap fuzz and unsafe/Miri review, generated-code diagnostics and shipping/disabled-feature/size measurement. The same four frozen performance controls remain failed; this platform milestone does not waive them.

**Relevant files:** `.github/workflows/tests.yml` is the promoted active workflow; `workflows/tests.yml` is removed; `Makefile` propagates targets and adds `jit-platform`/`ci-check`; `flake.nix` supplies actionlint; `src/jit/fuzz.rs` records platform-qualified replay; `JIT.md` and this plan publish scope, evidence and remaining requirements.

### Matched feature-cost and shipping-profile decision

Add `examples/jit_feature_cost.rs`, built from identical source without default features and with only `jit` added. Both variants execute the same shared workload definitions as `jit_bench`; the JIT variant retains a runtime-selectable Auto path so its linked size includes a usable compiler, rather than dead-stripping an Off-only toy. Compare default interpreter behavior against compiled-but-Off, asserting zero compilation/native work on every Off case and checking every Lua result. Size numbers describe these instrumented embeddings, not arbitrary downstream executables or library archive sizes.

`make jit-size SIZE_PROFILE=speed|shipping` builds both variants sequentially with identical locked dependencies, target, LTO/codegen/strip settings and opt-level 3 or s. Copy immutable binaries before switching features; record hashes, ELF section sizes, file-size deltas, toolchain, CPU, flags and normal dependency trees under `target/jit-evidence/feature-cost/`. Alternate two fresh worker processes per measured pair; worker-internal timings exclude process startup, source/executor setup on warm cases, and two warmup executions. Cold state construction/loading/execution remains timed. Preserve all raw pairs and dispersion. Strict protocol validation rejects wrong feature/mode, missing/reordered/extra cases, nonzero native work, invalid durations and unchecked results. Eleven pairs and twenty timed repetitions per worker are defaults; checked runs require at least eleven pairs. Enforce the frozen 1.05 maximum compiled-disabled ratio independently for every corpus case, reporting all failures before exiting nonzero. Separate-process/ASLR/hash/layout differences remain measurement caveats, not reasons to waive failures.

Add `make jit-shipping` to publish paired Off/Auto results at the unchanged opt-level-s shipping profile. The existing `jit-performance` remains pinned to opt-level 3; neither a shipping run nor a caller-supplied optimization variable silently changes its frozen attribution gate. Embed the wrapper's optimization label in the benchmark executable; direct Cargo builds without that label report `unspecified`, not an invented opt-level 3. No performance, size or full-plan acceptance is claimed until the new gates execute.

**Initial feature-cost evidence:** focused default/JIT checks and four protocol/corpus/limit tests in each feature configuration pass (`/tmp/luna-jit-feature-cost-focused.log`, `/tmp/luna-jit-feature-cost-tests.log`). The full `nix develop -c make jit-verify` passes (`/tmp/luna-jit-feature-cost-verify.log`): 2490 executions / 404 suite invocations, including the new example tests in repeated modes/docs and supervised smoke. Workflow lint also passes after adding matched shipping artifact builds (`/tmp/luna-jit-feature-cost-ci.log`); CI does not assert a controlled performance result from those builds.

Two standalone `nix develop -c make jit-size SIZE_PROFILE=speed` runs both fail the frozen 5% disabled-cost gate on the float loop (`/tmp/luna-jit-feature-cost-speed.log`, `/tmp/luna-jit-feature-cost-speed-repeat.log`). No concurrent gates/builds ran during measurements. Matched stripped file sizes are 1416480 bytes without JIT and 6426720 with runtime-selectable JIT (+5010240 bytes, 4.5371x). Float Off ratios are 1.1051 / 1.1107 (10.51% / 11.07% overhead). Other median ratios (first / repeat): integer 1.0188 / 0.8457; table 0.8485 / 0.8213; upvalue 1.0113 / 1.0325; metamethod 1.0396 / 1.0343; callback 0.9221 / 0.9208; allocation/GC 0.9268 / 0.9235; Oslo 0.9941 / 0.9954; cold 1.0144 / 1.0078. Raw pairs show host/process dispersion; apparent wins are not attributed to JIT machinery while runtime Off. The repeated float failure is an additional acceptance gap, not a reason to relax the control. Shipping feature-cost and paired native measurements remain pending at this checkpoint.

**Shipping feature cost:** two standalone `nix develop -c make jit-size SIZE_PROFILE=shipping` runs also exit 2 (`/tmp/luna-jit-feature-cost-shipping.log`, `/tmp/luna-jit-feature-cost-shipping-repeat.log`). Matched stripped file sizes are 1144040 bytes without JIT and 4151040 with JIT (+3007000 bytes, 3.6284x). Both runs fail integer, table and upvalue disabled-overhead controls. Median ratios (first / repeat): integer 1.1107 / 1.1232; float 0.9568 / 0.9751; table 1.0819 / 1.0858; upvalue 1.1244 / 1.1166; metamethod 1.0122 / 1.0154; callback 1.0305 / 1.0344; allocation/GC 1.0062 / 1.0046; Oslo 1.0007 / 0.9970; cold 1.0038 / 0.9945. These profile-dependent differences are not a controlled causal diagnosis of dispatch overhead.

The shipping repeat also executes the copied JIT artifact in Auto before measurement, proving checked results and nonzero native counts on all eight warm cases; the cold case remains interpreted. `nix develop -c make jit-cost-native SIZE_PROFILE=speed` separately passes with the same assertions (`/tmp/luna-jit-feature-cost-speed-native.log`). Both native proof records are retained in the profile artifact directories. `jit-size-build` remains build-only for hosted CI; `jit-cost-native` performs the runtime proof and `jit-size` composes that proof with the frozen disabled-cost control.

**Shipping native measurements:** two standalone `nix develop -c make jit-shipping` runs pass all result/native/cold-policy assertions and exit 0 because this reporting lane does not enforce the opt-level-3 acceptance exit (`/tmp/luna-jit-shipping-paired.log`, `/tmp/luna-jit-shipping-paired-repeat.log`). Both executables report `opt_level=s`. Off/Auto median ratios (first / repeat): integer 3.0710 / 2.8682; float 5.3889 / 5.2746; table 1.1180 / 1.2540; upvalue 0.5342 / 0.5305; metamethod 0.8416 / 0.8363; callbacks 0.7299 / 0.7541; allocation/GC 0.8347 / 0.8211; Oslo 0.9039 / 0.8982 (unscored); cold 0.9999 / 1.0157. Warm Auto executes native work, cold Auto makes no compilation request. A reporting command's zero exit is not numerical acceptance; table/GC vary around displayed reference controls and upvalue/callback cases regress persistently. The original opt-level-3 failures and new compiled-disabled failures remain open.

**Final focused gate:** `nix develop -c make fmt-check ci-check jit-cost-tests TARGET=x86_64-unknown-linux-musl jit-evidence` passes (`/tmp/luna-jit-feature-cost-final.log`). All four protocol/corpus/limit tests execute in each musl feature configuration. This is not a new full musl gate or an ARM64/hosted result. Evidence copying retains every separate first/repeat log; both profile directories retain matching hashes/sections/flags/dependency trees/native proof. `git diff --check` passes after the final documentation/security update.

#### Latest session handoff: matched feature costs and shipping

**Goal:** continue full `PLAN_JIT.md` implementation on `feat/native-jit`; keep the goal active until the entire original acceptance contract is proven.

**Instructions:** use Nix/Make and patch tools; preserve default interpretation, source/weak identity rules, original scope and frozen controls. The maintainer delegated corpus selection. No commits, pushes, releases, remote CI dispatches or kernel-permission changes were made.

**Discoveries:** same-binary Off/Auto timing does not measure the cost of compiling the JIT feature at all. Matched identical-source binaries expose a repeatable disabled float regression at opt-level 3 and integer/table/upvalue regressions at the shipping profile. Profile-specific code/layout and process dispersion prevent causal attribution from those ratios alone. Retaining a runtime Auto path and executing its native counters prevents the size result from measuring a compiler-dead-stripped Off-only toy.

**Accomplished:** shared frozen workload definitions; strict checked subprocess feature-cost protocol with four default/JIT tests; matched stripped/hashes/sections/dependency/CPU/profile artifacts; alternating raw pairs and frozen 5% control; copied-artifact native proof; correctly labeled shipping benchmark wrapper and CI shipping-artifact builds. Full correctness gate passes (2490 executions / 404 suites). Two runs each of speed/shipping feature-cost gates fail as recorded, and two shipping reporting runs complete without implying acceptance. Formatting/whitespace/workflow checks pass.

**Next steps:** repair compiled-Off regressions and the four original opt-level-3 controls with controlled dispatch/helper profiling and differential tests; do not relax ceilings or silently avoid the claimed paths. Continue compiler/fixed-owner/combined-host resource limits, eviction/backoff/scheduling tests, full heap/lifecycle and coverage-guided fuzz, unsafe/Miri review and generated-code diagnostics. Execute ARM64/hosted CI and complete compilation/latency/coverage metrics before release acceptance.

**Relevant files:** `examples/jit_support/workloads.rs` owns shared Lua cases; `examples/jit_feature_cost.rs` owns identical-source probes and strict comparison protocol; `examples/jit_bench.rs` uses shared sources and embeds the optimization label; `Makefile` adds size/cost/native/shipping targets and keeps the speed gate pinned; `.github/workflows/tests.yml` builds matched shipping artifacts; `README.md` removes the outdated security premise that JIT/timing callbacks do not exist and retains the explicit lack of side-channel isolation; `JIT.md` and this plan publish evidence and non-acceptance.

### Rejected compiled-Off experiments and instruction profiling

Three compiled-Off dispatch experiments were tested and rejected below. The current VM retains its original single-loop/function/result structure and final metrics update; no const-generic specialized helper or macro-expanded duplicate dispatch remains. Keep the new mode-switch/fuel/GC regression and the instruction-profile lane. No performance ceiling or native coverage requirement was weakened.

**Rejected first form:** keeping the manager statistics update inside the specialized loop passes full correctness (2495 executions / 404 suites, `/tmp/luna-jit-off-dispatch-verify.log`) and the new mode-switch/64-instruction/GC test, but `make jit-size SIZE_PROFILE=speed` regresses badly (`/tmp/luna-jit-off-dispatch-speed.log`): integer 1.5515, float 1.3457 and upvalue 1.0782 disabled ratios. Do not accept that form merely because the native branch is constant-folded. Move accounting to the wrapper and remeasure; exact error/zero-budget/transition accounting must remain unchanged. The prior sizes/ratios are historical checkpoints, not evidence that this experiment meets the 5% ceiling.

**Rejected revised form:** moving accounting outside the const-generic loop still fails six controls (`/tmp/luna-jit-off-dispatch-accounting-speed.log`): integer 1.5294, float 1.3767, table 1.0955, upvalue 1.0984, metamethod 1.0655 and allocation/GC 1.1145. Both specialized-function forms are removed. Keep the new Off/Auto/Off/Auto/Off slice test, which proves unchanged fuel, exact Off metrics, no Off lookup/compilation/native work and GC-safe resumption.

**Profiler-backed replacement experiment:** add `make jit-cost-profile PROFILE_CASE=float_loop`, using symbol-retained matched opt-level-3 binaries and [Callgrind function-scoped collection/cache/branch simulation](https://valgrind.org/docs/manual/cl-manual.html). Valgrind is supplied by the Nix shell; no host perf permissions change. Warm-only `--case` is diagnostic and rejected in checked comparison mode; default comparisons still require all nine cases. Collection is restricted to `*run_vm*`; the gate rejects an empty profile and preserves raw instruction-level and annotated artifacts. These are simulated instruction/cache/branch events, not hardware cycles or acceptance timings.

The restored pre-specialization float loop profile passes (`/tmp/luna-jit-cost-callgrind.log`, `target/jit-evidence/feature-cost/speed-symbols/`). Total collected instructions: 47412165 without JIT versus 48609580 compiled-Off; conditional branches: 4416210 versus 4841405. VM-exclusive instructions: 23608660 versus 24806075. Arithmetic/coercion/decode event counts match between variants; the increase is in the VM body. This localizes extra dispatch work but does not prove the hardware-time cause. Hoist the native eligibility choice once inside the original VM function, emitting native-aware and pure interpreter loops from one private macro body. Preserve the original function/result ABI, final metrics update and per-op Auto observation semantics; no const-generic helper or separately maintained interpreter is retained. Verify the replacement with all correctness/cost/native controls before accepting it.

**Rejected same-function macro hoist:** focused fourteen native/three reference/eleven heap tests pass, but the cost gate again fails six controls (`/tmp/luna-jit-off-loop-hoist-speed.log`): integer 1.5188, float 1.2039, table 1.1698, upvalue 1.1045, callback 1.0780 and allocation/GC 1.0775. Restore the original loop rather than retaining an apparently branch-free but slower implementation. Removing source-level branches alone is not acceptance evidence. Subsequent profiler outputs are case-separated under `speed-symbols/callgrind/<case>/`, preserving hashes/settings and requiring a positive collected instruction count.

**Final profiling and restored correctness:** the original VM plus retained mode-switch regression passes full `nix develop -c make fmt jit-verify` (`/tmp/luna-jit-off-profile-final-verify.log`): 2495 passed executions / 404 suite invocations, including repeated modes/docs and supervised smoke. `make jit-cost-profile` then passes again in its final case/mode-separated form (`/tmp/luna-jit-cost-callgrind-final-off.log`); float counts reproduce the 47412165 / 48609580 instruction totals and 4416210 / 4841405 conditional branches. The events are saved under `target/jit-evidence/feature-cost/speed-symbols/callgrind/float_loop/off/`.

`make jit-cost-profile PROFILE_CASE=closure_upvalue PROFILE_MODE=auto` also passes (`/tmp/luna-jit-cost-callgrind-upvalue-auto.log`). Auto executes 298428 native logical instructions, verifies the result and retains unknown-address generated-kernel events alongside Rust helper symbols. Collected instructions are 77861273 for the no-JIT interpreter versus 145362502 for Auto. Auto-exclusive costs include `Runtime::invoke::<8>` at 26227703 instructions (18.04%) and the VM at 29273162 (20.14%); Get/SetUpvalue helper specializations each consume 3574368 instructions. This identifies invocation/materialization/helper work as measured optimization targets, not proof that a particular fusion/cache change is correct or profitable. Call/return counts/costs remain visible rather than being omitted. These Valgrind runs are not platform certification or numerical performance acceptance.

**Restored-loop cost check:** a standalone `make jit-size SIZE_PROFILE=speed` with the diagnostic-capable probe still exits 2 (`/tmp/luna-jit-off-profile-restored-speed.log`), failing metamethod (1.0796) and callback (1.0502) controls. Other ratios: integer 0.9530; float 1.0406; table 0.9938; upvalue 1.0208; allocation/GC 0.9969; Oslo 0.9947; cold 0.9987. Sizes are 1417600 / 6427792 bytes. This is one run with a changed probe/link layout, not proof that the historical float regression was fixed by a retained runtime optimization. No dispatch experiment remains, and disabled-cost acceptance is still failed.

The final `nix develop -c make fmt-check ci-check jit-native jit-cost-tests TARGET=x86_64-unknown-linux-musl jit-evidence` passes (`/tmp/luna-jit-off-profile-final-focused.log`): fourteen native integrations and four probe tests in each feature configuration, plus formatting/workflow validation. This focused musl result is not a replacement for ARM64/hosted certification. All experiment/profile/restored-cost logs are copied into ignored evidence storage, and final `git diff --check` passes.

#### Latest session handoff: rejected dispatch experiments and scoped profiles

**Goal:** fully implement the original plan on `feat/native-jit`; full acceptance remains unproven and the goal stays active.

**Instructions:** Nix/Make and patch tools only; do not relax frozen controls, drop Off metrics, hide poor native candidates, change interpreter defaults or weaken source/weak identity rules. No commits, pushes, releases, hosted CI dispatches or kernel-permission changes were made.

**Discoveries:** source-level branch removal is not a reliable performance result: two const-generic forms and same-function macro hoisting pass semantic gates but worsen checked feature-cost controls, so all are removed. Scoped simulated profiles localize extra Off dispatch events and substantial Auto invocation/helper work. Unknown-address generated-code events are retained but not mistaken for symbolized disassembly or hardware cycles.

**Accomplished:** exact Off/Auto mode-switch, tiny-fuel and per-slice GC regression with retained Off metrics and real Auto work; rejected bad dispatch changes; added Nix Valgrind and Make-backed matched/symbol-retained case/mode-scoped profiles; isolated diagnostic selection from the checked nine-case comparison; full restored correctness gate and successful float-Off/upvalue-Auto profiles. Results and artifacts are recorded above.

**Next steps:** use the measured invocation/helper/dispatch costs for targeted optimization with per-PC/budget/effect/fuel proofs, rather than another unmeasured restructuring. Original native controls and compiled-disabled controls remain failed. Continue compiler/fixed-owner/combined-host limits, eviction/backoff/scheduling tests, broader lifecycle/heap fuzz, coverage-guided/unsafe/Miri review, generated-code diagnostics and ARM64/hosted evidence. No phase or release acceptance follows from profiler availability.

**Relevant files:** `tests/jit_native.rs` adds mode-switch/counter/fuel/GC coverage; `src/thread/vm.rs` retains the original loop after rejected experiments; `examples/jit_feature_cost.rs` selects one warm diagnostic case but rejects selection in comparison mode; `Makefile` supplies symbol-retained matched profiles, case/mode artifacts and empty-profile rejection; `flake.nix` supplies Valgrind; `JIT.md` and this plan record scope and findings.

### Commit authorization and progress checkpoint

The user authorized commits on 2026-09-30 and requires incremental, logical
milestone commits rather than one final implementation commit. Use unsigned,
title-only Conventional Commits. Pushes and releases remain unauthorized.

#### Session summary

**Goal:** report actual plan progress and checkpoint the existing implementation
in separate runtime, verification, tooling and documentation commits.

**Instructions:** commit logical milestones as work proceeds; retain the existing
Make/Nix, patch-only editing and unsigned title-only commit requirements.

**Discoveries:** all eleven phases still have open acceptance items. Passing
correctness gates does not mean performance or release acceptance.

**Accomplished:** real native execution, extensive differential verification,
resource ledgers, benchmark/profile tooling and x86-64 GNU/musl evidence exist.
The latest full gate reports 2495 executions across 404 suite invocations.

**Next steps:** repair failed native and compiled-Off performance controls;
complete compiler resource bounds, hardening, transition/lifecycle coverage and
ARM64/hosted execution evidence. The full implementation goal remains active.

**Relevant files:** `src/jit/` and runtime integration implement the native tier;
`tests/` verifies boundaries and embedding behavior; `Makefile`, `examples/` and
`.github/workflows/tests.yml` supply gates; `JIT.md` and this plan retain findings.

### Register materialization experiment

The upvalue Auto profile identifies `Runtime::invoke::<8>` as a substantial
cost. Test direct scalar write-back with no read or rewrite of canonical GC
references, rather than copying the existing `Value` through `Slot::value` on
every register. Helpers still store new references in the traced frame, scalar
operands still reconstruct from scratch, and every exit/panic materializes the
same prefix. No value layout, ABI, GC barrier, PC or fuel contract changes.
Acceptance requires scalar bit-pattern/reference identity tests, existing
boundary/heap integration gates and sequential paired performance evidence;
remove the runtime change if the measured result does not justify it.

**Rejected:** focused ABI/boundary, fourteen native and eleven heap tests pass,
including panic materialization. Sequential baseline and candidate opt-level-3
performance gates both fail the same four frozen controls. Upvalue speedup
changes from 0.5691 to 0.5448 and table from 1.1191 to 1.0633; there is no measured
justification to retain the change. Restore the original write-back. Keep three
explicit ABI regressions for scalar bit patterns (including negative zero and
NaN payload), every GC-reference kind/canonical identity, and invalid tags.
Logs: `/tmp/luna-jit-materialize-{baseline,candidate,focused}.log`. These ratios
are separate paired runs, not a controlled hardware-level causal attribution.

### Finalized native-code diagnostics

Add a test-only diagnostic lane, `make jit-disassembly`, that compiles scalar
loop and table/helper fixtures, copies their relocated bytes while each owning
module remains live, and disassembles those bytes at their actual entry address.
Record Lua source, decoded PC/entry flags, constants, ABI/target, helper symbol
addresses, executed instruction counts, hashes and environment alongside the
assembly. The scalar fixture must execute its generated kernel to the expected
5050 result; the null-host table fixture must decline before allocation. Both
modules must reclaim their native mappings after the dump. No diagnostic state
or file-writing path is added to production builds. Raw disassembly may include
embedded data; recorded helper addresses aid manual correlation, not automatic
symbolization or an unsafe-code correctness proof. ARM64 support still requires
execution on an ARM64 host.

**Verified:** `/tmp/luna-jit-materialize-diagnostics-verify.log` exits 0 for
`make fmt jit-disassembly jit-verify`: 2511 passed executions / 405 suite
invocations, including the explicitly invoked dump test (2510 / 404 for the
full correctness gate alone). GNU target-qualified dumping plus formatting and
workflow validation passes (`/tmp/luna-jit-diagnostic-gnu.log`); musl dumping and
all fourteen native integrations pass (`/tmp/luna-jit-diagnostic-musl.log`).
Seeded fuzz smoke artifacts are retained at
`target/jit-evidence/fuzz/1790777755199176270-2704807`.

Prepopulating a separate diagnostic directory with valid old binaries and
metadata, then substituting `CARGO=true`, exits 2 before disassembly rather
than accepting stale output (`/tmp/luna-jit-diagnostic-stale-refusal.log`).
The executed-test and architecture checks are part of the gate. Nix supplies
binutils explicitly, and `jit-platform` now runs the diagnostic, retaining its
output in the existing native CI upload. Hosted/ARM64 execution is not claimed.

#### Current session handoff: materialization and kernel diagnostics

**Goal:** continue the full plan and report verified progress without claiming
release acceptance.

**Instructions:** retain frozen controls; commit logical milestones separately.

**Discoveries:** direct reference-skipping write-back did not improve the
targeted performance controls. Four native controls still fail; the runtime
experiment was reverted.

**Accomplished:** committed three ABI regressions as `7f6605c`. The new finalized
kernel diagnostic passes locally: scalar code is 1904 bytes and executes 207
native instructions to return 5050; table code is 1104 bytes and declines a null
host before allocation. Source/metadata/assembly artifacts were inspected.
The interrupted full command completed successfully and was polled to terminal
status, not restarted. GNU/musl diagnostic and stale-artifact refusal checks
pass as recorded above; the diagnostic milestone is ready for its own commit.

**Next steps:** targeted invocation/helper optimization with measured acceptance
evidence; full compiler/combined-host limits, eviction, lifecycle and unsafe
hardening, and ARM64/hosted runs. Performance/resource/hardening/ARM64
requirements remain open. No tool process remains live from this milestone.

**Relevant files:** `src/jit/abi.rs` adds committed ABI tests;
`src/jit/backend.rs`, `Makefile`, `flake.nix`, `JIT.md` and this plan contain the
separate diagnostic milestone. No release or push has been performed.

### Bounded cache-pressure eviction decision

Implement deterministic LRU eviction for installed, unleased modules only.
Update recency at installation and successful per-slice lookup using a
saturating logical clock; break saturated-clock ties by prototype generation.
After a native-mapping quota refusal, permit at most one eviction and compiler
retry in a service call, and only when the prototype's existing lifetime
attempt budget has room. Count each failed compiler invocation, retirement and
no-victim refusal. Do not evict for snapshot/metadata/capability/compiler errors.
Keep the same source identity; reset an evicted prototype's hotness but not its
attempts, preventing unlimited cache-thrash recompilation. Explicit cache clear
retains its documented attempt reset. If every module is leased, decline
eviction and preserve the interpreter fallback; leases remain charged and
executable. Tests must cover recency, deterministic victim choice, bounded retry,
pinned refusal/retirement, actual native execution and final reclamation.
This policy is not a hard compiler CPU or working-memory ceiling.

**Verified:** six private cache tests pass: LRU recency, saturated-clock tie
breaking, exhausted admission, no eviction for metadata refusal, leased-code
refusal/retirement/reclamation, and a failed oversized retry capped at one victim
and two failed compilations. The public cache-pressure test preserves three
live closures, proves native execution of surviving/replacement modules,
interpreter fallback of the victim, GC safety and final zero resource usage.
Allocation/protection-denial tests explicitly assert no pressure eviction.
`make jit-policy` now includes the cache tests, and paired benchmark reports
expose both new cumulative cache counters.

Full `nix develop -c make jit-verify` exits 0: 2545 executions / 404 suite
invocations (`/tmp/luna-jit-eviction-verify.log`), with supervised smoke artifacts
at `target/jit-evidence/fuzz/1790779028665404578-2790667`. Focused musl policy,
all fourteen native integrations and finalized-kernel dumping pass
(`/tmp/luna-jit-eviction-musl.log`). Focused GNU evidence is in
`/tmp/luna-jit-eviction-final-focused.log`. These are correctness/resource
results, not numerical performance acceptance.

#### Session summary: bounded pressure eviction

**Goal:** implement the missing cache-pressure policy within the original full
JIT plan and commit the verified milestone separately.

**Instructions:** keep native leases, attempt limits, source identities and
frozen performance controls intact; use Make/Nix and unsigned milestone commits.

**Discoveries:** retries need their own attempt charge and failure counter even
when eventual installation succeeds. Evicted sources must retain attempts to
prevent an unlimited cache-thrash recompilation loop. A failed oversized retry
can leave its one victim interpreted; there is no transactional restoration.

**Accomplished:** bounded deterministic unleased LRU retirement/retry, cumulative
diagnostics, six cache unit tests, one public GC/fallback/native-effect test,
full GNU correctness and focused musl evidence. Documentation and phase status
are updated without claiming complete Phase 3/7 or release acceptance.

**Next steps:** repeat native/compiled-Off performance gates on an idle machine;
unrelated Magi Cargo/Rust compilation was live during the final resource checks,
so no concurrent timing run was started. Continue invocation/helper optimization,
compiler/combined-host limits, sparse-cache compaction, scheduling failure
injection, broader lifecycle/unsafe/fuzz hardening and ARM64/hosted execution.
No Luna tool process remains live from the correctness gates.

**Relevant files:** `src/jit/mod.rs` implements recency/eviction/retry and private
cache tests; `tests/jit_policy.rs` verifies the public lifecycle; backend denial
tests forbid eviction; `Makefile` runs the cache suite; `examples/jit_bench.rs`
reports cache counters; `JIT.md` and this plan document behavior and limitations.

### Compact invocation-counter experiment

Test eight byte-sized private helper counters instead of eight `u64`s in the
scoped helper frame. Every validated kernel checks budget before each logical
operation, caps budget at 64, and exits immediately on helper decline/panic.
Each logical operation makes at most one helper call, so each local counter is
at most 64. The opaque helper host and ABI v3 remain unchanged; public cumulative
statistics still use saturating `u64` arithmetic. Verify a helper-dense 64-op
slice and cumulative totals beyond 255, plus existing panic/fuel/heap tests.
This is an experiment, not performance acceptance; retain only with evidence.

The source constructor/assignment sequence includes non-helper instructions;
257 allocations did not produce an all-helper slice. Use 256 constant-key raw
table writes instead, require an observed 64-helper slice, and compare tiny-fuel
steps against the interpreter with GC between slices. Check cumulative writes
and helper totals beyond 255 rather than assuming one helper per source statement.

Add separate `jit-bench-build` and `jit-bench-run` targets so artifact compilation
can finish before idle-machine timing begins. The existing combined target
retains its interface; run-only supports a copied immutable artifact through
`JIT_BENCH_BINARY`. Checked benchmark execution rejects artifacts whose embedded
optimization label is not `3`, preventing accidental shipping-profile checks.
These controls do not themselves establish an idle machine or performance
acceptance. Preserve both counter-width artifacts and their hashes.

**Rejected:** sequential run-only paired checks of copied opt-level-3 artifacts
both fail the same four frozen controls. Baseline `u64` versus candidate `u8`
speedups: integer 2.4065 / 2.4813; float 4.4255 / 4.4972; table 1.0969 / 1.0850;
upvalue 0.4958 / 0.4996; metamethod 0.7554 / 0.7705; callbacks 0.6694 / 0.6521;
allocation/GC 0.9733 / 0.9863; Oslo (unscored) 0.8556 / 0.8469; cold 0.9956 /
1.0068. Upvalue improvement is negligible and callback timing worsens; do not
retain counter narrowing merely for its smaller frame. Restore the original
counter types and aggregation. The dense-helper GC/fuel regression remains.

Logs: `/tmp/luna-jit-compact-counts-{u64,u8}-performance.log`; artifact hashes
are in `target/jit-evidence/compact-counts/binaries.sha256`. Both binaries were
built before timing, and Cargo/rustc process checks before and after each run
found no concurrent jobs. These are separate paired runs with dispersion, not
causal hardware profiling or performance acceptance. No ceiling was relaxed.

**Next measured target:** source inspection confirms successful LRU lookup now
performs an additional hashed `Tracking` lookup through `touch`. Investigate
storing recency in the charged cache-map entry, reusing the successful code
lookup. Preserve deterministic eviction, source identities, exact counters and
lease safety. The observed post-LRU upvalue slowdown alone is not causal proof.

**Restored verification:** `/tmp/luna-jit-compact-counts-restored-verify.log`
exits 0 for `make fmt jit-verify`: 2555 passed executions / 404 suite invocations,
including the new helper-bound and checked-profile tests. Seeded smoke output
is at `target/jit-evidence/fuzz/1790781107301822079-2911773`. All twelve heap
integrations pass on musl with original counters restored
(`/tmp/luna-jit-compact-counts-restored-musl.log`). Run-only rejects a missing
artifact with exit 2 (`/tmp/luna-jit-bench-run-missing.log`). No runtime counter
narrowing or aggregation change remains in the worktree.

#### Session summary: rejected compact counters and isolated benchmark stages

**Goal:** reduce measured invocation overhead without weakening the full plan's
correctness, accounting or frozen performance contract.

**Instructions:** keep only justified optimizations; commit logical verified
milestones, using Make/Nix and patch tools.

**Discoveries:** local helper counts are bounded by 64, but source statements
need not map one-to-one to helper instructions. Counter packing alone does not
repair the upvalue/callback performance failures. Successful LRU lookup currently
adds a hashed tracking probe; investigate that measured-path overhead next.

**Accomplished:** reverted unhelpful counter packing; retained a helper-dense
64-call slice/cumulative-256-write GC/fuel regression; separated benchmark build
and timing with immutable-artifact support and explicit checked-profile tests;
captured both failing paired reports and hashes; full restored GNU correctness,
focused musl heap coverage and missing-artifact rejection pass.

**Next steps:** remove avoidable recency lookup work without weakening charged
metadata, pinning or LRU tests; repeat native/compiled-Off gates on idle hardware.
Complete compiler/combined-host limits, sparse-cache policy, lifecycle/fuzz/unsafe
hardening and ARM64/hosted evidence. The full goal is still incomplete; no tool
process from this session remains live.

**Relevant files:** `tests/jit_heap.rs` holds the new regression; `Makefile` and
`examples/jit_bench.rs` split/validate benchmark execution; `JIT.md` documents the
interface; this plan retains rejected-experiment evidence and the next handoff.

### Single-probe LRU recency decision

Move recency from source `Tracking` into a `CachedCode` value in the existing
budget-allocated cache map. A successful `get_mut` can update recency and clone
the code lease without a second hashed tracking lookup; eviction scans can read
recency directly. The larger cache-map value is charged by its allocator rather
than adding unaccounted state to native `Code`. Source tracking shrinks again.
Keep the saturating clock, generation tie break, attempt/queue state, one-victim
retry, exact counters and all lease/retirement semantics. Compare copied matched
baseline/candidate artifacts and rerun cache, quota, heap and full gates. This
does not waive the still-failed native/compiled-Off performance controls.

**Retained candidate evidence:** two sequential copied-artifact comparisons,
with order reversed for the second, improve the upvalue Off/Auto speedup from
0.4849 / 0.4916 (tracking recency) to 0.5823 / 0.5908 (cache-entry recency).
Table moves from 1.0535 / 1.0347 to 1.1717 / 1.1773. Both variants still fail
table/upvalue/metamethod/callback controls; these are targeted improvements,
not performance acceptance. Other candidate first/repeat ratios: integer
2.4123 / 2.8095; float 4.5684 / 4.5561; metamethod 0.7952 / 0.7751; callbacks
0.6914 / 0.6953; allocation/GC 1.0167 / 1.0328; Oslo (unscored) 0.8516 /
0.8972; cold 1.0002 / 0.9915. Raw paired dispersion is retained, including the
first candidate integer sample minimum of 0.7823; no sample is discarded.

Logs: `/tmp/luna-jit-cache-recency-{tracked,entry}-{performance,repeat}.log`;
matched copied artifact hashes are in
`target/jit-evidence/cache-recency/binaries.sha256`. An earlier attempted run
was deferred before any timing because an unrelated Cargo job was active;
completed runs found no Cargo/rustc jobs before/after each artifact. Two new
tests prove miss/hit clock/counter semantics and quota charging of cache-entry
recency. Existing deterministic LRU, lease, retirement and retry tests remain.
Full GNU `make jit-verify` passes: 2565 executions / 404 suite invocations
(`/tmp/luna-jit-cache-recency-verify.log`), with supervised smoke artifacts at
`target/jit-evidence/fuzz/1790782459805833980-2987580`.

**Compiled-Off speed-profile control:** two complete `make jit-size
SIZE_PROFILE=speed` runs pass all nine unchanged 5% controls
(`/tmp/luna-jit-cache-recency-feature-cost{,-repeat}.log`). First/repeat ratios:
integer 0.9900 / 0.9980; float 0.9754 / 0.9732; table 0.9798 / 0.9856; upvalue
1.0485 / 1.0464; metamethod 1.0469 / 1.0449; callbacks 1.0295 / 1.0275;
allocation/GC 0.9666 / 0.9697; Oslo 0.9739 / 0.9711; cold 1.0012 / 1.0118.
Matched binary sizes are 1417600 / 6426496 bytes. Both artifacts retain checked
results, and the JIT artifact's Auto proof reports actual native work before
the disabled comparisons. Upvalue/metamethod ratios remain close to the ceiling;
retain raw dispersion and repeat after further changes. The shipping profile
still requires a current passing result. Disabled execution does not perform
the removed active-cache probe, so do not causally attribute these Off timings
to that probe alone; source-tracking layout and linked code also changed.

#### Session summary: single-probe cache recency

**Goal:** remove avoidable LRU bookkeeping overhead while preserving the full
plan's metadata, cache, lease and acceptance contracts.

**Instructions:** keep deterministic eviction and exact counters; measure
copied artifacts sequentially on an idle machine, retain all samples, and
commit the verified milestone separately.

**Discoveries:** recency belongs in the charged cache-map entry rather than an
extra tracking probe. Two tests demonstrate its allocation charge and unchanged
miss/success clock/lease semantics. Source-tracking metadata shrinks, while
installed cache entries grow under the same allocator.

**Accomplished:** retained single-probe lookup and direct-recency eviction scans;
two-order comparisons improve upvalue and table ratios but still fail four
native controls. Full GNU verification passes (2565 executions / 404 suites),
and both speed-profile compiled-Off controls pass all nine workloads. Existing
cache/pinning/refusal/GC tests remain applicable; documentation is updated.
Focused musl policy, all fourteen native and twelve heap integrations pass
(`/tmp/luna-jit-cache-recency-musl.log`), with formatting/workflow validation.
This is not a newly executed full musl gate or ARM64 certification.

**Next steps:** continue short-frame/helper optimization, shipping-profile acceptance,
compiler/combined-host bounds, sparse-cache policy, broader lifecycle/unsafe/fuzz
hardening and ARM64/hosted execution. Full plan acceptance is still unmet; no
process from this milestone remains live after final checks.

**Relevant files:** `src/jit/mod.rs` holds charged cache recency and new tests;
`src/jit/model.rs` adapts the explicit lease fixture; `JIT.md` and this plan
describe the behavior and evidence. Ignored copied benchmark artifacts preserve
baseline/candidate hashes and raw timing reports.

## 15. Primary references

- [Cranelift project and backend scope](https://cranelift.dev/) — native code generator, targets, and security caveats; not a Lua runtime.
- [Cranelift IR](https://github.com/bytecodealliance/wasmtime/blob/main/cranelift/docs/ir.md) — validate against the pinned version, not moving-main assumptions.
- [Cranelift frontend / FunctionBuilder](https://docs.wasmtime.dev/api/cranelift/prelude/struct.FunctionBuilder.html) — SSA construction and stack-map facilities; their existence does not integrate Luna's collector automatically.
- [LuaJIT language/runtime compatibility](https://luajit.org/extensions.html) — why this plan is not a transparent LuaJIT integration.
- [LuaJIT hooks and sandboxing caveats](https://luajit.org/faq.html) — compiled-loop interruption and hostile-input limitations.
- Local `README.md`, `COMPATIBILITY.md`, `src/dump.rs`, and the tests listed above — the actual Luna compatibility and execution contract.

## Key Learnings:

1. Luna's executor can host a native tier without replacing its object model, but canonical frame state and arena lifetime rules constrain every native boundary.
2. The VM advances the PC before effects and charges some transitions differently; bailout and fuel semantics need explicit characterization, not a generic instruction counter.
3. Native code generation, collector integration, nonblocking compilation, and proof of actual native execution are separate acceptance responsibilities.
4. The first integrated scalar tier executes real native instructions and preserves measured slice/fuel behavior, but heap/callback-heavy workloads need additional work rather than relaxed performance gates.
5. Source-defined standard-library functions create legitimate weak registrations in core states; empty-state tests are necessary to isolate provenance and collection behavior.
