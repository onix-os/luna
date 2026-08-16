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
