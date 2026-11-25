# Changelog

## [0.6.0] - 2026-11-25

### <!-- 0 -->⛰️  Features

- Add pure Starlark-style states
- Hand off compiler job completion
- Reserve worker address space
- Add fallible string publication
- Retain owned compiler jobs
- Account shared compiler images
- Add identity-safe worker launch
- Embed source-built compiler images
- Seal verified compiler images
- Bootstrap standalone compiler worker
- Integrate readonly lookup runtime
- Integrate packed runtime dispatch
- Lower packed scalar reads
- Enable verified array loop kernels
- Model atomic calls in rooted slots
- Lower fixed table lists natively
- Enable scoped caller regions
- Split region compilation requests
- Route native caller chains in Auto
- Queue source-bound call programs
- Add scoped activation hosts
- Admit frame-free scalar entries
- Lend stable scalar projection buffers
- Report every native exit reason
- Detach finalized compiler images
- Budget cached code owner allocations
- Verify source regions and retry effects
- Verify scalar transfer source semantics
- Verify arithmetic source operands
- Validate scalar tag SSA stores
- Validate canonical exit snapshots
- Observe coroutine and async slices
- Measure cold and scheduling costs
- Add optional native execution tier

### <!-- 1 -->🐛 Bug Fixes

- Honour freeze in insert and remove
- Retain charged compiler diagnostics
- Restore default make target
- Isolate compaction byte accounting
- Bound compiler error publication
- Support special cost cases
- Avoid borrowing an active Oslo VM
- Lock shared reproducible build caches
- Enforce frozen elapsed thresholds
- Accept elided zero event totals
- Reject overflowing relative indices
- Resume instructions after debug hooks
- Withdraw regressing window constructor
- Bind optimized dispatch prefix reads
- Bound callback dispatch iteration
- Avoid duplicate PGO stress training
- Share PGO build paths
- Withdraw atomic counter publication
- Withdraw regressing constant fast path
- Pin matched profiles to one cpu
- Preserve profiled invocation paths
- Retain native calls across fallback
- Recover failed resident returns
- Release chain leases on native unwind
- Classify pair calls before queuing
- Handle unavailable JIT preparation
- Verify benchmark artifact hashes
- Collect complete executor profiles
- Return host ISA detection refusals
- Account for ABI signature storage
- Account for module relocation copies
- Validate full-range Lua indices
- Preserve Lua integers on 32-bit
- Make Miri evidence uploads portable
- Avoid allocations in unmapping proof
- Forward heap campaign case limits
- Retire ephemerons on kv reattachment

### <!-- 2 -->🚜 Refactor

- Extract owned compiler service
- Separate native memory faults
- Restore admission inlining
- Bound preferred entry checks
- Combine caller admission views
- Borrow activation frames

### <!-- 3 -->📚 Documentation

- Retain failed hash reuse results
- Checkpoint hash reuse validation
- Record completed work profiles
- Record 809 passing benchmark gates
- Record interrupted stress recovery
- Record four full Miri matrices
- Record both default Miri matrices
- Record complete VM matrix proof
- Record shared-control verification
- Record comparison validation
- Record comparison work reduction
- Record predicate cycle findings
- Record combined timing failures
- Record cold-state cost trial
- Retain failed feedback experiments
- Record current acceptance evidence
- Record decoder qualification
- Record completed platform gates
- Record packaged consumer proof
- Record resume and size qualification
- Record typed lookup timing rejection
- Record verified native platforms
- Record full acceptance matrix results
- Record lookup benchmark tradeoffs
- Record frontend bandwidth loss
- Record counter Force validation
- Record outlined fusion tradeoffs
- Record equality recovery and gaps
- Record paired scan measurements
- Record canonical timing regressions

### <!-- 4 -->⚡ Performance

- Adopt lazy snapshot ledger runtime
- Dispatch integer resumes directly
- Specialize constant global reads
- Resume integer callbacks in place
- Optimize table reads and windows
- Read only the snapshot limit
- Fuse scalar lookup input admission
- Specialize scalar callback entries
- Bound array mirrors by fuel
- Compact scalar array validation
- Reuse loaded callback hint
- Scope native callback lookups
- Bound fused scratch initialization
- Release fused source snapshots
- Fuse fresh lookup capture and entry
- Classify roots without scalar encoding
- Group zero write-counter publication
- Pack canonical reads without hashes
- Withdraw known-frame entry trials
- Withdraw counter storage trials
- Inline cached string comparison
- Share binary metamethod resolution
- Reuse report storage for progress
- Inline native slot conversion
- Trial optional bound-entry runtime pair
- Trial thread-local prepared bucket hint
- Restore shared array-aware entry
- Inline cached source hash lookup
- Restore identity inlining
- Restore prepared key matching
- Validate known inline source roots
- Outline non-integer slot conversion
- Isolate hook checks from plain dispatch
- Outline the hook resume predicate
- Keep hook resume dispatch immutable
- Restore measured register view layout
- Outline prepared string fallback
- Retain preflight callee binding
- Fuse byte tag loads and extension
- Prove tags across effect-free cycles
- Classify disjoint scratch fields
- Track strided helper writes
- Streamline strided mask traversal
- Integrate charged inline loop batches
- Split prepared and generic entries
- Construct array windows in scope
- Preserve array kernel transport
- Avoid full set-list rebinding
- Isolate integer payload binding
- Withdraw inline control trial
- Bypass reference payload rebinding
- Simplify experimental tag decoding
- Add packed C helper bridge
- Gate handoffs before frame probes
- Refresh upvalue destinations only
- Preserve bindings across table writes
- Refresh local table destinations only
- Load proven bound numeric fields
- Bind proven scalar write entries
- Share payload gateway table
- Withdraw regressing combined guards
- Combine numeric loop tag guards
- Share inline key hash construction
- Specialize constant upvalue reads
- Streamline native callback setup
- Admit fuel before bounded subtraction
- Keep nil payload binding inline
- Share payload helper table
- Specialize scalar materialization
- Narrow closed table helper borrows
- Localize payload materialization
- Withdraw shared callback setup trials
- Withdraw deferred region release
- Withdraw owner-release trial
- Withdraw bucket cache trials
- Cache checked string bucket offsets
- Specialize suffix consumption
- Withdraw slower invoker table
- Withdraw regressing projection trials
- Outline projection bridge setup
- Use register-return exit transport
- Cache compact code bucket offsets
- Use one unwind-safe progress counter
- Short-circuit identical string keys
- Read rooted integer inputs directly
- Stage small rooted frame publication
- Reuse table borrows in stable loops
- Separate array access from hashing
- Specialize floating loop bodies
- Branch numeric loop limit checks
- Inline JIT operand resolution
- Reuse fixed call stack capacity
- Scope compact call writeback
- Bypass admitted call callbacks
- Restore full generic import set
- Restore transition register views
- Lease sources across native regions
- Admit pair sites once per region
- Gate scopes on installed call pairs
- Retain scoped native caller slots
- Restore return transition checks
- Isolate test resume entry state
- Isolate bound pair invocation
- Isolate scoped activation entry
- Reuse scoped pair leases
- Specialize nonprojected runtime exits
- Specialize small native frames
- Skip redundant plain-table key reads
- Hash metadata with seeded AHash
- Update LRU recency in one cache probe

### <!-- 6 -->🧪 Testing

- Verify raw compiler job memory
- Verify packaged release consumers
- Share compiler cleanup fault controls
- Gate owned worker workspace
- Isolate descriptor leak checks
- Preserve jobs through Lua cleanup
- Admit images against Lua budgets
- Require sealed startup conformance
- Bound compiler stream transport
- Preserve typed compiler outcomes
- Bind owned compiler requests
- Bound compiler target identity
- Install artifacts with bounded linker
- Emit worker code without linking
- Verify worker-backed runtime owners
- Transfer and install worker artifacts
- Exercise bounded compiler workers
- Verify owned compiler snapshots
- Cover callback resume publication
- Avoid formatting scalar snapshots
- Add fresh scalar entry bridge
- Cover callback source registration
- Cover Off continuation boundaries
- Verify native array reference roots
- Cover reference-valued environments
- Cover memory limit fast-path contracts
- Isolate Oslo row lifecycle profiles
- Bound callable width portably
- Cover numeric add operand aliases
- Preserve supplied prepared hashes
- Cover closure argument layouts
- Cover native call successor traces
- Add warmed cycle profiling
- Cover weak prepared-key collisions
- Verify wide helper compiler path
- Use host ISA in direction fixtures
- Require batched PGO coverage
- Measure prepared table key hashes
- Execute payloads through runtime
- Cover nested outcome ownership
- Cover executor mode handoffs
- Model bounded exit write sets
- Cover rejected call observations
- Bound Rust-only Miri validation
- Verify bounded native tail recursion

### <!-- 7 -->⚙️ Miscellaneous Tasks

- Bump version and internal deps to 0.6.0
- Require static GNU producers
- Verify standalone producer conformance

### Build

- Add compiler image bootstrap tool
- Add headless worker toolchain

## [0.5.1] - 2026-08-16

### <!-- 0 -->⛰️  Features

- Make searchers a real hook for require
- Add getregistry, uservalues and upvalue ids

### <!-- 1 -->🐛 Bug Fixes

- Report the language version and add _LUNA

### <!-- 6 -->🧪 Testing

- Cover the closed stdlib divergences

### <!-- 7 -->⚙️ Miscellaneous Tasks

- Point internal deps at 0.5.0

## [0.5.0] - 2026-08-16

### <!-- 0 -->⛰️  Features

- Give StashedError Debug and Display
- String.dump and a validating bytecode loader
- FromValue and IntoValue derive macros
- Weak keys with ephemeron marking
- Getlocal and setlocal over a register-name table
- Line and count hooks via sethook
- Option surface for both directions
- Await foreign futures behind the async feature
- Default io streams, package search, native table fast paths
- Serialize a Lua value into any serde format
- Hold values weakly for __mode=v tables
- Collect before enforcing the memory ceiling
- Separate the collector step from mutator entry
- Run __gc handlers via the finalizer registry
- Add UserRef for typed payload arguments
- Honour gsub __index, add table.clear and gmatch init
- Add Either, a prelude, and wider conversions
- Add the debug library with tracebacks
- Add source positions and error levels
- Add __metatable, __name, and PUC-Rio float printing
- Add popen, execute and an all-writes store hook
- Implement close attribute on every scope exit
- Add string.pack, unpack and packsize
- Implement the collectgarbage verbs as deferred requests
- Add the utf8 library
- Add frozen tables and a memory ceiling
- Add io file handles over std::fs
- Add package and require without a C loader
- Add the os library with UTC dates
- Convert maps and sets to and from tables
- Add xpcall, coroutine wrap/close, loadfile, rawequal
- Iterate the map part in insertion order
- Bound call depth with a catchable error
- Implement the `string` stdlib
- Implement PUC-Rio tonumber ([#75](https://github.com/onix-os/luna/issues/75))
- Make type match PUC-Lua
- Make nesting more apparent in compiler example.

### <!-- 1 -->🐛 Bug Fixes

- Report numbers in Lua terms, not Rust ones
- Reject a zero for-step and repair Stack::drain
- Reject values that would corrupt a binary record
- Keep weak-value entries out of the array part
- Make weak keys and weak values actually work
- Bound os.time fields and table.sort borders
- Wrap hex integer literals like PUC-Rio
- Match PUC-Rio gsub, patterns, format and pack
- Match Lua 5.4 numeric semantics
- Support generic types
- Make print honour __tostring
- Bound metamethod chains and concat length
- Rawlen accepts strings, not only tables
- A native caller contributes no source position
- Stop repeating the message in the cause chain
- Match PUC-Rio integer and float semantics
- Bound deserializer recursion instead of overflowing
- Load honours chunkname, mode and env
- Run natives with their thread released
- Gsub honours the ^ anchor
- Reinit
- Reinit
- Be correct so we don't burn from Zs ([#77](https://github.com/onix-os/luna/issues/77))
- Allow seed to be any size ([#66](https://github.com/onix-os/luna/issues/66))
- Math.random ([#48](https://github.com/onix-os/luna/issues/48))

### <!-- 2 -->🚜 Refactor

- Run natives on the real stack, not a copy
- Give the value stack its own lock

### <!-- 3 -->📚 Documentation

- Drop the plans folder
- Record the 0.4.0 release and the hardening pass
- Record the sort and os.time bounds
- Correct the status, size and debug claims
- Record the VM's resource limits
- Record string.dump in the parity map
- Record the ephemeron design and its measured cost
- Record the closed stdlib gaps
- Refresh the parity map and compatibility tables
- Measure and document binary size
- Record phase 3 completion
- Record the measurement that closes phase 3
- Document threading, weak keys and the debug library
- Record parity implementation status
- Refresh COMPATIBILITY against the running VM
- Add mlua parity inventory
- Remove upstream blog link
- Add simple function execution example. ([#105](https://github.com/onix-os/luna/issues/105))

### <!-- 4 -->⚡ Performance

- Keep the error path out of the dispatch loop
- Settle interned equality by pointer
- Follow table metamethod chains in a loop
- Register finalizable objects in O(1), and prune weak-key tables
- Shift call arguments instead of shifting the whole frame
- Share one hash seed instead of one per table
- Summarise Debug output instead of dumping graphs

### <!-- 6 -->🧪 Testing

- Cover chain, concat and print hardening
- A rust callback can be a __gc handler
- Reproduce the nested-executor upvalue panic

### <!-- 7 -->⚙️ Miscellaneous Tasks

- Rename LICENSE-MIT to LICENSE
- Update lockfile for serde_json dev-dependency
- Silence unused warnings in io
- Rename gc-arena references
- Version 0.4.0
- Give fork a unique name (ottavino)
- Format and clean up some warnings

### SequenceExt

- :map_result

### Table

- :next API does not need auto key conversion

### `OpCode

- :NumericForLoop` clarity

### Build

- Gate every feature in make verify
- Optimise release builds for size
- Drop clap, trim rand, tune release profile

## [0.4.0]

The first release under the name **luna**, and the release that closes the gap between "a Lua VM"
and "a Lua VM you can hand an untrusted script". Renamed from `ottavino`; the GC crate is
`ottavino-gc-arena` and keeps its name.

### Hardening

An audit of the whole tree found, and this release fixes, six ways a *pure Lua script* could kill or
hang its host. None were catchable by `pcall`, because a Rust panic is not a Lua error:

- `math.mininteger % -1` and `math.abs(math.mininteger)` aborted the process.
- `table.sort` on a table with a pathological border aborted on `capacity overflow`. A border is not
  a count — `t[1]=true; t[1<<62]=true` reports `#t` as 2^62 while holding two entries.
- A `__mode="k"` table panicked once it held more than three object keys, and again on array growth.
- `for i = 1, 10, 0` looped forever.
- A cyclic `__index`/`__newindex` chain looped forever.
- `s = s .. s` repeated 60 times exhausted memory rather than erroring.

And three ways a script could *silently corrupt data*, which are worse in that nothing reports them:

- `string.gsub` deleted characters whenever the pattern could match empty:
  `("hello"):gsub("x*", "-")` produced `------` instead of `-h-e-l-l-o-`.
- `string.pack("i1", 300)` wrote `"\44"`; `string.pack("s1", <300 bytes>)` wrote the full payload
  with a truncated length prefix, so `unpack` returned a prefix and the reader could not tell.
- `string.pack("z", {})` wrote a raw pointer — different on every run — into a binary record.

Two more that were not weak-table-specific: `pairs` looped forever on an ordinary table after a
string key was removed and re-added through an equal-but-distinct string (reachable from any
runtime-built string over 40 bytes), and `Stack::drain` left back-yielded values on the stack, so a
host callback draining in reverse returned its own arguments alongside its results.

Resource limits are now documented and bounded — see COMPATIBILITY.md's Limits section: call depth,
string length, metamethod chain length, `table.sort` array size and `os.time` field ranges.

### Added

- **Standard library**: `os`, `io` with real file handles, `package`/`require` (no C loader),
  `debug`, `utf8`, `string.pack`/`unpack`/`packsize`, `io.popen`, `os.execute`, `xpcall`,
  `coroutine.wrap`/`close`, `loadfile`, `rawequal`, `table.clear`, `gmatch` init.
- **Weak tables**: `__mode` `"v"`, `"k"` and `"kv"`. Weak keys use real ephemeron marking, so an
  object-to-metadata table whose metadata points back at the object no longer leaks.
- **`__gc` finalizers** on tables and userdata, run through a finalizer registry.
- **`debug` library**: `traceback`, `getinfo`, `sethook` (line and count masks), `getlocal` and
  `setlocal` over a register-name table, up/setupvalue, get/setmetatable.
- **Bytecode**: `string.dump` and a validating loader that rejects malformed input rather than
  trusting it.
- **`async` feature**: await foreign futures from inside a callback, and `Lua::execute_async`.
  `std::task` throughout — choosing a runtime stays the host's.
- **`derive` feature**: `#[derive(FromValue)]` / `#[derive(IntoValue)]`, including generic types.
- **serde**: `SerializeValue` to take a value out into any format, plus an option surface both ways.
- **Sandboxing**: frozen tables, a byte-exact memory ceiling, and `Table::set_intercept_all`.
- **Errors**: source positions (`chunk:line:`), `error` levels, and `BadArgument` carrying the
  argument index as data rather than only in the message.
- API: `UserRef<T>`, `Either`, a prelude, and wider conversions including maps and sets.

### Changed

- `print` formats through `__tostring`, as PUC-Rio's does.
- Numeric semantics now match Lua 5.4 exactly: float `%` with infinities, `2^63` rejected as an
  integer, `math.floor`/`ceil` no longer round large integers through `f64`, `//`/`%`/unary `-` on
  numeric strings keep the integer subtype, `tonumber("inf")` is `nil`, and hex integer literals
  wrap so `0x8000000000000000` is `math.mininteger`.
- String library corrections: `%s` matches vertical tab, `%z` removed (a Lua 5.1 leftover), `%1` in
  a `gsub` replacement means the whole match when the pattern has no captures, `string.format`
  coerces numeric strings for `%d`/`%x`/`%f`, and `%q` emits a hex float so an integral float reads
  back as a float.
- Conversion errors name Lua types: `expected number`, not `expected i64`. A value that is a number
  but has no integer form is now distinguished from a wrong type.
- `StashedError` implements `Debug` and `Display`, so it can be `unwrap`ed, logged, and stored.
- Callbacks run on the real stack rather than a copy, and the thread's value stack has its own lock.
- Release profile defaults to `opt-level = "s"`.
- Licensed under MIT alone (the parents offer MIT or CC0; luna takes the MIT branch).
- `make verify` builds and tests every feature; previously the `async` and `derive` suites compiled
  to zero tests.

### Performance

- Table-valued `__index`/`__newindex` chains are followed in a bounded loop instead of one GC
  callback plus a full executor round-trip per link: **127 ns → 70 ns** on an inherited field read,
  **185 ns → 123 ns** on `o:method()`, and 32 bytes of garbage per inherited read down to **zero**.
- Metamethod names are interned as statics rather than re-hashed as bytes on every metatable probe.
- One process-wide table hash seed instead of one per table (32 bytes and ~18 ns per table).
- Registering a `__gc` object is O(1); it was a linear scan, so N finalizable objects cost O(N²).
- Interned string equality settles on the slice pointer before comparing bytes.
- Calls shift only their arguments instead of the whole frame.

## [0.3.3]
* Bugfix to not reset live threads held in upvalues of dead threads.

## [0.3.2]

* Bugfix for tail-calling uncallable values. Fixes internal panics.
* Major bugfix for finalization, make sure to transition the collector
  immediately to `Collecting` after finalization is done. Fixes lost `Thread`
  finalization and unclosed upvalues.
* Make the `type` builtin match PUC-Rio Lua by @Jengamon.
* Fix Lua stack corruption during tail calls with less arguments than expected.
* Make function statements act like local / upvalue assignment when appropriate.
* Fix `math.random` and `math.log` to better match PUC-Rio Lua by @Jengamon.
* Fix `select` to better match PUC-Rio Lua by @Jengamon.
* Implement `math.randomseed` by @Jengamon.
* Let `__index` and `__newindex` chain through `UserData` in addition to
  `Table`.
* Implement "dead keys" to make table iteration behavior match PUC-Rio Lua.
* Implement `gc_arena::Collect` for `luna_util::UserDataMethods` and
  `luna_util::StaticUserDataMethods`.
* Implement `string.sub`, `string.lower`, `string.upper`, `string.reverse` by
  @Jengamon.
* Better match PUC-Rio Lua behavior with longstring newlines.

## [0.3.1]

Small fixups from 0.3

* Actually export `ExecutorInner`
* Add a missing `#[doc(hidden)]` around an internal macro.

## [0.3]

Huge release! Much safer `Executor` API that no longer requires recursion
from Rust -> Lua -> Rust for Rust callbacks to call Lua functions, eliminating
problems with unrestricted Rust stack usage. The `Executor` API also has a
bunch more weird powers that other implementations of Lua can't have, like "tail
resuming" other coroutines and "tail yield".

There is a new `luna-util` crate that adds support for some very common use
cases that are not trivial to do in `luna` proper:

* Serde support for convenient conversion between Rust types and Lua tables.
* "Freeze" system to safely support the common case where you need to pass
  a non-'static (and non-'gc) value into Lua. Not specific to luna, it is
  actually a general way of safely erasing a single lifetime parameter from a
  type (and replacing it with a runtime check).
* Super quick and simple way to wrap Rust types into a Lua userdata with
  methods.

`luna-util` will always be an **optional** dependency, and it may contain
code that is more opinionated or limited than vanilla `luna` should be.
`luna-util` will have opionions about things, and those opinions may be
different than yours... if it is in your way or incomplete for your use, you can
always use it as a starting point for something better.

Also includes a lot of quality of life API improvements, error message
improvements, and more!

- New `Executor` API that enables safe thread recursion and "tail resume" /
  "tail yield".
- New `luna-util` crate with very commonly requested, useful features that
  are too opinionated or limited to belong in `luna` proper.
- API changes to `Stack` to support a single, unified thread stack shared
  between Lua and callbacks, similar to PUC-Rio Lua et al.
- Upvalues no longer keep entire threads alive and instead use new gc-arena
  finalization support to become closed when threads are garbage collected.
- `IntoMultiValue` / `FromMultiValue` conversion for tuples now allows every
  element to be multi-converted rather than just the last element.
- Support the `__eq` metamethod.
- Error message improvements in lexer / parser errors (they now have line
  numbers at least!).
- API changes to second callback parameter, now an `Execution` type with `Fuel`
  access *and* also calling thread information.
- Add "chunk name" information to compiled chunks for future use in runtime
  errors / tracebacks.
- Simplified `ctx` access, most methods are now directly implemented on `Context`.
- Lots of type renames for clarity, `AnyCallback` -> `Callback`, `AnyUserData`
  -> `UserData`, `AnyValue` -> `Any`, and others.
- Add line number annotations to opcodes for future tracebacks.
- Clean up general ptr handling and allow the user to access internal `Gc`
  pointers in all cases, allows for weak pointers to all pointer types.

## [0.2]
- Allow `Thread` to be forcibly reset to a stopped state.
- Improve the `Table` API, add functions that skip `IntoValue` conversion and
  simplify `Table::next`.
- Support `__newindex`.
- Auto conversion improvements, add a `Variadic` wrapper type to indicate
  variadic multi-values instead of bare arrays.
- Add `Function::compose` and `Function::bind` for easier generic function
  handling from Rust.
- Completely track used memory within interpreter instances. Tracks both
  `gc-arena` allocated `Gc` pointers as well as all normal heap allocations
  using `gc-arena` external allocation tracking.
- `Fuel` system to limit the execution time of Lua code.
- Properly handle `...` in table constructors.
- Implement `table.select('#')`, `table.pack`, and `table.unpack`.
- Fix local function declarations to be visible in their own function body.
- Guard against arbitrary recursion depth of callbacks (only ever a risk for
  Threads calling callbacks on *other* Threads, aka Lua coroutines).

## [0.1.1]
- Initial crates.io release
