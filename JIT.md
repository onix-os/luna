# Native JIT: experimental scalar and heap tier

This branch implements the first native execution tier described in [PLAN_JIT.md](PLAN_JIT.md). It is **not the finished plan** and is not the LuaJIT runtime or its FFI. Constructors still default to interpreted execution. Do not use these results to claim production readiness or hostile-code isolation.

## Enable and prepare

Enable the optional `jit` Cargo feature and set `JitConfig.mode` to `JitMode::Auto`. See `examples/jit.rs`; run it with `nix develop -c make jit-example`.

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
retired. Metadata/snapshot/capability/compiler errors do not trigger eviction.
A service call still handles at most one prototype, but can make two backend
compilation attempts. This is bounded retry, not compiler preemption.
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

Calls, returns, actual metamethod/user callback invocation, close tracking/unwinding, coroutines, and async transitions still run through explicit interpreter exits. Native execution does not recursively call Lua on the native stack or keep a generated frame across suspension. Hook-enabled slices remain interpreted. Heap coverage and lifecycle acceptance remain incomplete until the full plan's stress/performance/platform gates are satisfied.

Every generated operation checks the remaining reference slice allowance before executing; a native invocation completes at most 64 logical bytecode instructions. A guard failure leaves the PC before the unperformed instruction. Completed scalar writes are materialized into the same canonical Lua registers, and the interpreter immediately makes progress without repeating completed work. Fuel retains the interpreter's existing approximate transition charges, including minimal progress with exhausted or interrupted input.

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
setup and can refuse with `ResourceLimit("entry path verification")`. CFG and
compiler jump-table storage remain outside the incomplete ledger. These checks
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
Cranelift CFG/dominator allocations still needed for dominance, backend internals
and fixed owner allocations remain outside the ledger. This is not complete
compiler working-memory or RSS accounting.
The retained, fallible PC-to-block map is charged to the snapshot ledger and
can refuse with `ResourceLimit("frontend block map")` before compiler/host setup.
Semantic checks are
vacuous only on guarded paths proven unreachable without queried-tag assumptions;
physical store coverage and record counts still apply. This does not prove full
payload selection, operation equivalence, liveness, aliasing or complete typed SSA.
Luna-owned store/input records and worklists use the snapshot ledger and can report
`ResourceLimit("scalar tag verification")`; compiler CFG/dominator storage remains
outside the current ledger. Record-envelope violations are compiler errors and
never silently grow verification storage.

Completed full and incremental GC cycles retire dead source registrations and
their cached code even while JIT is Off. Live closures keep their registrations;
partial cycles can defer retirement until completion. GC performs no compilation.
The disabled `service_jit()` fast path remains unchanged.

- `max_prototype_instructions` and `max_snapshot_bytes` bound snapshot admission before copying bytecode. Native scratch uses the smallest fitting 8/16/32/64/128/256-slot tier, bounded to 256 scalar slots. Scratch-bearing routines are non-inlined, so compiled-but-Off VM entries do not reserve their 4 KB maximum.
- `max_ir_instructions` (1048576) and `max_ir_blocks` (65536) independently cap a conservative frontend expansion envelope before graph allocation or Cranelift setup. Actual frontend instruction/block counts must fit the admitted envelope before code generation. The estimate can reject a prototype below the source-opcode ceiling; these limits do not bound optimizer CPU time, compiler working memory or RSS. Lowering either limit retires cached code and pending requests; existing leases retain their normal lifetime policy.
- `max_queue_entries` bounds pending identity requests. Saturating hotness and `max_compile_attempts` limit repeated failures; clearing the cache resets attempts.
- `max_code_bytes` bounds actual page-rounded JIT mappings, including the compiler's code/readonly/writable segments. Retired but pinned mappings remain charged until the last lease drops. Allocation failure frees partial mappings and leaves interpretation usable.
- `max_metadata_bytes` independently bounds requested allocation layouts for weak registrations, tracking/code-index maps, pending identities, preparation ID buffers, native-entry flags and mapping records. Containers use a shared fallible allocator; retained capacity and temporary old/new growth are charged. Registration refusal leaves ordinary source loading/interpreting usable; metadata allocation refusal is typed `ResourceLimit("JIT metadata")`. Lowering this ceiling retires registrations, code and requests; existing closures continue interpreted, and new source loads can register after limits are raised. Active leases keep their entry flags/mapping records and remain charged until their final owner drops, even above a newly reduced quota. Ordinary cache clearing retains live source registrations.
- Owned operation/constant snapshot vectors have a separate allocator and `max_snapshot_bytes` ceiling; their charges follow the vectors' actual lifetime and release on success, refusal or panic. `metadata_bytes`/`snapshot_bytes` report current requested container storage, and their peak fields retain the high-water reserved usage. `metadata_allocation_refusals` counts quota/underlying allocation refusals; `registration_refusals` counts optional source registrations declined on allocation pressure. Empty retired containers release capacity.
- This is **not yet a complete compiler or combined-host ledger**. It excludes fixed owner/Arc/Rc headers, allocator overhead, Cranelift's internal/transient/retained allocations (including its provider's internal records) and process RSS. Luna's persistent entry flags and outer mapping-record storage use the metadata ledger; pinned mappings remain charged after retirement. Bounded unleased LRU eviction and fallible sparse-container compaction exist, but complete compiler accounting and combined-limit policy remain required before Phase 3/7 acceptance. Container limits are not an RSS or hostile-compiler ceiling.
- Existing `Lua::total_memory()` and memory limits still describe the collector, not compiler/JIT memory. Configure native limits separately; do not treat GC metrics as a process memory ceiling.
- `native_entries` counts real machine-code invocations, including immediate guard exits. `native_instructions` counts completed logical bytecodes, not CPU instructions. `interpreted_instructions` counts the reference VM's reported instructions, which exclude some transition opcodes. `interpreted_slices` counts completed slices that fetched an interpreted opcode. `hook_exits` counts slices kept interpreted with hooks enabled.
- `code_lookups` counts eligible slice-local cache probes; `code_leases` counts successful owned leases, even when an entry is interpreted. A single VM slice reuses its lease across native/reference fragments, but repacks canonical registers for every invocation. Hotness observation remains per interpreted dispatch when no code is installed.
- `helper_calls` counts scoped helper attempts, `helper_instructions` counts completed helper-backed bytecodes, and `helper_declines` counts effect-free fallback requests. Table/upvalue/allocation counters count successful accesses; table-through-upvalue operations include a successful upvalue read. These are real native helper paths, not the interpreter's opcode loop.
- `code_bytes` is live mapped usage; `snapshot_bytes` follows owned snapshot vectors; `installed_regions`, requests, failures, and execution counters are cumulative. Preparation is synchronous, so the host usually observes zero current snapshot usage after it returns. Benchmarks also report current/peak container charges and refusals.

The backend is compiled for Linux x86-64/aarch64. Executed integration evidence exists on x86-64 GNU and musl; ARM64 execution remains unverified. `supported_target` reports build eligibility, **not successful executable-memory allocation or release platform certification**. Explicit preparation/service reports unsupported targets, typed `ResourceLimit` mapping/metadata/snapshot refusals, native allocation/protection denial as `Unavailable`, or compiler errors. Optional convenience service failures do not become Lua language errors. Test-only injection covers allocation/protection denial after source loading, preservation of another installed module, interpreter fallback and recovery; it does not modify host permissions or expose a script-facing fault option.

## Unsafe boundary review: current scalar/heap tier

1. The v3 generated signature is `extern "C" fn(*mut Slot, u64, u32, *mut Exit, *mut Host)` with the platform's native C calling convention. `Slot` and `Exit` use `repr(C)` and asserted size/offsets. Nine fixed imported helpers use `extern "C" fn(*mut Host, *mut Slot, u32, u32, u32, u32) -> u32`; each specializes a const helper kind. The opaque host carries only the scoped frame pointer, not an indirect callback. Compiler lookup matches explicit unique kind keys, independent of registry order. No Rust enum, `Gc`, frame layout, or arena lifetime is assumed by generated code. There is no persisted native cache or public helper ABI to migrate.
2. Snapshots contain decoded owned instructions and scalar tag/bits constants only. Reference constant placeholders resolve by validated index through the scoped helper's active closure, not cached GC pointers. Register, constant, upvalue, prototype, skip, and jump operands are validated before code generation. Code checks the budget at every instruction entry, including backedges.
3. Each eligible VM frame slice acquires one `Rc<Code>` lease with its `JITModule` alive, releasing the manager borrow before execution. It reuses the lease across native/reference fragments and drops it on slice completion, frame transition, error or unwind; no scratch/host state survives an invocation. Scratch slots, exit buffer, opaque host, and borrowed Rust helper frame outlive the synchronous call. Entry/bounds checks select an outlined scratch tier large enough for the admitted prefix; every used `MaybeUninit` slot is written before a typed slice is formed, and the unused suffix is never read as `Slot`. Reference-result tests cover both sides of every tier boundary including 256. The lifetime-erasing pointer cast is confined to the scoped gateway; no pointer, GC reference, or helper result escapes into cached code. A null host in boundary-model tests declines helper work.
4. Helpers decode scalar operands from scratch and reference operands from canonical traced slots. Each destination synchronizes both representations; table/upvalue setters retain normal barrier APIs. Pending scalars materialize on every exit and before a caught panic resumes. No helper invokes collection, users, hooks, or frame changes. Existing upvalue access reads a closed cell, another stack, or registers above the current frame; its same-stack assertion excludes current-frame aliasing. Old canonical references can remain rooted until exit, but no collection can observe that interval. Any future callback/safepoint/current-frame-inspection helper must restore full synchronization first. No Rust heap layout is assumed by machine code.
5. Each memory-provider allocation owns an independent system provider. Quota is reserved before allocation and undone on failure; finalization delegates RW-to-RX / readonly transitions and icache handling to the pinned compiler provider. No deliberate RWX mapping is requested. Failed compilation and code retirement explicitly free mappings; module drop alone is not relied on for reclamation.
6. Helper gateway signatures use opaque pointers and fixed-width immediate operands. The gateway catches Rust unwind payloads and returns an explicit panic exit; after native return, Rust resumes the same payload rather than converting it to Lua success or interpreter fallback. No helper invokes user callbacks, Lua code, frame transitions, the compiler, or collection. Rust `panic=abort` still aborts normally. A deliberately conflicting table borrow verifies the unwind profile returns through the generated frame and leaves the state reusable.
7. Helpers advance the canonical PC before effects, restore it when declining before effects, and return at most one completed logical operation per call. Guard failures do not perform a table mutation, user callback, or Lua error before interpreter retry. Fresh metatable contents and interception/readonly flags are consulted on each access; no invalidation cache or table layout assumptions exist yet.
8. Tests hold a code lease across cache retirement and verify memory returns to zero after the last lease drops. Scalar boundary tests compare every entry and budget against an independent Rust model. Heap tests compare per-slice fuel/state with full GC after every slice, prove heap counters, exercise weak tables, readonly/invalid-key typed errors, open/closed upvalues, Rust interception mutation, and reentrant callbacks. Extend this review for every new helper, cached assumption, or frame transition.

## Verification and remaining work

The expanded supervised heap campaign and case-limit fix (`3762300`)
passes `nix develop -c make jit-verify clippy jit-clippy` on x86-64 GNU and
with `TARGET=x86_64-unknown-linux-musl`: each reports 4400 passing tests across
422 suite results and 24 ignored tests. Selected pure-Rust/pure-IR Miri checks
pass 211 tests across 25 selected suite results with default `MIRIFLAGS`; they do not execute
generated native code. Clippy retains the existing warning backlog, so these are
not strict-warning acceptance. Raw revision-scoped evidence is stored locally in
`target/jit-evidence/heap-expanded/`. Seeded scalar/admission campaigns additionally
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
metadata/snapshot ledger peaks. Compiler allocations, fixed owners, allocator overhead and RSS remain
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
observations do not complete the full mixed-tier transition/error matrix.

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

`make jit-helpers` directly tests all nine scoped Rust helper entries with
closed upvalues and canonical reference identity. It checks effect-free decline
and PC rollback, panic materialization of exactly the declared scratch prefix,
unchanged trailing scratch and transport of the same panic payload back to
Rust. These fixtures run in the Miri lane too. They do not invoke generated
code or replace native open/foreign-upvalue and GC-slice tests.

Use `make jit-boundary`, `make jit-native`, `make jit-heap`, `make jit-policy`, `make jit-registers`, and `make jit-verify` in the Nix development environment. Test-only Force wrappers prepare after host arena entries; sources loaded and executed wholly within one entry cannot be prepared between those operations and are not falsely counted as forced-native coverage. Dedicated native tests explicitly prepare and assert nonzero native instruction counts.

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

The active workflow is `.github/workflows/tests.yml`, promoted from the former non-active `workflows/tests.yml` template. It retains baseline checks and adds full native GNU/musl x86-64 and GNU ARM64 jobs on matching hosted runners, with pinned action commits, Rust 1.97.1, target-specific caches and uploaded logs/campaign artifacts on failure as well as success. `make ci-check` validates the workflow with actionlint supplied by the Nix shell. Local workflow validation is not an executed hosted run; ARM64 and hosted-CI results remain required before release acceptance.

Remaining stages include complete boundary/IR/cache accounting, broader helper/heap optimization and explicit mixed-tier lifecycle stress, refined promotion policy, expanded heap/lifecycle and coverage-guided fuzzing, executed ARM64 verification, and actual hosted CI results.

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
