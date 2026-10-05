# Native JIT: experimental scalar and heap tier

This branch implements the first native execution tier described in [PLAN_JIT.md](PLAN_JIT.md). It is **not the finished plan** and is not the LuaJIT runtime or its FFI. Constructors still default to interpreted execution. Do not use these results to claim production readiness or hostile-code isolation.

The 2026-10-05 frame-free scalar candidate (`711ae8a`) was measured and disabled:
the full 24-block comparison fails the unchanged upvalue, callback and disabled
cost gates. Direct Auto upvalue gains are only 1.012215x speed and 0.972175x
shipping; callbacks worsen. Test-only kernels, ordinary native fallback,
optional admission/late-refusal recovery and reference-result coverage checks
remain. GNU/musl correctness checks pass after rollback. The upvalue, callback
and compiled-Off regressions are still open; `PLAN_JIT.md` records the rejected
candidate, immutable evidence and remaining structural work.

As checked on 2026-10-04, all seven hosted jobs pass at `d054ba8`
([run 37168811569](https://github.com/onix-os/luna/actions/runs/37168811569)):
native Linux x86-64 GNU/musl and ARM64 GNU, two Rust-only Miri seeds, baseline
verification and real i686 interpreter fallback. This includes the
oversized-image admission repair, relocation-copy accounting and requested-byte
diagnostics, integrated mock, signature/helper-symbol accounting and anonymous
private entry, plus the host-ISA refusal fix described below.

Local full GNU/musl gates at `d054ba8`, including host-ISA refusal handling, each
pass 5378 executions across 482 repeated suites, with 24 ignored and zero failures.
These are repeated test executions, not unique tests or performance measurements.

The test harness makes Force refusals explicit, with exact IR-block
exclusions for the existing math and string corpora. Both GNU/musl examples
assert native execution and return 5000050000 with 200007 native instructions.
Dynamic numeric-exit tests also pass in jit/jit+async on GNU/musl, and dispatch
accounting covers errors and transitions on GNU/musl and real i686.
The local full run includes
baseline, all JIT modes, optional features, doctests and the existing bounded
smoke corpus; it does not establish current ARM64 or Miri evidence by itself.
The host-ISA refusal fix also passes focused GNU/musl checks (573 passing
executions each, four ignored) and real i686 fallback (30 passing executions).
Hosted run `37168811569` supplies that revision's actual ARM64 and Rust-only
Miri evidence, independently of the local GNU/musl results. Only documentation
changed after the tested runtime revision.
`PLAN_JIT.md` retains
the revision-specific evidence, compatibility reconciliation and acceptance limits.
Repeated under-load measurements now cover five windows: 120 batches per native
profile and 60 compiled-Off batches per profile, with all nine workloads and
eleven paired samples. `PLAN_JIT.md` records medians, observed percentile spread,
window variation and pass counts without discarding outliers. An idle CPU is
not required to collect useful estimates. Speed-profile median ratios are
2.68x integer, 4.67x float, 1.23x table, 0.74x upvalue and 0.82x callback
(Off / Auto). The last two regress consistently across windows. Compiled-Off
controls also remain unmet; repeated measurements do not erase systematic
host bias or substitute for passing the frozen gates. This is not full-plan
acceptance.

The 2026-10-05 native-entry continuation trials were also rejected after 288
interleaved timing commands: small direct Auto gains did not fix upvalues, and
disabled execution worsened. Neither runtime change is retained. The new
guard-fallback test requires one lookup, native entry and guard exit without
replaying the attempt. `PLAN_JIT.md` records both candidates and their controls;
the original performance regressions remain unresolved.

## Enable and prepare

Enable the optional `jit` Cargo feature and set `JitConfig.mode` to `JitMode::Auto`. See `examples/jit.rs`; run it with `nix develop -c make jit-example`.

The native dependency set uses Rust 1.97.1 in the Nix shell and CI; the pinned
Cranelift and instruction-cache crates declare Rust 1.96.0 as their minimum.
`make environment` reports the selected local toolchain. Direct native-only
dependencies, as recorded in `Cargo.toml`, `Cargo.lock` and their package
manifests, are:

| Dependency | Locked version | Declared license |
| --- | --- | --- |
| `cranelift-codegen`, `cranelift-frontend`, `cranelift-jit`, `cranelift-module`, `cranelift-native` | 0.136.1 | Apache-2.0 WITH LLVM-exception |
| `wasmtime-internal-jit-icache-coherence` | 49.0.1 | Apache-2.0 WITH LLVM-exception |
| `memmap2` | 0.9.11 | MIT OR Apache-2.0 |
| `libc` | 0.2.189 | MIT OR Apache-2.0 |

Compiler/cache/mapping versions are exact manifest pins; `libc` uses the 0.2
requirement and the lockfile version above. This is the direct native dependency
inventory, not a transitive license audit. Default builds do not enable the JIT
compiler dependencies.

### Compiler dependency maintenance

Keep the Cranelift family on matching exact versions and update the manifest
and lockfile together. Before accepting an upgrade, check the upstream release
notes and advisories for the pinned compiler, mapping and instruction-cache
crates. Recheck the native-builder flags, signature/symbol/relocation allocation
contracts, provider ownership and executable-memory finalization assumptions
documented here. Run the existing native platform gates on GNU, musl and ARM64,
Rust-only Miri and interpreter fallback; record the new revision and results.
An affected native backend stays disabled in consuming applications until its
fix passes the applicable gates. This is a maintenance policy, not an automated
advisory monitor, response-time commitment or claim that these pins are free of
known defects. Source-only JIT provenance remains mandatory: dependency updates
do not authorize native compilation of dumped or manually constructed bytecode.

### Execution and preparation

The example asserts the Lua result and nonzero preparation, native-instruction
and code-memory counters on supported targets. On unsupported targets it stays
Off and asserts the same result with zero native counters. Build eligibility is
not permission to map executable memory: explicit preparation errors propagate
from this example rather than silently passing an interpreted run as native.

Host instruction-set detection uses the fallible `cranelift_native::builder`
path. Its ordinary capability refusals return `JitError::Unavailable`; they do
not invoke the panic used by the pinned upstream convenience constructor.
Convenience execution remains interpreted after refusal, and previously cached
code is preserved. The replacement retains the pinned native feature detection,
speed optimization, verifier, non-colocated libcalls and x86-only PIC settings.
This is not a catch-all for compiler panics, signals or allocation failure, and
does not enable loading code produced for another CPU. Refusal tests inject
detector errors; they are not tests on physically unsupported CPUs.

Source loaded through `Closure::load` / `load_with_env` gets private weak prototype registrations, including nested functions. `prepare_jit()` queues a bounded batch of registered sources and compiles that queue outside the GC arena. Keep closures or executors stashed so collection cannot retire their source before preparation. A batch is limited by `max_queue_entries`; call again for further prototypes. Binary chunks and manually constructed prototypes remain interpreted.

Manual `Executor::step` only queues hot compilation requests. Call `Lua::service_jit()` **outside** the arena between steps; one call handles at most one prototype. It creates a size-limited owned snapshot in a separate arena mutation, releases the arena, compiles, then installs on the owner thread. `Lua::finish` / `finish_async` service queued work between execution slices. Ordinary `Lua::enter` never invokes the compiler. Convenience finishing can therefore spend compiler time; manual stepping never does. There is no compiler worker or hard compiler timeout yet.

`clear_jit_cache()` retires installed code and queued work without changing Lua objects. Disabling Auto also retires code/work and resets admission attempts. Lowering code/snapshot/prototype limits conservatively retires installed code; queue/attempt reductions discard pending requests exceeding the new limits. Active invocation leases remain executable and charged until released. Raising a limit alone does not reset exhausted attempts; explicitly clear the cache to retry. No native artifacts are persisted or included in `string.dump`.

Native-mapping quota pressure can evict one least-recently-used, unleased module
and retry compilation once, provided the requested prototype still has an
attempt available. Installation and successful lookup update recency; saturated
clock ties use prototype generation. Eviction resets hotness, not attempts.
Recency is stored in the budgeted cache-map entry; a successful lookup updates
it while obtaining the lease, without another tracking-table probe.
An evicted prototype can warm again while its lifetime attempt budget allows;
otherwise it stays interpreted until explicit cache clearing. Leased modules
are never pressure-eviction victims. If all candidates are leased, no code is
retired. An image whose own page-rounded segments exceed the entire code-cache
limit returns `ResourceLimit("native image size")` without evicting a peer or
consuming a retry; increasing the limit permits another attempt if its budget
is not exhausted. Alignment padding and all provider segment kinds count.
Metadata/snapshot/capability/compiler errors do not trigger eviction.
A service call still handles at most one prototype, but can make two backend
compilation attempts. This is bounded retry, not compiler preemption.
`make jit-policy` checks exact hotness promotion/saturation, exhausted-attempt
blacklisting, queue cancellation, pending-state destruction and short-lived
source generations that never compile. A blocked compiler in an independently
owned state must not stop manually stepped executors in another state; this
does not make synchronous `service_jit` nonblocking for its own caller.
`cache_evictions` counts pressure retirements; `cache_eviction_refusals` counts
mapping-pressure retries refused because no unleased victim exists.
`compilation_failures` counts each failed backend attempt, including failures
recovered by eviction and retry.

## Current execution coverage

`nix develop .#miri -c make jit-miri` uses the separately pinned nightly
2026-08-16 toolchain for Rust-only resource, scalar/reference ABI, helper,
registry and policy unit tests. It records environment/setup/per-namespace logs
under `target/jit-evidence/miri/<target>/`. Default isolation, alias checking
and leak checking remain enabled. The exact resource test
`installation_refusal_reclaims_generated_mappings` is excluded because it
finalizes executable code (`mprotect`), which this Miri lane cannot execute;
ordinary native resource gates still run it. This is neither generated-code
validation nor a complete unsafe review or soundness proof.

Generated code handles scalar move/load, truth testing, integer/float add/subtract/multiply, float division, exact same-type and mixed numeric comparisons, branches without closing upvalues, and bounded numeric for loops. Integer arithmetic wraps; string coercion, floor division/modulo, and complex operations fall back to the existing interpreter. Mixed comparisons retain all integer bits, handle negative fractional ties, and return false for NaN without trapping on float-to-integer conversion.

Native code also calls scoped, barrier-aware helpers for reference move/constant load, table allocation/read/write, and open/closed upvalue access. Fresh raw-value, metatable, readonly, and interception checks admit operations that need no user callback; all operations requiring metamethod dispatch decline before effects. Weak table accesses use the same weak-upgrade/store APIs as the interpreter. Dedicated counters and tests prove successful native heap operations, not just scalar prefixes.

Upvalue joins can target the executing frame's own locals. Same-stack access
uses the register split at `base`, not an assumption that all cells precede the
frame. Native helpers read the current scratch value for such aliases and write
both canonical storage and scratch; aliases outside the scratch prefix use
canonical storage. Closed and foreign-stack cells retain their existing paths.
Fixed bytecode write masks are not complete dynamic helper clobber sets;
upvalue writes can affect any current-frame slot. Generated calls are memory
fences in the pinned backend, and subsequent instructions reload guarded slots.

Calls, returns, actual metamethod/user callback invocation, close tracking/unwinding, coroutines, and async transitions still run through explicit interpreter exits. Native execution does not recursively call Lua on the native stack or keep a generated frame across suspension. Hook-enabled slices remain interpreted. Integrated GC/lifecycle tests and bounded ownership/barrier review are recorded in `PLAN_JIT.md`; broader compiler/unsafe-boundary review, current full-platform verification and performance acceptance remain open.

Every generated operation checks the remaining reference slice allowance before executing; a native invocation completes at most 64 logical bytecode instructions. A guard failure leaves the PC before the unperformed instruction. Completed scalar writes are materialized into the same canonical Lua registers, and the interpreter immediately makes progress without repeating completed work. Fuel retains the interpreter's existing approximate transition charges, including minimal progress with exhausted or interrupted input.

`make jit-numeric-exits` compares Off/prepared execution for 24 dynamic-operand
cases at five fuel budgets, with GC after every slice. It covers wrapping integer
arithmetic, floor division/modulo signs and minimum-integer edges, integer-zero
errors, floating zero/infinity/NaN, signed zero and string coercion/errors. Exact
slice traces, results and error strings agree; floats are compared by bits except
that NaN payloads are normalized. Each prepared run must perform exactly 201
native table writes, proving native work both before and after the numeric
operation or its caught error. This proves mixed-tier behavior, not native
lowering of floor-division/modulo/coercion paths that intentionally remain interpreted.

## Resources and counters

Before code generation, scalar-tag verification checks actual scratch stores and
their opcode-specific tag admissions. Constants and phi inputs supply tag unions;
slot loads require dominating, single-predecessor guarded edges to exclude
references. Merged/bypassed guards, reference-contaminated phis, unrecorded stores
and unsupported memory-store forms are refused. Input slot tags are the five ABI
v3 tags produced by canonical packing and helpers, not arbitrary external native
buffers. Numeric-consumer records additionally require Integer or Numeric tag
admission, adjacent tag/payload loads from the same slot (or constant pairs), and
an actual SSA dependency on the recorded payload. Compound Boolean guards use a
bounded predicate evaluator; arbitrary non-Boolean operands supply no proof.
Exact expected record counts reject missing obligations. Explicitly marked F64
conversion selectors require semantic checks: each possible
numeric tag must select signed integer conversion or Number bitcast of the same
recorded payload. Equivalent inverted selector/arm polarity is accepted; unknown
conditions, swapped arms, unsigned conversion and foreign payloads are refused.
Source-derived exact float-record counts prevent reclassifying away the proof.
Add/Sub/Mul/Div records additionally bind decoded PC/arm order, exact opcode,
left/right register or constant identities, verified float-conversion inputs,
and the result's adjacent destination tag/payload stores. Input-record ordering
and a maximum of two records per consumer bound lookup work. Arithmetic loads,
stores and bitcasts require the emitter's default native memory flags. These
records use the same snapshot ledger and reject missing/extra obligations.
Not/Test records bind canonical source reads to a bounded typed Lua-truth
expression: only nil and false are false. Logical-not payload/tag stores and
Test's decoded polarity, branch targets and one-step fuel increment are checked.
Eq/Less/LessEq records bind both source operands to exact same-type and mixed
numeric expressions, including signed comparisons, fractional ties, IEEE
ordering and saturating conversions at the +/-2^63 boundaries. Actual split
edges, the two phi inputs, final branch polarity/targets and one-step fuel
increment are verified. Comparison record storage is exact-counted and charged
to the snapshot ledger; missing obligations and growth are refused.
NumericForPrep/NumericForLoop records bind source registers to typed subtraction,
addition, signed-overflow suppression and directional integer/IEEE limit tests.
The nonzero-step retry guard, integer/float split, phi triples, unconditional
index store, taken-only visible-variable store and decoded jump/fallthrough fuel
increments are checked. Unexpected stores in the verified loop arms and finish
blocks are refused. Loop records are exact-counted and quota-charged; these
local checks do not complete whole-program path, alias or liveness proof.
Scalar Move/LoadConstant, LoadBool/LoadNil and non-closing Jump records verify
source values, destination stores, nil-range ordering, Boolean skips, decoded
targets and one fuel increment. Empty nil ranges still require a checked edge.
Move's direct path requires its actual non-reference split. Unexpected stores
in these scalar finish blocks are refused. Write/edge records are exact-counted
and snapshot-quota-charged.
Helper records independently decode all nine source helper kinds, including
reference moves/constants, and bind each physical direct call to its imported
function, host/slot pointers, six-argument ABI, operand encodings and source PC.
Completion/panic tests must use the actual returned status; success edges require
the decoded next PC and one-step fuel, while panic/decline edges preserve the
source PC and unchanged count. Missing/extra calls, unexpected stores in helper
call/status blocks, and record growth are refused before code generation.
Exact-counted helper records use the snapshot ledger and can refuse with
`ResourceLimit("helper call verification")`. This local IR check does not prove
runtime helper effects/borrows/GC or global source paths.
The four shared exit handlers independently bind the entry ABI, distinct handler
blocks and typed PC/count parameters to three exact default-flag stores at the
Exit ABI offsets, canonical reason and empty return. All physical returns must
belong to these handlers. Direct output-pointer accesses outside them and its
escape through branch arguments are refused. This pass uses fixed-size stack
storage and a bounded linear IR scan without new allocated records. It proves
local handler output, not correct source/fuel on every predecessor or global paths.
Entry dispatch uses an independently verified unsigned I64 PC range test before
I32 reduction into a dense jump table. Its ordered trampolines enter each decoded
PC's header with zero count; unknown PCs return their full original PC and zero
work. Each header contains only its source-PC constant, unsigned count-versus-
budget test and conditional exhausted/body edge. Exhaustion preserves source PC
and count; each operation body has only its own header as predecessor. Exact-
counted trampoline/body records are snapshot-quota-charged before host compiler
setup and can refuse with `ResourceLimit("entry path verification")`. Compiler
jump-table storage remains outside the incomplete ledger. These checks
do not independently establish opcode equivalence, alias/liveness, runtime
materialization or compiler resource isolation.
Source-region ranges additionally classify every physical block and branch.
Private blocks must be dominated by their own operation body; private edges stay
within that source region and move forward, preventing uncharged private cycles.
Cross-source edges must be decoded successors with one fuel increment. Shared
retry exits require an admitted kind, exact source PC and unchanged count.
A snapshot-quota-charged byte workspace propagates scalar stores across every
private predecessor and rejects retry/guard exits after any path writes; refusal
is `ResourceLimit("source path verification")`. Helper declines/panics retain
their independently verified status flow and canonical runtime effect contract.
This structural path proof does not prove helper effects/borrows/GC, full
instruction equivalence/source maps, alias/liveness or complete compiler accounting.
Semantic records additionally bind every declared instruction and value definition
to its physical source region, including scalar stores/inputs, arithmetic, truth,
comparison, loop, transfer and helper records. Generic store/input PCs are charged
in their existing exact-counted vectors. Store masks/destinations must match
decoded source access; default-flag I64 tag/payload stores must form exact adjacent
pairs. Every physical scratch payload store must belong to one recorded pair,
refusing orphan payload writes that common tag admission alone does not detect.
These scans reuse the region map without new worklists and reject unknown value
unions rather than infer foreign ownership. Binding does not independently prove
every opcode's expression, optimizer correctness, runtime helper/GC behavior,
alias/liveness or full compiler accounting.
Standalone predecessor queries use a Luna-owned sorted edge vector charged to the
snapshot ledger before allocation. Terminal branch destinations are counted
before reservation; repeated destinations from one instruction are deduplicated
without releasing retained capacity charges. Predecessor order matches Cranelift's
instruction order, including unreachable and non-layout block queries. Entry,
helper, transfer, comparison, loop and scalar guard checks use this graph.
Verification dominance uses snapshot-charged node, iterative DFS stack and
postorder vectors; only nodes remain after construction. Storage refusal reports
`ResourceLimit("frontend dominance storage")`. Construction is limited to
`64 * (DFG blocks + reserved raw predecessor edges + 1)` charged steps, refusing
with `ResourceLimit("frontend dominance work")` before codegen. This bounds the
analysis, not total compiler CPU time. Backend internals and frontend builder
storage remain outside the ledger; fixed owners are accounted below. This is not complete
compiler working-memory or RSS accounting.
The retained, fallible PC-to-block map is charged to the snapshot ledger and
can refuse with `ResourceLimit("frontend block map")` before compiler/host setup.
Semantic checks are
vacuous only on guarded paths proven unreachable without queried-tag assumptions;
physical store coverage and record counts still apply. This does not prove full
payload selection, operation equivalence, liveness, aliasing or complete typed SSA.
Luna-owned store/input records and worklists use the snapshot ledger and can report
`ResourceLimit("scalar tag verification")`. Record-envelope violations are compiler errors and
never silently grow verification storage.

Completed full and incremental GC cycles retire dead source registrations and
their cached code even while JIT is Off. Live closures keep their registrations;
partial cycles can defer retirement until completion. GC performs no compilation.
The disabled `service_jit()` fast path remains unchanged.

- `max_prototype_instructions` and `max_snapshot_bytes` bound snapshot admission before copying bytecode. Native scratch uses exact 1–8-slot specializations, then the smallest fitting 16/32/64/128/256-slot tier, bounded to 256 scalar slots. Zero-register entries use an empty initialized prefix. Scratch-bearing routines are non-inlined, so compiled-but-Off VM entries do not reserve their 4 KB maximum.
- `max_ir_instructions` (1048576) and `max_ir_blocks` (65536) independently cap a conservative frontend expansion envelope before graph allocation or Cranelift setup. Actual frontend instruction/block counts must fit the admitted envelope before code generation. The estimate can reject a prototype below the source-opcode ceiling; these limits do not bound optimizer CPU time, compiler working memory or RSS. Lowering either limit retires cached code and pending requests; existing leases retain their normal lifetime policy.
- `max_relocations` (65536) caps the actual compiled function's relocation records before module relocation conversion/copying or native mapping allocation. Excess records return `ResourceLimit("native relocations")` and leave interpretation and other installed modules usable. Admitted records are converted in a fallible snapshot-ledger staging vector; refusal returns `ResourceLimit("native relocation staging")`. The module's separate relocation copy is admitted against that ledger before definition using its checked `ModuleReloc` array layout; refusal returns `ResourceLimit("native relocation copy")`. Its reservation outlives module destruction, including errors/unwinding. Cranelift installs the bytes and relocations with its existing linker/provider, then the staging vector and codegen context are dropped before finalization. The checks follow code generation: they do not bound Cranelift's earlier machine-relocation buffer, other compiler working memory or compilation time. The copy accounting follows pinned Cranelift 0.136.1 and Rust 1.97.1 slice-clone allocation; review that contract on upgrades. Quota refusal is recoverable, but the upstream clone still uses infallible standard allocation. Lowering the cap retires installed code and queued work without invalidating active leases. Raising it alone does not reset failed attempts; clear the cache to retry.
- `max_queue_entries` bounds pending identity requests. Saturating hotness and `max_compile_attempts` limit repeated failures; clearing the cache resets attempts.
- `max_code_bytes` bounds actual page-rounded JIT mappings, including the compiler's code/readonly/writable segments. Retired but pinned mappings remain charged until the last lease drops. Allocation failure frees partial mappings and leaves interpretation usable.
- `max_metadata_bytes` independently bounds requested allocation layouts for weak registrations, tracking/code-index maps, pending identities, preparation ID buffers, native-entry flags and mapping records. Containers use a shared fallible allocator; retained capacity and temporary old/new growth are charged. Registration refusal leaves ordinary source loading/interpreting usable; metadata allocation refusal is typed `ResourceLimit("JIT metadata")`. Lowering this ceiling retires registrations, code and requests; existing closures continue interpreted, and new source loads can register after limits are raised. Active leases keep their entry flags/mapping records and remain charged until their final owner drops, even above a newly reduced quota. Ordinary cache clearing retains live source registrations.
- Owned operation/constant snapshot vectors have a separate allocator and `max_snapshot_bytes` ceiling; their charges follow the vectors' actual lifetime and release on success, refusal or panic. Compiler workspace, known helper-symbol/ABI-array reservations, relocation staging and the known module relocation-copy reservation share that ceiling and the host parent quota. `metadata_bytes`/`snapshot_bytes` report current requested/reserved storage, and their peak fields retain the high-water reserved usage. `metadata_allocation_refusals` counts quota/underlying allocation refusals; `registration_refusals` counts optional source registrations declined on allocation pressure. Empty retired containers release capacity.
- Cached code uses a private strong-only shared owner whose count, allocator handle, padding and `Code` payload are one exact budgeted allocation. Allocation is fallible before publication; refusal frees the finalized image and does not evict a live peer. Cache retirement retains the charge until the final lease drops. State bootstrap owners are charged separately below.
- Compiler/provider error status uses one fallible metadata-charged atomic owner rather than three uncharged std Arc flags. Its count, allocator handle, padding and flags are charged before JITBuilder setup and retained through provider/module reclamation. Error precedence remains metadata, mapping quota, provider unavailability, then ordinary compilation failure. The atomic owner requires a Send + Sync value for either auto trait; checked clone increments and an AcqRel final decrement govern lifetime. The cached-code owner remains single-threaded.
- The provider adapter box and its shared mutex handoff are allocated fallibly and charged by exact typed layouts before JITBuilder setup. The initialized Global adapter allocation transfers to Cranelift's required standard Box; its charge outlives box destruction. After successful relocation/protection, the complete mapping owner moves into Code, the handoff becomes empty, and the module/adapter/charge are destroyed before publication. Failure drops the unclaimed mapping owner with the last handoff handle. Cached Code retains no ISA, symbols, declarations, compiled blobs or finalization queues. Other transient Cranelift-owned compiler buffers remain unaccounted.
- Each provider allocation owns one anonymous memmap2 mapping in a fallibly budgeted Segment vector. There are no per-segment SystemMemoryProvider private record vectors. Page-rounded padding for over-page alignment is charged before mapping, and an aligned payload-end guard precedes pointer creation. Zero-size requests reserve a live page; invalid alignment/overflow refuses before record allocation. Finalization does not grow record storage. Executable maps clear cache before RX conversion, readonly maps become R, writable data remains RW; feature-gated Linux ARM64 BTI and the pinned compiler's pipeline flush are preserved.
- Metadata and snapshot allocators share a state-owned host ledger with page-rounded mappings. Parent and individual quota admission both precede allocation; temporary old/new growth and failed-provider rollback apply to the parent too. `accounted_jit_bytes`/`accounted_jit_peak_bytes` report current/high-water reservations across those categories; `host_allocation_refusals` counts parent quota refusals. Mapping pages do not consume the metadata quota.
- `Lua::total_memory()` remains collector-only. Additive `Lua::accounted_memory()` includes collector allocations plus the charged JIT categories; without the JIT feature the two metrics are equal. `set_memory_limit` applies to accounted usage. JIT admission uses headroom refreshed at host/arena/GC boundaries, with typed `ResourceLimit("host memory")` from public preparation/service. Lowered limits deny growth without invalidating active leases. Removing a limit restores admission; individual JIT ceilings remain in force.
- Synchronous/asynchronous finish checks reclaim idle cached code and collect twice before stopping an over-limit executor. They check before running more work, before final completion and before polling a parked foreign future. Manual `Executor::step` remains host-controlled. GC allocation within a slice can overshoot, and finalizer/host allocations are not made fallible by this policy.
- Mapped-page usage is an inline atomic in the existing host root ledger, not a separately allocated counter Arc. Private `MappingCounter` handles resolve child ledgers to that root without allocating and preserve it through detached code leases. Mapped usage and charged combined-host usage remain separate counters; actual mappings still reserve both quotas before allocation and release both after unmapping.
- Ledger and runtime owners use private exact-layout, strong-only Global allocations. `bootstrap_bytes` reports their live layouts; `accounted_jit_bytes` and the host ceiling include them. The normal state-owned floor is three ledger layouts plus the runtime layout, retained after code/prototype cleanup. Both reserve the same root atomic as payloads before allocation; refusal rolls back and final deallocation precedes charge release. The runtime's RefCell preserves non-Send/non-Sync ownership, with compile-time trait checks. Private fallible factories exist; existing infallible Lua constructors retain their allocation-failure contract.
- This is **not yet a complete compiler ledger**. It excludes allocator overhead, other Cranelift-owned transient allocations and process RSS. Cached code retains the finalized mappings, not the compiler module or its buffers. Luna's entry flags and actual provider mapping-record storage use the shared ledgers; pinned mappings remain charged after retirement. Bounded unleased LRU eviction and fallible sparse-container compaction exist, but complete compiler accounting remains required before Phase 3/7 acceptance. Accounted limits are not an RSS or hostile-compiler ceiling.

Compilation releases verification records, flow graphs and frontend block maps
after verification and before codegen. The frontend context and helper signature
are dropped after lowering; the codegen Context is dropped after the module has
copied code/relocations and before finalization. Source snapshots remain alive
through any bounded eviction retry, then are dropped before cached-owner
installation. `make jit-compiler-lifetimes` checks the charged workspace and
snapshot boundaries, native results, and live-peer preservation on failures.

The ABI parameter-array reservation covers Luna's entry/helper vectors plus
their known Cranelift module-declaration and function-import clones. Initial
vectors use fallible exact reservation. The combined charge precedes their
construction and outlives both compiler Context and Module, conservatively
retaining capacity budget for vectors dropped earlier. Refusal reports
`native signatures` without triggering cache eviction. This accounts for known
array layouts, not Signature-containing tables, symbol strings, ISA/backend
working buffers or every internal clone. Upstream cloning remains infallible;
this is not system-OOM recovery. The contract is checked against pinned
Cranelift 0.136.1 and the Rust vector clone/capacity behavior and needs review
on compiler/toolchain upgrades.
The private entry uses an anonymous local declaration and is defined/retrieved
by `FuncId`. It therefore needs neither an owned entry name nor a name-map
registration. Helper imports remain named. A separate reservation covers their
three retained name payloads and one largest-name sequential lookup temporary,
before builder construction and through module destruction. Initial strings use
fallible exact reservation; refusal reports `native symbols` without eviction.
Private symbol/declaration map capacities and dynamic libcall names are not
covered. Returned `JitError::Compilation` strings and diagnostic formatting are
also outside the ledger; Manager/cache do not retain these caller-owned errors.
Earlier destruction reduces overlapping lifetimes; it does not account for or
bound all Cranelift transient allocations.
At `a0f9ac5`, full GNU/musl gates each pass 5152 test executions across 446
repeated suites, with 24 ignored and zero failures. The focused lifetime/resource
gates pass 77 executions per target. These are correctness/resource results,
not the deferred performance acceptance.
- `total_dispatches` counts completed native bytecodes plus interpreted opcode dispatches, including call/return transitions and failing interpreted operations. Native guard attempts, declined helpers and a panicking native instruction are not counted as completed native work; subsequent interpreter dispatch is counted once. Interpreter dispatches are retained on error or Rust unwinding, using a slice-local accumulator published at scope exit. This is not CPU instructions, fuel or callback-body work, and it saturates independently. Cache clearing/disabling retains it.
- `native_entries` counts real machine-code invocations, including immediate guard exits. `native_instructions` counts completed logical bytecodes, not CPU instructions. `interpreted_instructions` counts the reference VM's reported instructions, which exclude some transition opcodes and slices returning errors. `interpreted_slices` counts completed slices that fetched an interpreted opcode. These existing counters and fuel accounting retain their previous meanings. `hook_exits` counts slices kept interpreted with hooks enabled.
- Native invocations return exactly one of four reasons: `native_interpreter_exits` (handoff for an interpreted operation or helper decline), `guard_exits` (failed scalar guard), `native_budget_exits` (slice allowance exhausted), or `native_panic_exits` (caught helper panic). These totals partition `native_entries` until counters saturate; each saturates independently at `u64::MAX`. Zero-instruction exits still count. Panic exits are recorded after state materialization and before resuming Rust unwinding. Hook-only interpreted slices do not count as native exits. Cache clearing and disabling JIT retain these cumulative counters. `make jit-stats` checks reason accounting, native execution and unsupported-target zeros; the metrics example includes the new reason totals without requiring a timing run to test them.
- `code_lookups` counts eligible slice-local cache probes; `code_leases` counts successful owned leases, even when an entry is interpreted. A single VM slice reuses its lease across native/reference fragments, but repacks canonical registers for every invocation. Hotness observation remains per interpreted dispatch when no code is installed.
- `helper_calls` counts scoped helper attempts, `helper_instructions` counts completed helper-backed bytecodes, and `helper_declines` counts effect-free fallback requests. Table/upvalue/allocation counters count successful accesses; table-through-upvalue operations include a successful upvalue read. These are real native helper paths, not the interpreter's opcode loop.
- `code_bytes` is live page-rounded mapped usage. `code_requested_bytes` is the provider-requested payload total before page/alignment padding, including linker veneers/GOT storage and any read-only/writable segments. It is not just machine instruction bytes. A zero-byte provider request contributes zero requested bytes but still reserves a live page. Both counters follow retired leases through final reclamation. Requested bytes are already included in mapped charges and are not added to `accounted_jit_bytes` again.
- `snapshot_bytes` follows owned snapshot vectors; `installed_regions`, requests, failures, and execution counters are cumulative. Preparation is synchronous, so the host usually observes zero current snapshot usage after it returns. Benchmarks also report current/peak container charges and refusals. `make jit-example` reports both requested and mapped native bytes without timing acceptance.

The backend is compiled for Linux x86-64/aarch64. Executed integration evidence exists on x86-64 GNU, musl and ARM64 GNU at `2e21d80`, as linked above. `supported_target` reports build eligibility, **not successful executable-memory allocation or release platform certification**. Explicit preparation/service reports unsupported targets, typed `ResourceLimit` mapping/metadata/snapshot refusals, native allocation/protection denial as `Unavailable`, or compiler errors. Optional convenience service failures do not become Lua language errors. Test-only injection covers allocation/protection denial after source loading, preservation of another installed module, interpreter fallback and recovery; it does not modify host permissions or expose a script-facing fault option.

## Unsafe boundary review: current scalar/heap tier

1. The v3 generated signature is `extern "C" fn(*mut Slot, u64, u32, *mut Exit, *mut Host)` with the platform's native C calling convention. `Slot` and `Exit` use `repr(C)` and asserted size/offsets. Nine fixed imported helpers use `extern "C" fn(*mut Host, *mut Slot, u32, u32, u32, u32) -> u32`; each specializes a const helper kind. The opaque host carries only the scoped frame pointer, not an indirect callback. Compiler lookup matches explicit unique kind keys, independent of registry order. No Rust enum, `Gc`, frame layout, or arena lifetime is assumed by generated code. There is no persisted native cache or public helper ABI to migrate.
2. Snapshots contain decoded owned instructions and scalar tag/bits constants only. Reference constant placeholders resolve by validated index through the scoped helper's active closure, not cached GC pointers. Register, constant, upvalue, prototype, skip, and jump operands are validated before code generation. Code checks the budget at every instruction entry, including backedges.
3. Each eligible VM frame slice acquires one strong-owned `Code` lease that keeps the detached native mappings alive; the `JITModule` was destroyed before publication. The manager borrow is released before execution. The slice reuses the lease across native/reference fragments and drops it on completion, frame transition, error or unwind; no scratch/host state survives an invocation. Scratch slots, exit buffer, opaque host, and borrowed Rust helper frame outlive the synchronous call. Entry/bounds checks select an outlined scratch tier large enough for the admitted prefix; every used `MaybeUninit` slot is written before a typed slice is formed, and the unused suffix is never read as `Slot`. Reference-result tests cover both sides of every tier boundary including 256. The lifetime-erasing pointer cast is confined to the scoped gateway; no pointer, GC reference, or helper result escapes into cached code. A null host in boundary-model tests declines helper work.
4. Helpers decode scalar operands from scratch and reference operands from canonical traced slots. Each destination synchronizes both representations; table/upvalue setters retain normal barrier APIs. Pending scalars materialize on every exit and before a caught panic resumes. No helper invokes collection, users, hooks, or frame changes. Upvalues can alias the current frame: reads use pending scratch within the admitted prefix, and writes update both scratch and canonical storage. Cells outside that prefix, earlier frames, foreign stacks and closed cells retain their canonical/barrier paths. Old canonical references can remain rooted until exit, but no collection can observe that interval. Any future callback or safepoint helper must restore full synchronization first. No Rust heap layout is assumed by machine code.
5. Each provider allocation owns an anonymous memmap2 handle in a budgeted Segment record. Metadata, page quota and shared parent reservations precede mapping; allocation failure rolls page reservations back. The aligned payload remains within the mapped range. Executable ranges clear cache before RX conversion and pipeline-flush after all segments; readonly ranges become R, writable data stays RW, and no RWX mapping is requested. Linux ARM64 BTI uses the same detected-feature policy as pinned Cranelift. Protection failure never publishes an entry; existing leases pin all maps until module reclamation. The exact optional memmap2/internal-icache dependencies match the pinned compiler versions; the upstream icache crate is an unsupported internal API requiring renewed review on upgrades. Executed ARM64 evidence is revision-scoped; later runtime changes still require the native platform gate.
6. Helper gateway signatures use opaque pointers and fixed-width immediate operands. The gateway catches Rust unwind payloads and returns an explicit panic exit; after native return, Rust resumes the same payload rather than converting it to Lua success or interpreter fallback. No helper invokes user callbacks, Lua code, frame transitions, the compiler, or collection. Rust `panic=abort` still aborts normally. A deliberately conflicting table borrow verifies the unwind profile returns through the generated frame and leaves the state reusable.
7. Helpers advance the canonical PC before effects, restore it when declining before effects, and return at most one completed logical operation per call. Guard failures do not perform a table mutation, user callback, or Lua error before interpreter retry. Fresh metatable contents and interception/readonly flags are consulted on each access; no invalidation cache or table layout assumptions exist yet.
8. Tests hold a code lease across cache retirement and verify memory returns to zero after the last lease drops. Scalar boundary tests compare every entry and budget against an independent Rust model. Heap tests compare per-slice fuel/state with full GC after every slice, prove heap counters, exercise weak tables, readonly/invalid-key typed errors, open/closed upvalues, Rust interception mutation, and reentrant callbacks. Extend this review for every new helper, cached assumption, or frame transition.
9. The private cached-code owner has no weak handles or raw-pointer escape API. A checked `Cell` count governs immutable shared access; its marker preserves single-thread ownership. Construction obtains an initialized typed allocator-api2 box and transfers its allocation/allocator. Final destruction uses the same typed layout and allocator with a deallocation guard, including value-destructor unwind. Alias/alignment/count/identity/drop behavior is covered by selected Miri; generated-code safety still depends on the preceding ABI, source and lease checks.
10. The compiler-status atomic owner is likewise strong-only with no mutable/raw escape API. Its generic deallocation guard is shared with the single-thread owner, using the actual inner type's layout. Cloning uses checked Relaxed atomic update; dropping uses AcqRel decrement and only the last owner destroys the value. Send and Sync implementations both require Send + Sync values; the budget allocator's ledger handles are atomic. Dedicated concurrent lifetime/destructor-visibility fixtures run under selected Miri, without executing generated code.
11. The provider-box transfer uses BudgetAllocator's Global allocation with the concrete initialized payload's exact layout, then forms one standard Box owner from its raw pointer. The separate charge never deallocates that pointer; standard Box deallocates it, including value-destructor unwind, before charge release. The charge is declared before provider/builder/module locals and released after their destruction, on success as well as failure. Exact layout, alignment, borrowed/ZST values, Send trait-object erasure and scope/value panic cleanup are covered by selected Miri. The private charge must not be dropped before its transferred box; this is not an arbitrary-allocator conversion API.
12. Mapping handoff uses a metadata-charged AtomicShared mutex slot. Taking empties the slot once; subsequent adapter allocation/finalization refuses, and adapter free/drop cannot release the detached image. All executable/read-only/writable segments and their quota reservations move together without copying addresses or code. Cached Code owns Memory directly; final lease destruction drops mappings before quota release. Poisoned mutex recovery retains the initialized owner rather than leaking it. The pinned JITModule has no Drop implementation or external unwind registration that must survive finalization; Luna does not permit unwinding through generated frames.

## Verification and remaining work

Current-frame alias repair (`710dd33`) passes full GNU/musl
`nix develop -c make jit-verify clippy jit-clippy`: each reports 4941 passing
executions across 434 suites, with 24 ignored tests. Existing 141 lib-test
Clippy warnings remain. Focused helper Miri passes seven tests with default
checks; this is not a fresh full-selected-Miri or generated-code lane.
The native regression compares seven scenarios, per-step fuel/mode and full GC,
including scalar/reference writes, stale-table declines, returned closed cells,
tail calls and coroutine yield/resume. Native counters prove the alias writes.
Read bypass fails two pure tests and produces 1 instead of 42; write bypass
fails one pure test and reproduces 41 instead of 42. Both fixes are required.
Long GNU/musl scalar and heap campaigns pass; artifacts and restored-source
hashes are under `target/jit-evidence/current-frame-upvalues/`.
This closes a reproduced alias bug, not the full safety/performance review.

Finalized-image detachment (`f875233`) passes full GNU/musl
`nix develop -c make jit-verify clippy jit-clippy`: each reports 4915 passing
executions across 434 suite results, with 24 ignored tests. Counts repeat mode
and doc suites, not unique tests. Selected Miri passes 288 tests across 42 suites
with default checks; it does not execute generated code. Existing 141 lib-test
Clippy warnings remain. Long campaigns on each platform pass 4096 scalar/
admission cases and 96 heap/lifecycle cases; platforms reuse seeds/programs.
Evidence is under `target/jit-evidence/detached-image/`. Native fixtures prove
exact retained runtime-storage charges, execution after module destruction,
and detached-provider refusal without releasing the image. Retaining the
temporary handoff deliberately fails the retained-storage oracle.
The first full GNU attempt failed in the linker; the first full Miri attempt
failed with ENOSPC after a namespace passed. Neither is accepted. Successful
retries use a separate build volume, unchanged toolchains/profiles/checks.
No current performance acceptance follows; transient compiler allocations,
remaining safety/hardening and ARM64/hosted evidence are still open.

Frontend helper declarations, entry parameters and helper constants now use
fixed stack arrays (`f4d2ea9`). Focused GNU and musl gates each pass 18 tests
across four suites; isolated Miri passes 12 tests across two suites. Reversing
declaration order fails three fixtures, including early-error and panic-prefix
cleanup checks. Evidence is under `target/jit-evidence/frontend-arrays/`.
These are focused results, not a new full-suite or performance acceptance.
Cranelift-owned transient signatures, IR/context and module buffers remain outside the
completed compiler-memory policy.

The complete state-bootstrap owner accounting implementation (`c385b10`)
passes `nix develop -c make jit-verify clippy jit-clippy` on x86-64 GNU and
with `TARGET=x86_64-unknown-linux-musl`: each reports 4855 passing tests across
434 suite results and 24 ignored tests. Selected pure-Rust/pure-IR Miri checks
pass 278 tests across 40 selected suite results with default `MIRIFLAGS`; they do not execute
generated native code. Clippy retains the existing warning backlog, so these are
not strict-warning acceptance. Raw revision-scoped evidence is stored locally in
`target/jit-evidence/runtime-owner/`. Four runtime-owner fixtures verify exact
charge retention/final release, quota and underlying refusal/input cleanup,
exact-ceiling admission with borrow-unwind recovery, and compile-time non-Send/
non-Sync traits. A detached native lease stays callable after runtime destruction
while retaining its own root/mapping quota; only the freed runtime/snapshot-owner
charges disappear. Runtime charge bypass fails three owner fixtures and the Lua
floor check. Six Global-owner fixtures verify exact
alignment/borrowed layout, clone/drop, checked overflow, refusal, destructor
unwinding and concurrent final-destructor visibility. Three bootstrap fixtures
verify exact root/nested-header lifetimes, one-byte-short/underlying refusal and
concurrent header/payload admission through one atomic ceiling. A Lua fixture
proves the layout-derived ledger-plus-runtime floor survives cache clear and prevents
growth below that ceiling. Header-release bypass fails two fixtures; quota
bypass fails two Lua checks and the exact/one-byte-short native preparation
check. Relaxed final-drop mutation triggers a Miri data race; deallocation bypass
triggers six Miri leak errors. Default Miri checks were not disabled.
Three pure counter fixtures cover nested
root identity, clone/update sharing, independent roots, lifetime and separation
from host reservations. A native fixture proves quota retention and executable
lease lifetime after runtime destruction, final reclamation and recompilation.
Root-resolution bypass fails both pure and native fixtures. The counter removes
one distinct allocation, not all bootstrap owners. Two pure request tests cover page/alignment/payload
bounds and overflow; six provider fixtures cover real RW/RX/R permissions, fixed
record storage across finalization, padding/zero-size admission, record refusal,
partial protection failure and physical reclamation. The isolated reclamation
worker probes every page before/after free without allocating after free. Initial
musl /proc-observer failure is retained separately: its String allocation reused
the freed address. Padding, RX-to-readonly and reclamation bypass mutations are
detected; no mutated mapping is executed as native code.
Six transfer tests cover exact Global layout,
alignment/borrow/ZST behavior, Send trait-object erasure, quota/underlying refusal,
and scope/value panic cleanup. Exact pre-host and live-module fixtures distinguish
provider admission from the earlier status allocation, preserve a native peer and
recover the same snapshot. Transfer-admission and charge-release bypass mutations
are detected. Concrete outer provider and actual mapping-record storage are now
charged. State-bootstrap owners are also charged; other compiler working storage
remains outside this milestone.
Seven atomic-owner tests include concurrent
clone/drop, final-destructor visibility without external synchronization, exact
alignment/lifetime/accounting, panic cleanup, allocation refusal and checked
overflow. Three status tests and a pre-host sentinel verify budget admission and
error precedence. A live-module fixture proves native peer preservation and
same-snapshot recovery. Budget bypass and relaxed-drop mutations are detected;
the initial weaker refusal fixture is retained as draft rather than proof.
Seeded scalar/admission campaigns additionally
pass 4096 generated cases per platform, each checking 1641780 native invocations
against the independent slice model; both platforms reuse the same four seeds.
Separate six-family heap campaigns pass 96 cases per platform, comparing 6344
main-executor slices, 725 yields and 1633 callback effects, with 65297 completed
native instructions, 964 consumed host reads and table/allocation/upvalue counters.
The 2809 userdata observations are repeated state samples, not individual native
userdata-instruction coverage. Nested/finalizer internal slices are not separately
paired. The same seeds and parameterized families
are reused on both platforms; this is not independent unique-program coverage.
Miri does not execute these heap/native programs. Performance thresholds, ARM64 execution,
complete compiler accounting and the full plan's remaining proofs are not accepted.

`nix develop -c make jit-metrics` builds a separate opt-level-3 scheduling probe.
Use `ARGS='--mode all --samples 3 --fuel 64'`; `--case` selects a shared benchmark,
`oslo_predicate`, `cold_config`, `cache_churn`, `coroutine_resume` or `foreign_await`.
The Make build includes `jit,async`; explicit `foreign_await` selection in a
JIT-only build reports an error rather than silently skipping the case.
Build without timing using `jit-metrics-build`,
then measure the artifact with `jit-metrics-run`. `jit-metrics-tests` checks
argument validation and observation accounting. Logs, CPU/toolchain/profile
configuration and the binary hash are under `target/jit-evidence/metrics/`.

Each fresh-state sample verifies its result and reports source-load duration,
explicit preparation-batch duration, service cost, first observed native work,
logical VM coverage, maximum observed step work/fuel debit, executor-only and
host-enter latency, queue occupancy and existing GC/JIT memory ledgers. The
Oslo case loads once and checks 10000 alternating rows. Off and cold Auto must
not claim native execution; warm Auto and Prepared must actually execute it.
Installed-region counters are checked around every step: compilation service
is outside the arena. This probe does not replace paired performance gates.

Preparation can include core-library prototypes, not just the selected script.
Service cost includes maintenance, snapshots and backend installation. Native
timestamps are post-slice observations; host-enter latency includes collector
work. Fuel is approximate. Work and coverage use the existing `run_vm`
accounting, not callback work: operations that leave the frame (calls, returns
and metamethod transitions) break before its completed-work
increment. A high native fraction therefore does not mean those transitions
are compiled or inexpensive. Full opcode/transition coverage remains open.
Memory peaks are observed at host boundaries except the existing
metadata/snapshot ledger peaks. Fixed owners are included in `bootstrap_bytes`
and accounted JIT usage. Cranelift transient allocations, allocator overhead and RSS remain
excluded. These measurements are neither hard CPU limits nor complete memory
accounting.

The shared suspension scenarios use hot threshold one and three table-update
segments separated by two coroutine yields or foreign awaits. Off, Auto and
Prepared all verify the same results and table identity. Prepared executes native
table writes in every segment; Auto must do so after each resumption. With fuel
1/64, Auto also executes native work before the first suspension. With fuel
65536, that first cold segment finishes interpreted before the host can service
its queued compilation. The probe reports that zero honestly rather than
compiling inside a slice. Full collection runs at
each suspension (four completed cycles for coroutines, twelve for awaits).
Foreign futures are polled outside the arena, with exactly six Pending polls,
two Ready polls and six counted wakes per session. VM instructions and installs
must not advance during those external polls. This uses a synthetic delayed
future, not a network/timer benchmark or hard scheduling limit.

`suspension_case` rows report native segments, source/preparation/service cost,
time to first observed native work, maximum step work and fuel debit, latency,
forced GC and separate external poll cost, coverage and observed resources.
Counters exclude frame-transition opcodes as above; one whole executor step may
contain more than one 64-operation VM invocation. `make jit-suspension` checks
both feature configurations and all modes at fuel 1, 64 and 65536. These tests and
observations alone do not complete the mixed-tier transition/error matrix;
the Phase 5 checklist in `PLAN_JIT.md` records the combined evidence.

`make jit-sequences` checks direct Rust `Sequence` continuations with and without
async enabled: Pending, nested Lua call/error, yield and host resume. Exact
Off/native slice traces, retained stack prefixes, error identity and trailing
nils are checked with collection between slices. Per-phase table-write counts
prove native work before, inside and after the continuation. `jit-suspension`
also checks normal return/break/goto/fallthrough close handlers, and
`jit-upvalues` checks nested Rust-driven executors reading and writing open and
closed cells. These are correctness checks, not timing acceptance.

The same gate compares successful non-tail recursion at depths 128 and 4096
against the interpreter, plus tail recursion at depths 128 and 16384. It checks
exact slice modes/fuel/results with collection between slices and requires
native execution. Callback stack-address samples every 128 levels must stay
within a 32 KiB span. Non-tail Lua frames intentionally consume managed memory;
only tail recursion checks depth-independent arena usage. Separate tests retain
the configured frame-limit error and unbounded-tail-loop fuel checks. Run with
`ARGS='-- --nocapture'` to retain the observed stack spans. These checks concern
Lua recursion; recursively reentering executors from Rust callbacks can still
grow the host stack.

The separate `cache_churn` case supports fuel 1..=64. It calibrates a scalar
module in a separate native state, reports that calibration cost, then bounds
the measured state to two modules, eight retained sources, one queued request
and two attempts per source. Warm/revisit/steady/reset passes verify results,
actual native executions, eviction, exhausted retry budgets and recovery after
explicit cache clear. Final collection must reclaim source registrations and
all accounted code/metadata/snapshot storage in Off as well as native modes.
`churn_report`, `churn_pass` and `churn_cleanup` rows keep this protocol separate
from ordinary workload rows. Calibration is not compilation in the Off state.
These observational timings do not change the frozen paired performance gates.

Before backend generation, an owned instruction-level
CFG validates every operand and successor, including unreachable instructions.
An in-place linear-time basic-block partition records half-open bounds in charged
nodes. Branch/merge targets and interpreter continuations are block headers;
interpreted operations are singleton barriers. Compiler edge checks reject
nonsequential entry into a block interior. Arbitrary legal instruction-PC entry
remains available to slices; block analysis does not remove native entries or
introduce register caching across helpers. This is not a typed SSA/liveness or
complete exit-snapshot proof.
Reachability does not remove valid PC re-entry points. Exhaustive classifications
separate direct/guarded/helper paths from interpreter transitions; generated
successors and helper IDs must match that analysis. Whole-op user-code effects
are opaque heap/upvalue mutation barriers, distinct from admitted native effects.
Graph records and the bounded traversal worklist share the snapshot quota and
are released before installation completes. `make jit-ir` covers flow/effect and
allocation rollback tests, also selected by the Rust-only Miri lane. This is not
full typed/effect SSA or a crafted-binary verification/security claim.

Each charged node also owns an exit snapshot: current PC, canonical fixed-register
prefix and admitted interpreter/guard/budget/helper-panic outcomes. Compiler
guards and retry exits are refused after scalar stores in the current instruction;
helper panic keeps the current instruction's frame/error PC at PC plus one and
uses the existing rooted payload/materialization path. This conservative compiler
write marker is not path-sensitive SSA. Wire reasons and ABI v3 are unchanged,
and these checks add no native dispatch work. `make jit-exits` tests snapshot
contracts and rejects four injected retry-after-store IR emission cases without
executing native code; the selected Miri lane includes those Rust-only tests.

Owned nodes carry fixed-register read/may-write bitmaps, known scalar output tags
and exact helper kind/operand records. Compiler loads/stores must use declared
registers, known scalar stores must match result tags, and reference tags cannot
be emitted through raw scalar stores. Helper operands distinguish constants and
upvalue slots from frame registers. Unsupported instructions remain interpreted;
their conservative full-frame masks do not grant native lowering. Conditional
loop outputs are may-writes, not definite definitions. `make jit-access` tests
the descriptors and six pure-IR rejection cases, also selected by Miri. These
records share the snapshot allocation quota; no runtime checks, new references
or ABI change are added. This is not optimizing typed SSA or a full value-bit,
path-sensitive, variable-frame/liveness proof.

Host service/preparation sweeps compact sparse registration, tracking, code
index and queue storage. Nonempty containers qualify at capacity 64 or greater
and at most quarter occupancy. Replacement storage is reserved fallibly before
moving entries; old and new requested layouts remain charged together. Quota
or allocator refusal preserves contents, capacity, pending work, recency and
code leases, and defers eight eligible passes before retrying. Empty backing
storage is released without replacement allocation. Compaction runs outside
native execution and manual executor steps, but has no hard wall-time bound.

`JitStats::metadata_compaction_attempts`, `metadata_compactions`,
`metadata_compaction_refusals` and `metadata_compaction_bytes` are cumulative,
saturating container-maintenance counters. Deferred/dense passes are not
attempts; empty backing releases are. Reclaimed bytes are shared-ledger
requested-layout deltas, not RSS or compiler-working-memory measurements.
Fixed compactor/counter owner fields remain outside the container ledger.

JIT metadata maps use the existing randomized `ahash::RandomState` for private
generation IDs and prototype-address routing keys. Weak upgrade and object
identity validation remain required; a hash/address match alone never admits
code. Sparse compaction clones the populated map's hasher state. This changes
neither the native ABI nor Lua table semantics or dependency requirements.

`make jit-upvalues` compares native and interpreted open/closed cells with
collection between slices, exact logical read/write counts, eight/nine distinct
captures, joined aliases, foreign coroutine stacks, reference values, scalar
type changes, error guards, debug rebinding and Rust callback reentry. These
are behavior checks for the current helper-backed tier, not performance
acceptance or a direct scalar-upvalue cache.

`make jit-debug` compares prepared native execution against Off for exact
line/count/combined hook events, hook replacement, caught hook errors and
normal/tail-call tracebacks. It checks identical slice modes, fuel and interrupt
boundaries at five budgets, collecting after every slice. Native table-write
checkpoints prove execution before and after hooks while hooked main-body
writes remain interpreted. The target runs with and without optional async
support; these are correctness checks, not performance measurements.

`make stdlib-debug` checks baseline, Off, Auto and prepared execution of debug
indices and mutation. Out-of-range local/upvalue indices retain their existing
nil/error results across the full Lua integer range; large frame levels do not
wrap on 32-bit hosts. The native debug gate also proves that rejected mutations
leave captured values intact and native execution resumes afterward.

`make jit-gc-requests` checks default, nil and explicit collection requests in
baseline, Off, Auto and prepared execution, including async-enabled builds.
The native fixture disables automatic collection, drains pending finalizers at
host boundaries and compares exact slice/fuel/interrupt/event traces. Ordinary
weak values and finalizable values are separate: finalizer resurrection may
retain the latter for another collection cycle. Native write counts prove work
before and after requests; no extra host collection hides a missing interrupt.

Lua `stop`/`restart` and the Rust pacing methods share one scheduling flag;
`isrunning` reports that flag, not allocation debt. Explicit collect/step requests
do not restart stopped automatic collection. Public `Context::request_gc`
retains its deferred, last-request-wins behavior; Lua pacing verbs update the
flag directly. The GC request gate checks that distinction and exact pacing
observations across native/interpreter boundaries.

`make jit-helpers` directly tests all nine scoped Rust helper entries with
closed upvalues and canonical reference identity. It checks effect-free decline
and PC rollback, panic materialization of exactly the declared scratch prefix,
unchanged trailing scratch and transport of the same panic payload back to
Rust. These fixtures run in the Miri lane too. They do not invoke generated
code or replace native open/foreign-upvalue and GC-slice tests.

Use `make jit-boundary`, `make jit-native`, `make jit-heap`, `make jit-policy`, `make jit-registers`, and `make jit-verify` in the Nix development environment. Test-only Force wrappers prepare after host arena entries; sources loaded and executed wholly within one entry cannot be prepared between those operations and are not falsely counted as forced-native coverage. Dedicated native tests explicitly prepare and assert nonzero native instruction counts.

Force preparation now fails tests on unexpected errors, including resource or
executable-memory refusals. A fixture intentionally testing a quota refusal must
call the test-only `allow_jit_resource_refusal` with its exact reason; a different
refusal still fails. `preparation_report` exposes calls, actual installations
(including work installed before a later refusal), empty successful batches,
allowed/unexpected resource refusals and unsupported-target skips. Explicit
exclusions emit `JIT_FORCE_EXCLUDED` diagnostics, visible with `--nocapture`.
Unsupported targets remain interpreter-only, not successful native preparations.
`make jit-test-modes` checks the wrapper in separate processes so mode selection
cannot race with other tests' environment. Empty batches and same-entry execution
are tested as zero native work, not acceleration evidence.
The existing `tests/scripts/math.lua` and `tests/strings.lua` corpora exceed the
default IR-block admission cap. Their fixtures explicitly permit only `IR blocks`
and assert that this refusal occurred in supported-target Force mode; other
scripts have no blanket exemption. Both original scripts still run and check
their Lua semantics.
No quota was raised and this pre-existing fallback is not claimed as whole-script
native coverage. A source executed within one arena entry may be prepared only
after that execution finishes; installation totals alone do not prove native use.

`make jit-resources` checks failed and successful growth, retained capacity, lower-limit ownership, injected underlying/partial-snapshot failure, queue refusal without stranded flags, generated-mapping reclamation after installation refusal, separate registration/snapshot budgets, retroactive registration retirement and final-source collection. Requested container layouts are reserved before `Global` allocation and released only after deallocation; default allocator growth keeps the old block charged while allocating/copying its replacement. The allocator is static/GC-free; the weak registry still uses the collector's traced hashbrown implementation and fresh generation/identity verification.

`make jit-boundary` additionally checks entry-metadata refusal before compiler setup, mapping-record refusal before allocation, preservation of already allocated segments, and code/metadata pinning across clear/Off/code-quota/metadata-quota reductions. Allocation records are reserved before segment allocation; record quota is distinguished from page quota by a typed provider flag, not error-string matching. Allocation/protection failures never publish a function pointer, and the failed module's mappings and metadata are reclaimed without touching another live module.

`make jit-verify` also runs `make jit-fuzz-smoke` on supported Linux hosts. This initial deterministic campaign creates 24 bounded scalar CFGs for each of four seeds and tests every entry (including unknown PCs), seven budgets, integer/float/mixed register inputs, guard exits and code reclamation against the Rust boundary model. Admission mutations check malformed operands, branches, register capacity and noncanonical descriptors; the backend revalidates before compiler setup or native allocation. Workers verify their inherited resource limits. Every worker has a separate process, 30-second CPU and 60-second wall deadline, 2 GiB address-space limit, disabled core dumps and 16 MiB output limit. Nonzero exit, signal, timeout or mismatch fails the gate; injected failure tests verify supervisor behavior. Seed-specific logs, campaign settings and replay commands live under `target/jit-evidence/fuzz/`.

Run a larger bounded campaign with `make jit-fuzz FUZZ_TARGET=all FUZZ_CASES=1024 FUZZ_SEEDS=0,1,42,0xdeadbeef`; `FUZZ_TARGET` may also be `admission`, `scalar` or `heap`. `all` retains its admission/scalar meaning for existing replay commands. Cases are bounded to 1..10000 per seed and 1..64 seeds. Fixed per-worker resource ceilings remain in force, so an overlarge campaign can legitimately fail on its deadline. This seeded harness is not coverage-guided fuzzing, does not execute mutated IR in Luna's reference VM, and does not establish the full heap/callback/lifecycle mutation corpus or Miri/platform acceptance.

`make jit-fuzz-heap` defaults to 24 expensive heap/lifecycle cases per seed;
explicit `FUZZ_CASES` overrides are respected. `jit-fuzz-smoke` includes eight
cases per seed. Six parameterized source
families exercise nested/cyclic aliases, open/closed upvalues, metamethod fallback,
weak-mode reattachment, catchable nil-key errors with close handlers, bounded
reentrant callbacks and finalizer resurrection. Fresh Off/Auto states
compare each slice's fuel/mode, ordered Rust callback effects, selected global
state, yielded values and final results. Every slice includes full host GC and
replacement of a rooted table/userdata field consumed by Lua. Current and
last-consumed object kind/ID-marker/scalar fields are observed before/after GC;
retained aliases are mutated before replacement. The manual-step host drains
queued finalizers and refuses unexpected warnings. One early host boundary
retires/reprepares code.
Each case requires native table/allocation/upvalue counters, suspension and
mapping/snapshot/queue cleanup. Heap counters are reported separately from scalar
kernel invocations. Host-read counts and userdata observations are separate from
native operation counts; repeated userdata samples do not identify individual
native userdata instructions. `make jit-heap-campaign-tests` exercises all six
families for three fixed seeds. Fuel/mode comparisons apply to main-executor
host slices; nested/finalizer internal slices are not individually paired.
Neither this finite corpus nor selected state observations
prove arbitrary heap equivalence, full collector safety or compiler accounting.

`make jit-bench` checks results and native counters. The first measured scalar tier improved the float loop substantially but regressed heap/callback-heavy workloads; **performance acceptance has not passed**. Repeated shipping and matched size/disabled-cost artifacts are recorded in `PLAN_JIT.md`; those runs include failed 5% compiled-Off controls at both profiles. Three dispatch restructuring experiments worsened the controls and were removed. Coverage, cold compilation cost, profitable mixed workloads and broader platform evidence still need completion. The plan's frozen performance thresholds are not waived.

`make jit-bench-paired` alternates Off/Auto order for same-process sample pairs with two untimed warmups and 11 measured pairs. It reports each mode's median/range and paired speedup dispersion, verifying results and native coverage. `make jit-performance` additionally exits nonzero for missed frozen loop/table/upvalue/mixed/cold thresholds after printing all cases. Oslo is observational, not silently assigned a convenient numerical threshold. This lane measures opt-level 3, not the shipping size profile or the separate compiled-but-disabled feature cost. Run it without concurrent builds/tests and repeat before treating timing as acceptance evidence.

`make jit-shipping` runs the checked paired corpus at the existing opt-level-s shipping profile, without applying the separate opt-level-3 speedup gate. Benchmark output embeds the wrapper's optimization label; direct Cargo builds without that environment label report `unspecified`.

`make jit-size SIZE_PROFILE=speed` measures opt-level-3 matched embeddings; `SIZE_PROFILE=shipping` is the default and selects opt-level s. Both binaries use identical probe source and the benchmark's shared Lua corpus, one without JIT and one with runtime Off/Auto selection. The latter retains usable compiler code in the linked artifact. Builds use the same locked dependencies, target, LTO, single codegen unit and stripping; file sizes, ELF sections, hashes, flags, dependency trees and CPU/toolchain metadata are recorded under `target/jit-evidence/feature-cost/`. These sizes include the measurement harness and are not universal downstream binary-size claims.

The size gate first runs the JIT artifact in Auto and asserts native work/results on warm cases, leaving cold Auto interpreted. It then compares feature-disabled versus compiled-but-Off runtime cost. Eleven pairs alternate process order; each process verifies every result and Off's zero compilation/native counters. Twenty timed repetitions follow two untimed warmups for warm cases. Warm source loading and the seven ordinary cases' executor creation are outside timing; Oslo includes per-row global mutation/executor startup, and cold scripts include state construction/loading/execution. Process launch is excluded. Reports include raw pairs, median/range and paired dispersion. The frozen 5% overhead ceiling is checked per case, and every missed control is printed before a nonzero exit. Do not run timing concurrently with builds/tests; repeat before acceptance. Separate-process layout/ASLR/hash and host scheduling variation remain caveats. `make jit-cost-tests` validates protocol rejection, corpus ordering and the frozen limit; `jit-size-build` produces artifacts without making runtime/performance claims.

`make jit-platform TARGET=x86_64-unknown-linux-musl` runs the full baseline/JIT gate and prepared example on matching Linux hardware. The gate rejects undeclared targets and architecture mismatches before compiling. The same command accepts `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`; the toolchain must contain the requested standard library and any required linker. Ordinary `jit-verify` and focused test/doc targets also forward `TARGET`, including recursive fuzz workers. Fuzz replay artifacts record the target so musl evidence is not silently replayed as GNU.

The active workflow is `.github/workflows/tests.yml`. Its baseline `verify` job
runs formatting, baseline check/test, Miri-wrapper tests, advisory Clippy and
rustdoc; it is not the full local `make verify` recipe and does not prove native
execution. Three native jobs run `make jit-platform TARGET=...` on matching GNU,
musl and ARM64 hardware, then build matched shipping artifacts with
`make jit-size-build SIZE_PROFILE=shipping`. These builds do not establish size
or runtime-overhead acceptance. Separate jobs run two pinned Rust-only Miri seeds
and real i686 fallback. Action commits/toolchains are pinned and native/Miri/
fallback evidence uploads run on failure as well as success. `make ci-check`
validates wiring with actionlint; executed results remain revision-scoped as
linked above, not implied by workflow lint or an older green run.

`nix develop .#fallback -c make jit-fallback TARGET=i686-unknown-linux-musl`
builds and executes static 32-bit binaries on a Linux x86-64 host with 32-bit
execution support. The separate fallback shell supplies the target library and
LLD; it does not change the normal development shell. The matching hosted job
uses Rust 1.97.1 and LLD. This is an unsupported-native-target interpreter lane,
not a newly supported native platform. It runs the baseline and JIT-feature
workspace suites, optional features and dedicated Off/Auto differential tests
with callbacks, coroutines, GC and exact fuel traces. Explicit Auto preparation
and service report `Unavailable`; convenience execution remains interpreted,
with no queued requests, compiled regions or native instructions. A failure to
execute the target fails the gate rather than silently skipping it.

The unchecked acceptance criteria in `PLAN_JIT.md` track remaining work. Historical
checkpoints and individual platform passes do not establish full-plan completion.

`make jit-mock` runs a test-only Rust slice model integrated with interpreter
dispatch. Before/one-scalar-after exits preserve exact executor traces, errors,
side-effect order and GC-visible state across selected callback/vararg/coroutine/
close cases. Unsupported operations execute canonically after a mock decline.
The mock reports no native work and also runs on i686 without a native backend;
it is not a substitute for generated-code tests and is absent from library builds.

`make jit-profile` retains Rust symbols and attempts an opt-level-3 perf capture of the checked upvalue workload; it requires host perf permission and does not alter kernel settings. `make jit-rust-assembly` extracts symbol-retained Rust dispatch/helper/invocation assembly without perf permission. These are diagnostic lanes, not acceptance timing builds or native JIT disassembly. On the current host perf recording is denied (`perf_event_paranoid=4`); Rust assembly verified that outlining removed the native scratch probe page from `run_vm`.

`make jit-cost-profile PROFILE_CASE=float_loop` uses Callgrind from the Nix shell on symbol-retained matched no-JIT/compiled-Off probes at opt-level 3. `PROFILE_MODE=auto` instead profiles the JIT artifact in Auto against the feature-disabled interpreter; the worker must assert real native work. Collection toggles only within `*run_vm*`; the command fails on a nonzero child exit or empty profile. Raw instruction-level events, exclusive annotations, hashes, settings and checked worker output live under `target/jit-evidence/feature-cost/speed-symbols/callgrind/<case>/<mode>/` (with a target-qualified directory when requested). Warm corpus cases are selectable; normal checked cost comparisons reject `--case` and continue to require all nine controls. These are simulated instruction/cache/branch events, not hardware cycles, wall-time acceptance or native-machine-code validation.

`make jit-disassembly` dumps finalized, relocated native code for scalar-loop
and table/helper fixtures through an explicitly invoked ignored test. The
scalar kernel executes to the checked 5050 return value; the table kernel exits
before allocation when supplied a null helper host. GNU objdump from the Nix
shell disassembles the live module bytes at their actual entry addresses.
Source, decoded PC/entry flags, constants, ABI/target and helper addresses,
execution counts, environment, tool version and hashes accompany the binaries
and assembly under `target/jit-evidence/native/<target-or-host>/`. Mappings are
reclaimed after dumping; addresses are valid only for that diagnostic process.
The target rejects missing test execution or empty/wrong-architecture artifacts.
`make jit-platform` includes it, so the native CI matrix retains these artifacts.
This is not full symbolization, platform certification or an unsafe-code proof;
raw disassembly may decode embedded constants as instructions. Production
builds gain no dump API, file writes or retained diagnostic state.

`make jit-bench-build` and `make jit-bench-run` separate compilation from timing.
The combined `make jit-bench`, performance and shipping targets still build then
run. Run-only accepts an immutable copied artifact through `JIT_BENCH_BINARY`;
its embedded optimization label remains authoritative. `--check` rejects
shipping or unspecified labels rather than applying the opt-level-3 acceptance
gate to the wrong build. Run-only does not guarantee an idle machine: do not run
acceptance timings alongside builds, tests, fuzzing or profiling.
