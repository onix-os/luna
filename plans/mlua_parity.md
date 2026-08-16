# mlua parity inventory

What luna would need to be a full alternative to [`mlua`](https://github.com/mlua-rs/mlua) for
embedding Lua in Rust — as a pure-Rust VM, with no FFI.

| | |
|---|---|
| **luna** | ~26k LOC |
| **mlua** | 0.12.0 (~23k LOC) |
| **Surveyed** | 2026-08-16 at `dd31437` |
| **Status refreshed** | 2026-08-16, re-probed against a running build — see §0.1 |

## Where this stands

**Most of this document has been implemented.** The survey below is kept as written — it is the
evidence for each gap, and the file:line citations are still where the work happened — but the
current state is the parity map in §0.1. Read that first; the tables after it describe the tree as
it was *before* the work, not as it is now.

Verified against a running build rather than assumed: every item marked done below has a test in
`tests/`, and the whole suite passes under `make verify`.

## Scope

luna is and stays pure Rust. Everything mlua has *only* because it binds a C library is out of
scope and is not counted as a gap — the backend feature matrix (`lua51`…`lua55`, `luajit`,
`luau`), `mlua-sys`, module mode, `lua_State` access, C-function creation, `package.loadlib`,
and the Luau-only value types. Those are listed in Appendix A so it is clear they were
considered rather than overlooked.

Everything else is fair game, including things mlua gets "for free" from PUC-Lua that luna would
have to write. `io`, `os`, `require` and `string.pack` need no C — only work.

The inventory judges **capability, not API shape**. Where luna does the same thing by another
name it is marked `present-different` and both spellings are given, so the entry is useful when
porting code but is not counted against luna.

## How this was produced

Six parallel agents each read both source trees for one dimension — stdlib, values/tables/strings,
functions/chunks/threads/async, userdata/metatables/scope, runtime state and control, and
errors/debug/serde/ergonomics. Each dimension's claims then went to a second agent whose only job
was to **refute** them by hunting for the capability in luna under a different name. 169 raw
findings went in; refuted ones were dropped and corrected statuses applied before synthesis.

The headline claims were then re-checked by hand against a running build rather than taken on
trust — and re-checked again after the implementation pass.

---

## 0.1 Parity map

Refreshed 2026-08-16 against a running build: every "yes" below was exercised, not assumed. The
survey in §1 onward is kept as written — it is the evidence for each gap and the file:line citations
are where the work happened — but it describes the tree *before* the implementation pass. This
section is the current state.

### The language

Complete. A 25-item Lua 5.4 conformance probe passes in full: integer/float subtypes and overflow,
`//`, bitwise operators, `goto`, `<const>`, `<close>`, varargs, metamethods, coroutines, patterns
including frontier `%f`, `string.pack`, `utf8`, proper tail calls (2,000,000 deep, no frame growth).
See COMPATIBILITY.md's Language section for the itemised list.

### Standard library

| Library | luna | mlua (PUC-Lua) | Gap |
|---|---|---|---|
| base | all of it | all | `_VERSION` is `"luna"`; `pairs` returns 2 values not 3 |
| string | all, including `pack`/`unpack`/`packsize` | all | `string.dump` — needs a bytecode format |
| table | all | all | `sort` and `move` are Lua polyfills, not native |
| math | all | all | — |
| coroutine | all | all | — |
| utf8 | all | all | — |
| os | all but `setlocale` | all | `date` is UTC-only (no tz database); `execute` is `/bin/sh`, so POSIX-only |
| io | files, `lines`, `popen`, `seek`, `type` | all | `input`/`output` default streams, `flush`, `tmpfile`, `setvbuf` |
| package | `require`, `path`, `preload`, `loaded` | all | `config`, `searchers`, `searchpath`, `cpath`, `loadlib` (the last two are C-only) |
| debug | `traceback`, `getinfo`, up/setupvalue, get/setmetatable | all | `sethook`, `getlocal`, `getregistry`, `upvalueid`, `uservalue` |

### Metamethods

All of them, including `__gc`, `__close`, `__metatable`, `__name`, `__pairs`. The one gap is
`__mode`: weak *values* work, weak *keys* are deliberately unimplemented rather than silently wrong
(see COMPATIBILITY.md).

### The Rust embedding API

| Capability | luna | mlua | Notes |
|---|---|---|---|
| Call Lua from Rust, Rust from Lua | yes | yes | |
| Userdata with methods and metamethods | yes | yes | `UserRef<T>` covers the common shape without a derive |
| Serde both directions | yes | yes | `SerializeValue` takes a value out into any format |
| Scoped / non-`'static` userdata | yes | yes | luna's is lifetime-branded rather than runtime-checked |
| Sandboxing | yes | yes | `load(env)`, frozen tables, `interceptall` |
| Memory ceiling | yes | only with a custom allocator | luna's is byte-exact and needs no allocator |
| **Interrupt running Lua** | **yes, `Fuel`** | **no** | mlua has no preemption; this is the largest capability difference in luna's favour |
| **Host-paced incremental GC** | **yes** | **no** | collection in slices, between `Executor::step` calls |
| No C toolchain, no `unsafe` boundary | yes | no | |
| `Send`/`Sync` `Lua` | no | yes | architectural; one `Lua` per thread + message passing |
| Real async (foreign futures) | no | yes | needs a `Waker` through `Executor::step` |
| Derive macros for userdata | no | yes | wants a proc-macro crate |
| Argument position in errors | no | yes | `Stack::consume` drops it |

### What remains, in priority order

| Item | Why it is still open | Size |
|---|---|---|
| **Real async** | Awaiting foreign futures needs a real `Waker` plumbed through `Executor::step`, and `Lua::enter` reconciled with being held across an await. The stackless design helps once started. | Large |
| **`Send`/`Sync` `Lua`** | The arena's ownership model is what makes the re-entrancy guarantees sound; changing it is a gc-arena change. One `Lua` per thread and message passing is the answer. | Architectural |
| **Weak keys (`__mode = "k"`)** | Needs ephemeron marking — the collector must iterate to a fixed point. Without it the naive version leaks in exactly the case weak keys are used for. | Medium |
| **Derive macros** | Wants a proc-macro crate in the workspace. `UserRef` removed the sharpest edge without one. | Medium |
| **`string.dump`** | Needs a versioned bytecode format *and* a validating loader, designed from scratch — a malformed chunk must not be able to corrupt the VM. | Medium |
| **`debug.sethook` / `getlocal`** | `sethook` needs a dispatch point in the opcode loop; `getlocal` needs a register→name table the compiler does not emit. `Fuel` already covers the count-hook use case, better. | Medium |
| **`Error::BadArgument`** | Argument position is dropped by `Stack::consume`; threading it through `FromMultiValue` is wider than it looks. | Small |
| **`io.input`/`output`/`flush`/`tmpfile`, `setvbuf`** | Default-stream plumbing. No design problem, just unwritten. | Small |
| **`package.config`/`searchers`/`searchpath`** | Same. `cpath`/`loadlib` are C-only and out of scope. | Small |
| **Native `table.sort`/`move`** | Currently Lua polyfills. Correct but slower than they need to be. | Small |
| **serde option surface** | The recursion crash is fixed and both directions work; the options are not built. | Small |

Out of scope permanently, because they exist only because mlua binds a C library: the backend
matrix (`lua51`…`lua55`, `luajit`, `luau`), `mlua-sys`, module mode, `lua_State` access, C function
creation, `package.loadlib`, `os.setlocale`, and the Luau-only value types. See Appendix A.

---


**Scope rule applied:** everything that exists in mlua only because it binds a C library has been excluded (see [Appendix A](#appendix-a--deliberately-excluded-nafffi)). Refuted findings from the adversarial pass have been dropped; corrected statuses are the ones used below.

**Status vocabulary:** `missing` = a luna user genuinely cannot do it · `partial` = reachable but incomplete/degraded · `present-different` = same capability, different spelling (recorded for porting, not counted as a gap).

---

## 0. Executive summary

| | Count |
|---|---|
| P0 — blocks common embedding use cases | 7 |
| P1 — expected by anyone coming from mlua | 21 |
| P2 — nice to have | 34 |
| P3 — marginal / cosmetic | 11 |
| present-different (no capability lost) | 30+ |
| n/a-ffi (excluded) | 20 |

The three structural themes behind most P0/P1 items:

1. **No file/OS/module surface.** `io`, `os`, `package`/`require`, `dofile`/`loadfile` are all absent, and none of them need FFI.
2. **Errors carry no location.** No `chunk:line:` prefix, no traceback, no `error(msg, level)`, no `xpcall`, no `debug` table. The line data is already compiled in (`src/closure.rs:43 opcode_line_numbers`) and the pc→line search already exists (`src/thread/executor.rs:648-653`); it is an exposure and capture problem, not a data problem.
3. **No resource-cleanup story.** No `__gc`, no `__close`, no `__mode`. Combined, a Lua object can never release a host resource deterministically or on collection.

---

## 1. Standard library coverage

> **These tables are the original survey.** Statuses here are as of `dd31437`, before the work in
> §0.1. They are kept for their evidence and citations, not as a current picture.

### 1.1 Entire libraries absent

| Gap | Status | Prio | Effort | luna evidence | mlua counterpart |
|---|---|---|---|---|---|
| **`io` library** — no file handles at all | missing | **P0** | large | `src/stdlib/io.rs` is 72 lines and sets only the `print` global; `Lua::full()` = `load_core + load_io` (`src/lua.rs:158`). No file-handle userdata type anywhere in `src/`. | `StdLib::IO` (`mlua/src/stdlib.rs:33`), opened at `src/state/raw.rs:1617` |
| **`os` library** — no time, clock, date, getenv, exit | missing | **P0** | medium | No `os.rs` in `src/stdlib/`; `src/stdlib/mod.rs` exports no `load_os`. | `StdLib::OS` (`mlua/src/stdlib.rs:36`) |
| **`package` / `require`** — no module system | missing | **P0** | medium | No `require`, no `package` table anywhere in `src/`. `Closure::load`/`load_with_env` (`src/closure.rs:261,270`) can compile a chunk but nothing caches or resolves by name. | `StdLib::PACKAGE`, `Lua::register_module` (`src/state.rs:536`), `preload_module` (:567) |
| **`dofile` / `loadfile`** | missing | **P0** | small | Not registered in `src/stdlib/base.rs`. Note `src/io.rs skip_prefix` already strips BOM+shebang exactly like `luaL_loadfile`, and is used by no stdlib function. | base library via `StdLib::ALL_SAFE` |
| **`utf8` library** — all six entries | missing | **P1** | small | No `utf8` table is created; `#s` and `string.len` count bytes. Substrate is right — luna strings are byte strings (`src/string.rs:17-20`, `as_bytes` :142). | `StdLib::UTF8` (`mlua/src/stdlib.rs:47`) |
| **`debug` library (Lua-facing)** | missing | **P1** | large | No `debug` table is ever built (`src/lua.rs load_core`/`load_io` load six libraries). `COMPATIBILITY.md:210-234` marks all of it unimplemented. Note `getupvalue`/`setupvalue` are *derivable today* from `Closure::upvalues()` (`src/closure.rs:284`) + `UpValue::get/set` (:170-176); `getlocal`/`setlocal` are not (no local-name table). | `StdLib::DEBUG` (`mlua/src/stdlib.rs:88`) |
| **`warn()` global** | missing | P2 | small | Not in `src/stdlib/base.rs`. The *routing* half is not a gap — globals is a plain table, so a host installs its own sink the same way `print` is installed (`src/stdlib/io.rs:14-16`). mlua's `set_warning_function` is itself 5.4/5.5-gated. | `Lua::set_warning_function` (`mlua/src/state.rs:978`) |

**Effort note on `io`/`os`:** `io.popen` and `os.execute` are *not* FFI blockers — `std::process::Command` implements both in pure Rust. Treat them as policy decisions.

### 1.2 Missing base / coroutine / string functions

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **`load(chunk, chunkname, mode, env)`** — args 2/3/4 silently discarded | partial | **P0** | medium | `src/stdlib/base.rs:345-368` consumes one `Value`, requires `Value::String`, calls `Closure::load(ctx, None, bytes)`. **This is a sandbox escape**, not just a missing feature: Lua code that loads untrusted source into a restricted `_ENV` silently gets the real globals. Rust side is complete (`Closure::load_with_env`, `src/closure.rs:270`); Lua-side `load("local _ENV = ...; ...")({})` is the only workaround. Reader-function chunks are rejected outright. |
| **`xpcall(f, msgh, ...)`** | missing | **P1** | small | `src/stdlib/base.rs:113` registers `pcall` only. The Rust-side analogue exists and is good — `Sequence::error` (`PCall::error`, base.rs:404) intercepts an unwinding error at the intercepting frame. Polyfillable over `pcall` in pure Lua; the handler-runs-before-unwind distinction is currently unobservable *because* luna has no traceback. |
| **`coroutine.wrap`** | missing | **P1** | small | `src/stdlib/coroutine.rs` registers `create, resume, continue, status, yield, yieldto, running` only. Polyfillable in Lua over `create`/`resume`. |
| **`coroutine.close`** | missing | **P1** | small | Not registered. *Not* polyfillable in Lua, but one binding away: `Thread::reset(mc)` (`src/thread/thread.rs:179`) is public and already used by `src/finalizers.rs`. |
| **`string.pack` / `unpack` / `packsize`** | missing | **P1** | medium | `src/stdlib/string/mod.rs load_string` registers 13 functions; these three are absent. Pure byte-buffer work over existing byte strings — nothing needs C. |
| **`rawequal`** | missing | P2 | small | Not in `src/stdlib/base.rs`. Primitive exists at `src/meta_ops.rs:449` (raw compare before `__eq`). |
| **`_G`** | present-different | P2 | small | `_ENV` works fully (`src/compiler/compiler.rs:1424-1466`; `type(_ENV)=="table"`, `_ENV.zzz = 5` verified). Missing is literally `globals.set("_G", globals)`. Dedicated failing test: `tests/scripts-wishlist/globals.lua`. |
| **`collectgarbage` verbs** — only `"count"` | partial | **P1** | medium | `src/stdlib/base.rs:327-341`; every other verb errors "bad argument to 'collectgarbage'". Rust machinery exists (`Lua::gc_collect` src/lua.rs:199, `gc_metrics` :213). **Structural obstacle:** these need `&mut Lua`, which a callback cannot have — implementation requires a request flag on the arena root acted on by `Lua::enter` after `arena.mutate` returns. |
| **`string.dump` / precompiled chunks** | missing | P2 | large | Not registered; `FunctionPrototype` (`src/closure.rs:35`) has no serialization in either direction, and `load` accepts source text only. Raw materials are public (`FunctionPrototype` fields all `pub`, `from_compiled` :49, `CompiledPrototype` compiler.rs:112, `Closure::from_parts` :244) so a downstream crate *could* define a format; luna ships none. Requires designing + versioning a format plus a validating loader. |
| **`string.gmatch` `init` argument** | partial | P2 | small | `src/stdlib/string/mod.rs:301` does `consume::<(String, String)>` — a third arg is silently discarded. `find`/`match` do honour `init` via `normalise_init` (:695). Silent-ignore is worse than an error. |
| **`string.gsub` table replacement ignores `__index`** | partial | P2 | small | `src/stdlib/string/mod.rs:376-377` says so in a comment; :409 uses raw `t.get_value`. The function-replacement path already goes through `async_sequence` (:442-553), so the machinery to fix it is present. |
| **`coroutine.status` never reports `"normal"`** | partial | P2 | small | `src/stdlib/coroutine.rs:50-53` folds `ThreadMode::Running \| Waiting => "running"`. PUC/mlua report `"normal"` for a coroutine that has resumed another (`mlua/src/thread.rs:155`). Will confuse ported scheduler code. |
| **`ipairs`/`pairs` return 2 values, not 3** | present-different | P3 | small | `src/stdlib/base.rs:319-325`; the `inext` callback defaults the control var (`index.unwrap_or(0)`), so generic `for` works. Breaks manual destructuring (`local f,s,var = ipairs(t)`) used by iterator-combinator libraries. |
| **`coroutine.isyieldable`** | present-different | P3 | small | `select(2, coroutine.running())` is exactly it — `running` returns `is_main` (`src/stdlib/coroutine.rs:83-91` ← `Execution::current_thread`, executor.rs:618). |

---

## 2. Metamethods and the object model

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **`__gc` finalizers** | missing (Rust `Drop` runs) | **P1** | large | Zero hits for `__gc` in `src/`/`util/`; not in the `MetaMethod` enum (`src/meta_ops.rs:21-46`). `src/finalizers.rs` finalizes dead `Thread`s only (`FinalizersState { threads: Vec<GcWeak<ThreadInner>> }`), and `register_thread`/`prepare`/`finalize` are `pub(crate)`. Rust `Drop` *does* run for `'static` payloads (gc-arena `types.rs:59 drop_in_place`), but **GC-typed payloads can never have a destructor**: `#[collect(no_drop)]` and `Drop` are made mutually exclusive by gc-arena's `__MustNotImplDrop`. |
| **`__close` / `local x <close>`** | missing | **P1** | medium | Parses (`src/compiler/parser.rs:642 LocalAttributes::CONST_CLOSE`) then hard-errors: `CompileErrorKind::CloseUnsupported` at `src/compiler/compiler.rs:64`, raised at :881. TODOs at compiler.rs:365-366 mark the insertion point. `<const>` works. Failing test: `tests/scripts-wishlist/attributes.lua`. Must also fire on error unwinding and on `goto`/`break`/`return` out of the block. |
| **`__mode` weak tables / ephemerons** | missing | **P1** | large | No hits in `src/table/table.rs` or `src/table/raw.rs`; `__mode` is accepted and silently ignored. **Correctness hazard, not convenience:** code written for PUC that relies on `__mode` leaks under luna with no diagnostic. `GcWeak` is available (used in `src/finalizers.rs:59`), so weak *values* are tractable; true ephemeron semantics (weak keys with values reachable only via the key) are a real collector addition. |
| **`__metatable` protection** | missing | **P1** | small | Neither `getmetatable` nor `setmetatable` (`src/stdlib/base.rs:194-215`) consults it; `Table::set_metatable` (`src/table/table.rs:166`) and `UserData::set_metatable` (`src/userdata.rs:170`) apply unconditionally. This is the *only* mechanism a library has to make a metatable tamper-proof — without it, sandboxing of host objects is trivially defeated from Lua. mlua even relies on it internally (`mlua/src/userdata/util.rs:270`). |
| **`__name` metafield** | missing | **P1** | small | Not in the `MetaMethod` enum; `Value::type_name` (`src/value.rs:31-41`) hard-codes `"userdata"`, `meta_ops::tostring` (:424-447) consults only `__tostring`. Every bound Rust type prints as `userdata: 0x…`. `tests/strings.lua:255` carries a `-- TODO: __name metatable support needed`. |
| **`getmetatable`/`setmetatable` restricted to tables** | partial | **P1** | small | `src/stdlib/base.rs:194-205` errors for anything but `Value::Table`, so `getmetatable("")`, `getmetatable(ud)` and `setmetatable(ud, …)` all fail — even though `meta_ops::get_metatable` (`src/meta_ops.rs:167-173`) handles `Value::UserData` and `UserData::set_metatable` exists on the Rust side. Mitigation: the string-extension idiom still works because `load_string` sets `StringMetatable.__index = string`, so `string.shout = f; ("hi"):shout()` succeeds. |
| **Metamethods on primitives beyond string `__index`** | partial | P2 | medium | `meta_ops::get_metatable` returns `None` for everything but `Table`/`UserData`, so numbers/booleans/functions/nil never dispatch. Strings are special-cased for `__index` only (`src/meta_ops.rs:224-237`); `__concat`, `__len`, `__eq`, `__call`, `__tostring` on strings are ignored. mlua exposes this uniformly via `Lua::type_metatable::<T>` (`src/state.rs:1747`). |
| **`rawlen` on strings** | partial | P3 | small | `src/stdlib/base.rs:174-181` consumes a `Table`. Mostly moot: strings have no `__len`, so `#s` is equivalent. |

**Not a gap — metamethod *dispatch* coverage is on par with mlua's 5.4 build:** `__index`, `__newindex`, `__call`, `__len`, `__tostring`, `__eq`, `__add`..`__pow`, `__unm`, `__idiv`, `__band`/`__bor`/`__bxor`/`__bnot`/`__shl`/`__shr`, `__concat`, `__lt`, `__le` (`src/meta_ops.rs:184-780`) plus `__pairs` (`src/stdlib/base.rs:236-268`), including one-sided-operand cases (`src/meta_ops.rs:547-574`).

---

## 3. Errors, diagnostics and debugging

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **Source position (`chunk:line:`) on runtime errors** | missing | **P0** | medium | `src/meta_ops.rs:148-164` formats `"could not index a nil value"` etc. with no location; `src/thread/vm.rs` never consults `opcode_line_numbers` when pushing `Frame::Error` (`src/thread/executor.rs:461`). The pc *and* the line table are both in hand at that exact point. Verified: `pcall(function() local x=nil return x.y end)` → `"operator error"`. Note the detail is not lost, only hidden — `MetaOperatorError` renders the real message but `src/thread/mod.rs:29 #[error("operator error")]` wraps it as an anyhow source recoverable only via `RuntimeError::root_cause`. Compile errors *do* carry position. |
| **Stack tracebacks** | missing | **P0** | large | `Error<'gc>` (`src/error.rs:152`) has two variants and no frame field. Everything needed is `pub(super)`: `Frame` (`src/thread/thread.rs:288`), `ThreadState.frames/stack/open_upvalues` (:329-333), `ExecutorState.thread_stack` (`src/thread/executor.rs:44-46`, private, no accessor). **Capture must happen at throw time** — executor.rs:468-491 pops one `Frame::Lua` per unwind step, so by the time the error leaves `Executor::step` every frame is gone. Coroutine chains need `thread_stack` walked too. Callback frames would render as `?` (`CallbackInner`, `src/callback.rs:88-98`, is a fn pointer with no name). README lists "stack traces" under "Doesn't yet". |
| **`error(message, level)`** | partial | **P0** | small | `src/stdlib/base.rs:95-98` is `Err(stack.get(0).into())` — the level arg is never even consumed, and the `Execution` param is discarded. Equivalent to PUC `error(msg, 0)` always, per luna's own `COMPATIBILITY.md:36`. **Level 1 is implementable today** with `Execution::upper_lua_frame()`; level 2+ needs the frame-walk API. |
| **Debug hooks (call / return / line)** | missing | **P1** | medium–large | `grep -rni 'hook' src/` returns exactly one prose comment (`src/meta_ops.rs:253`). `src/thread/vm.rs` has no dispatch point. **The count/interrupt half is not a gap** — `Fuel` covers it and covers it better (see §8). What is missing is `on_calls`/`on_returns`/`every_line` with frame info: the basis of step-debuggers, coverage tools and line-attributing profilers. |
| **`Error::BadArgument { to, pos, name, cause }`** | missing | **P1** | medium | `src/error.rs:11-16 TypeError { expected: &'static str, found: &'static str }` is the only conversion error; `Stack::consume`/`from_front`/`from_back` (`src/stack.rs:141-156`) and tuple `from_multi_value` (`src/conversion.rs:520`) all drop the position. `string.sub(t,'x')` → `type error, expected integer, found string`. Hand-written `"bad argument #N to 'f'"` literals exist in three places (`src/stdlib/string/format.rs:206`, `src/stdlib/table.rs:72`, `src/stdlib/base.rs:351`) but that is not a mechanism. |
| **Multi-frame stack introspection** | partial | **P1** | large | Only `Execution::upper_lua_frame()` (`src/thread/executor.rs:636-668`) → one `UpperLuaFrame { chunk_name, current_function, current_line }`, obtainable only from inside a running `Callback`, only if the caller is a closure. No `inspect_stack(level)` analogue (`mlua/src/state.rs:1042`). |
| **Local variable names in debug info** | missing | P2 | large | `CompiledPrototype` (`src/compiler/compiler.rs:112-125`) carries no local-name or register→name table; `UpValueDescriptor` (`src/types.rs:36-41`) is unnamed too. Means error messages can never say `attempt to index a nil value (local 'cfg')` and a debugger can never show a variables pane. Requires emitting a scope/name table from the register allocator. |
| **`Function::info()` for callbacks** | partial | P2 | small | For closures, `Closure::prototype()` (`src/closure.rs:280`) covers most of mlua's `FunctionInfo` — `chunk_name`, `FunctionRef::{Named(name,line),Expression(line),Chunk}`, `fixed_params`, `has_varargs`, `upvalues.len()` — minus `last_line_defined`, `short_src`, `name_what`. **Nothing at all for `Callback`**, and no `Function`-level accessor (the caller must match the enum). |
| **Dynamic detail on conversion errors** | partial | P2 | small | Both `TypeError` fields are `&'static str`, so `src/conversion.rs:222-225` smuggles the reason into the type slot (`found: "integer out of range"`), discarding the actual runtime type and the offending value. |
| **`.context()` on `Result<T, Error<'gc>>` / annotating Lua-raised errors** | partial | P2 | small | Rust-side chaining is fully covered by `RuntimeError(Arc<anyhow::Error>)` + `root_cause`/`is`/`downcast` (`src/error.rs:102-140`). What has nowhere to go is context on `Error::Lua(LuaError(Value))` — wrapping a Lua-raised value would change its identity as seen by `pcall`. |

**Present-different / luna is fine here:** `impl<E: Into<anyhow::Error>> From<E> for Error<'gc>` (`src/error.rs:184`) makes bare `?` work on any std error inside a callback, where mlua needs `.into_lua_err()`. REPL incomplete-input detection is present as `ParseErrorKind::EndOfStream` with a `line_number` mlua doesn't expose (`src/compiler/parser.rs:329,346`; used by `examples/interpreter.rs:61-73`). The error taxonomy is collapsed to two variants but typed errors stay recoverable by downcast — genuinely absent *concepts* are only `MemoryError` (no limit exists), `StackError` (no frame-depth cap in `ThreadState::push_call`), and `SafetyError` (nothing unsafe to refuse).

---

## 4. Async and concurrency

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **Await real Rust futures from a callback** | missing | **P0** | large | `src/async_callback.rs` *looks* async and explicitly is not: `noop_waker()` at :517 is the only waker ever passed to the future (:366), and :392 `expect("await of a future other than AsyncSequence methods")` panics on any foreign await. The doc comment at :25-33 says so outright. No `futures`/`tokio` dependency exists in the tree. A user cannot expose `async fn` doing network I/O, timers or channels to Lua. |
| **`Thread`/`Executor` as `Future`/`Stream`** | missing | **P1** | large | No `impl Future`/`impl Stream` anywhere in `src/`. `Executor::step(ctx, &mut Fuel)` (`src/thread/executor.rs:186`) is a synchronous pump and `Lua::finish` (`src/lua.rs:267`) is a blocking loop; `Lua::enter` takes `&mut self` so it cannot be held across an await point. Consuming a coroutine as an async iterator requires hand-rolling a poll loop. |
| **Async userdata methods** | partial | P2 | medium | The primitives exist (`async_sequence`, `CallbackReturn::Yield`) but `UserMethods::add`/`add_write` (`util/src/user_methods.rs:88,115`) hard-code `Ok(CallbackReturn::Return)`, and the module doc at :20-26 puts non-simple returns out of scope. Every async method must be a hand-written `Callback` inserted into `UserMethods::metatable()`. |
| **`Send`/`Sync` `Lua`, `Clone` handle, `WeakLua`** | missing | **P1** | large | `Lua` owns `Arena<Rootable![State<'_>]>` by value (`src/lua.rs:132-134`); every method takes `&mut self`/`&self`; no `Clone`, no weak handle. `grep 'unsafe impl Send|Sync'` over `src/`+`util/src/` returns only `ExternLuaError` (`src/error.rs:93-94`), and gc-arena declares none at all (its `Metrics` is `Rc`-backed). A luna state cannot be moved to a worker thread, `tokio::spawn`ed, or stored in Send application state. Workaround: pin each `Lua` to a thread and message it. |
| **Thread lifecycle event callbacks** | partial | P2 | medium | No `set_thread_event_callback` analogue. Creation/resume/yield *are* observable at the Lua level because `coroutine` is a plain global table (`src/stdlib/coroutine.rs:93`) that a host can wrap. Not observable: destruction/collection (`Finalizers` is `pub(crate)`) and threads created from Rust via `Thread::new` (`src/thread/thread.rs:77`), which bypass the wrappers. |

---

## 5. Userdata and binding ergonomics

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **Derive macros (`#[derive(UserData)]`, `#[userdata_impl]`, `#[derive(FromLua)]`)** | missing | **P1** | large | No proc-macro crate in the workspace (`Cargo.toml` members = `["util"]`). Manual equivalent is `UserMethods::{new,add,add_write,metatable,wrap}` / `StaticUserMethods` (`util/src/user_methods.rs:47-204`), one hand-written registration per method with an explicit `&'static str`. Exposing a 15-method struct is ~15 calls vs one attribute. **Largest per-line ergonomics difference between the two libraries.** |
| **Registration surface: field getters/setters, meta helpers, static fields, associated functions** | partial | **P1** | medium | Only `add` (immutable self) and `add_write` (write-barriered self); `StaticUserMethods` has no `add_write`. No field getter/setter helpers, no `add_meta_method` sugar, no constructor/static registration, no self-describing trait — the doc at `util/src/user_methods.rs:21-26` says as much. Metamethods *are* reachable manually because `UserMethods::metatable()` (:68) is public and `MetaMethod: IntoValue` (`src/meta_ops.rs:113`). |
| **Two divergent construction paths (silent inert userdata)** | partial | **P1** | medium | `UserData::new_static` (`src/userdata.rs:93`) attaches **no metatable**, so the object is inert in Lua (indexing it hard-errors, `src/meta_ops.rs:208-223`) with no compile-time signal. Only `UserMethods::wrap` (`util/src/user_methods.rs:151`) attaches one. mlua's `create_any_userdata::<T>` attaches the registered metatable automatically. |
| **Typed payload extraction as an argument (`UserDataRef<T>` as `FromLua`)** | partial | P2 | small | `FromValue` is implemented for `UserData<'gc>` itself only (`src/conversion.rs:285-291`), so `stack.consume::<(MyType, i32)>()` is impossible out of the box; `util/src/user_methods.rs:96-98` re-does the downcast by hand in every generated callback. `FromValue` is public and unsealed, so a user can write `impl FromValue for &'gc MyType` in ~10 lines — luna just ships no helper, and there is no borrow-tracking `RefMut` analogue. |
| **`take()` / `destroy()` / destructed tombstone** | missing | P2 | medium | `src/userdata.rs:66-181` exposes new/is/downcast/downcast_write/metatable/set_metatable only. Consuming methods (`ud:into_inner()`) require wrapping the payload in `RefCell<Option<T>>` by hand, with no "already consumed" error. |
| **Serializable userdata** | missing | P2 | medium | No serialization hook on `UserData`. `util/src/serde/de.rs:63-71` special-cases only the `none`/`unit` marker singletons and otherwise hard-errors `"cannot deserialize from userdata"` — so a config table containing one bound object fails wholesale. |
| **Wrapper forwarding (`Rc<RefCell<T>>`, `Arc<Mutex<T>>`)** | missing | P2 | medium | `downcast_static::<T>` matches the exact `TypeId`, and `UserMethods<U>` is monomorphised on one `U`, so every wrapper shape needs full re-registration. mlua's `userdata-wrappers` (`mlua/src/userdata/util.rs:17-193`) is pure-Rust dispatch, so in scope. |
| **Distinguishable userdata errors** | partial | P2 | small | One unit struct `BadUserDataType` = `"UserData type mismatch"` (`src/userdata.rs:14-16`) for wrong-type, wrong-self and every downcast variant, with no method name or expected type; borrow conflicts aren't modelled at all (a `RefCell` payload panics). |
| **Per-instance uservalues (payload-agnostic)** | present-different | P2 | small | Two working spellings: per-instance metatable via `UserData::set_metatable` (unreachable from Lua since `getmetatable` rejects userdata), or a GC payload like `Rootable![RefLock<Vec<Value<'_>>>]` mutated after creation with `downcast_write`. Absent is a payload-agnostic `set_user_value`/`user_value` pair. |
| **`__todebugstring`** | missing | P3 | small | Not in `MetaMethod`; `Value`'s Debug prints `Value::UserData({:p})` (`src/value.rs:114-116`). mlua-specific, not Luau-gated, so in scope. |
| **`ObjectLike` (`ud:method()` from Rust in one call)** | present-different | P2 | medium | Equivalent is `meta_ops::index`/`new_index`/`call` returning `MetaResult`/`MetaCall` driven through an `Executor` — inherent to the stackless design (see §9), not an oversight. No `get_path` helper either. |
| **`UserData` trait / blanket `IntoLua`** | present-different | P2 | medium | `IntoValue` is public and unsealed (`src/conversion.rs:7`), so `impl IntoValue for MyType` calling `ctx.singleton::<…>().wrap(ctx, self)` is legal and idiomatic. Same work as `impl UserData for MyType`; what is absent is one trait bundling registration + conversion, and any derive. |

---

## 6. Rust-side API: conversions, tables, ergonomics

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **Map/set conversions (`HashMap`, `BTreeMap`, `HashSet`, `BTreeSet`)** | missing | **P1** | small | `src/conversion.rs` covers only `Option`, `Vec`, `&[T]`, `[T;N]`, `Variadic`. The most common Rust→Lua shape must be built with a manual `Table::set` loop and read back with a hand-rolled `Table::iter` + per-key/value conversion. |
| **`&[u8]`/`Vec<u8>` silently become a table of integers** | partial (footgun) | **P1** | small | `src/conversion.rs:120,130` + `u8: IntoValue` from `impl_copy_into!` means `&[u8]` converts to a 256-element table, **not** a Lua string, with no compile error. The byte-string capability itself is fully present (`ctx.intern`, `String::from_slice`, `from_buffer`, `as_bytes`) — it just has no conversion impl, so the wrong one wins. |
| **`Either<L,R>`** | missing | P2 | small | No `Either` anywhere. A callback cannot express "string or number" in its signature; it must take `Value` and match. |
| **`char` / `OsString` / `PathBuf` / `CString`** | missing | P2 | small | No impls in either direction. `PathBuf`/`OsString` are somewhat moot while there is no `io`/`os` library, but they are the impls that preserve non-UTF-8 paths. |
| **Shifting `insert`/`remove` on `Table` from Rust** | partial | P2 | small | push/pop/remove-by-key are one-liners (`Table::set` returns the previous value, `src/table/table.rs:88`), but the shift loops exist only as private helpers `array_insert_shift`/`array_remove_shift` (`src/stdlib/table.rs:742,792`) behind the Lua-level callbacks. |
| **`Table::clear`** | partial | P2 | small | Emptying is sanctioned and tested (`Table::next` doc at `src/table/table.rs:145-147`; `tests/table.rs:36-43`), but `Key::Dead` tombstoning (`src/table/raw.rs:548-573`) means buckets are never reclaimed, unlike mlua's `clear`. |
| **`FromValue` for `usize`/`isize`/`i128`/`u128`** | partial | P2 | small | `impl_int_from!(i64,u64,i32,u32,i16,u16,i8,u8)` (`src/conversion.rs:238`). A callback taking an index must declare `i64` and re-do the bounds check. |
| **`IntoValue` for `u64`/`usize`/`isize`/`i128`/`u128`** | partial | P3 | small | `impl_int_into!` stops at `u32` (`src/conversion.rs:47`). Values are representable via `Value::Integer(i64::try_from(x)?)`; the real loss is mlua's automatic i64-overflow→`Number` promotion. |
| **`IntoValue` for runtime `&str`/`&String`/`Cow<str>`/`Box<str>`** | partial | P3 | small | Only `&'static str` and owned `StdString` (`src/conversion.rs:87,93`). `ctx.intern(s.as_bytes())` is the one-call workaround and is what luna's own stdlib uses everywhere. Ergonomic, not capability. |
| **`Eq`/`Hash` for `Value<'gc>`** | partial | P2 | small | `src/value.rs:10` derives `Debug, Copy, Clone, Collect` only. `meta_ops::equal` (`src/meta_ops.rs:449`) reproduces mlua's raw `PartialEq` with no Executor except for `__eq` on two distinct tables/userdata. The internal `CanonicalKey` (`src/table/raw.rs:482`) *does* derive `Eq, Hash` but is private, so a `HashMap<Value, _>` needs a user-written newtype. |
| **`get_path("a[1].c")`** | partial | P2 | small | Nested access works as chained `get_value` + nil checks; what the user must write themselves is the path-string parser. |
| **`prelude` module and `Result<T>` alias** | missing | P2 | small | No `prelude`, no `pub type Result`. `src/lib.rs:27-49` re-exports `String`, `Error`, `Table`, `Value`, `Function` under bare names, so `use luna::*` shadows `std::string::String` and `std::error::Error`. Three distinct error types appear in signatures — `Error<'gc>`, `TypeError`, `ExternError` — with no alias. |
| **`chunk!` inline-Lua macro / `AsChunk` trait** | missing | P2 | medium | `Closure::load(ctx, name, source: &[u8])` is the only entry point — no `Path`/`Read`/named-chunk abstraction, no `$var` capture. |
| **Chunk builder (`eval` expression fallback, file sources)** | partial | P2 | medium | `Closure::load`/`load_with_env` cover name + env + into-function. Missing: expression/statement auto-fallback (`examples/interpreter.rs:14-17` hand-rolls `"return " + code` and retries — exactly `mlua/src/chunk.rs:590`), mode selection, path sources (`examples/execute.rs:8-11` does `File::open` + `read_to_end` by hand while `src/io.rs buffered_read` sits unused by the stdlib), and direct `exec`/`call`. |
| **`bool` conversion rejects nil/truthy** | present-different | P2 | small | `src/conversion.rs:285` accepts only `Value::Boolean`; truthiness is the separate `Value::to_bool()`. mlua's `FromLua for bool` coerces. **Silent porting hazard:** `fn(ctx, flag: bool)` errors where mlua accepted. |
| **`Result<T,E>` → `(true, v)` / `(false, e)`, not `(v)` / `(nil, e)`** | present-different | P2 | small | `src/conversion.rs:371-399` prepends a pcall-style boolean. Ported mlua code returns an extra leading `true` and Lua callers written for the `nil, err` idiom misread results. Workaround: return `(Value::Nil, e)` explicitly. |
| **`Value::to_pointer`** | present-different | P3 | small | Obtainable as `Gc::as_ptr(x.into_inner())` — exactly what `Value::display` and `Key::kill` do. Caveat: luna does not re-export `ottavino-gc-arena`, so callers add it as a direct dependency (which the public API already forces). |
| **`Table == [T]`, typed `pairs::<K,V>()`, `create_table_from`, `Box<[T]>`, `LuaString::wrap`, `Value::as_*` accessors** | present-different | P3 | small | All compose from existing public API (`Table::length`/`get_value`, `FromValue`, `RawTable::with_capacity` + `Table::from_parts`, `Vec` round-trip, `ctx.intern`, public enum variants). Sugar only. |

---

## 7. Host control plane

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **Memory limit enforcement** | partial | **P0** | medium | No allocation-granular cap: `Lua::total_memory()` (`src/lua.rs:194`) is read-only and gc-arena's `MetricsAlloc::allocate` counts but never refuses; no `memory_limit` symbol exists. A coarse equivalent *is* available thanks to the stackless design — check `total_memory()` between `Executor::step` slices and call `Executor::stop`/`resume_err`. But it is slice-granular, not per-allocation, and not a pcall-catchable Lua-level error. **This directly contradicts README's claim that "CPU and memory both have ceilings you set"** — only the CPU half exists. |
| **Read-only / frozen tables (sandbox hardening)** | partial | **P1** | medium | No readonly flag on `Table` (`src/table/table.rs`), so `rawset` and `__newindex` always succeed. Two scripts sharing one `string` table — the normal setup, since `load_string` writes into shared globals — can sabotage each other (`string.format = evil`). Composable pieces exist (`Lua::empty()` + per-library loaders, `Closure::load_with_env`); the freeze primitive does not. mlua's `Lua::sandbox` is itself Luau-only, but the frozen-table *concept* is portable. |
| **`gc_step` with a host-specified budget** | partial | P2 | small | `Lua::enter` *is* the bounded incremental step (`arena.collect_debt`/`mark_debt` on exit, `src/lua.rs:228-252`), and `lua.enter(\|_\| ())` forces a slice. Missing: a host-specified step size, and the combination "stop pacing then step manually" does not work (with pacing disabled `allocation_debt()` stays 0 and `enter` does nothing; `Lua::arena` is private). |
| **`gc_is_running`** | missing | P3 | small | Stop/restart *are* achievable: `Metrics::set_pacing(Pacing::with_min_sleep(usize::MAX))` drives `allocation_debt()` to 0 forever, and restoring `Pacing::default()` resumes. There is no readout, so the host must track its own flag. `Pacing`/`Metrics` are not re-exported from `src/lib.rs`. |
| **Named (string-keyed) registry values** | present-different | P3 | small | Three lines: a `Singleton` returning a `Table`, reached via `ctx.singleton::<…>()` and keyed with `Table::get`/`set` — the pattern already used twice in-tree (`src/error.rs:227`, `src/stdlib/string/mod.rs:687`). Never reachable from Lua since it is never put in globals. No shipped wrapper. |
| **App data (`set_app_data`/`remove_app_data`)** | present-different | P2 | small | Same singleton-container pattern, plus `Callback::from_fn_with(mc, root, call)` for rooting host state directly. The one real difference: every access needs `lua.enter`, where mlua's works from `&Lua` anywhere. |
| **Checked registry fetch / `owns_registry_value`** | present-different | P2 | small | `Registry::roots()` (`src/registry.rs:80`) is public and hands out the `DynamicRootSet`, whose `try_fetch` and `contains` are public. Only the `Stashed*` convenience wrappers (`src/stash.rs`) hide their `DynamicRoot` and force the panicking `Registry::fetch` (`src/registry.rs:115`, documented as panicking on a foreign handle). luna is *better* on lifecycle: `DynamicRoot::drop` frees the slot, so mlua's `expire_registry_values` is unnecessary. |
| **`set_globals` (swap the globals table wholesale)** | present-different | P3 | small | mlua's own doc says it affects only newly-loaded chunks — which is exactly `Closure::load_with_env`. Retro-fitting an existing closure = `Closure::upvalues()` + `UpValue::set` on upvalue 0. |
| **`Lua::full()` is a misleading name** | doc bug | P3 | trivial | `src/lua.rs:158-162` — it is core + `print`, nothing more. |

---

## 8. Numeric and semantic divergences from PUC-Lua 5.4

These are behaviour differences, not missing features, but they silently change results for ported code.

| Divergence | Prio | luna evidence | PUC/mlua behaviour |
|---|---|---|---|
| **`tostring`/`print` of floats is lossy** | **P1** | `src/value.rs:64` uses Rust `Display`; `meta_ops.rs:445` routes `tostring` through it. `print(1.0)` → `1`, `print(7//2.0)` → `3`, `print(1/3)` → 16 digits, `print(0/0)` → `NaN`, `print(1e100)` → a 101-digit expansion. | `%.14g` + `.0` suffix. **The correct code already exists and is unused by `tostring`:** `format_float_g`/`format_number` in `src/stdlib/string/format.rs:832`. You cannot tell an integer from a float by printing it — the primary way 5.4 users reason about the int/float split. |
| **Integer/float comparison above 2^53 gives wrong answers** | P2 | `src/constant.rs:227,231,239-256` cast `i64 as f64`. Verified: `math.maxinteger == (math.maxinteger + 0.0)` → `true`; `(math.maxinteger-1) < (math.maxinteger+0.0)` → `false`. | PUC compares exactly. Silently corrupts sorts and range checks on large integer IDs. |
| **`math.modf` returns an integer and saturates** | P2 | `src/stdlib/math.rs:252-256` = `(f as i64, f % 1.0)`. `math.modf(1e100)` → `9223372036854775807, 0`. | Integral part is always a float; `1e100, 0.0`. Second defect is a wrong answer, not a type nuance. |
| **`math.fmod` always float, no zero check** | P2 | `src/stdlib/math.rs:243-250` coerces both args to f64. `pcall(math.fmod, 5, 0)` → `true, NaN`. | Two integers → integer; zero divisor → `bad argument #2 to 'fmod' (zero)`. |
| **`math.tointeger` accepts strings** | P2 | routes through `Value::to_integer` → `to_numeric`. `math.tointeger("3")` → `3`. | → `fail`. Accepting strings makes it unusable as the "convertible without loss" predicate. |
| **String→number arithmetic yields float** | P3 | `src/constant.rs:97-116` takes the integer path only when both operands are already `Integer`, discarding the `Integer` that `to_numeric` already computed. `math.type("10"+1)` → `"float"`. | `"integer"`. Float-ness is contagious (table-key identity at 2^53, `math.type` dispatch, `%d` acceptance). |
| **Bitwise operators accept strings** | P3 | `src/constant.rs:179-213` funnels through `to_integer`. `("10" \| 1)` → `11`. | PUC raises. luna is *more* permissive, so code written against luna breaks on PUC and a class of typo bugs goes undetected. |
| **`math.randomseed` returns nothing** | P3 | `src/stdlib/math.rs:319-355` returns `Some(())`. | Returns the two seed components (how 5.4 reproduces an unseeded run). |
| **`_VERSION == "luna"`** | P3 | `src/stdlib/base.rs:343`. | Portable libraries version-sniff on `_VERSION` and take their 5.1 fallback branch or error. Defensible identity choice; consider `"Lua 5.4"` plus a separate luna version global. |

**Correct and complete in this area** (verified, no action): wrapping integer arithmetic, floor-division rounding toward −inf, integer div/mod by zero raising, `/` and `^` always float, float table keys normalising to integer keys with NaN rejected (`src/table/raw.rs:502-514`), and every other `math` entry (`abs, acos, asin, atan(y,x), ceil, cos, deg, exp, floor, huge, log(x,base), max, min, maxinteger, mininteger, pi, rad, random` incl. `random(0)`, `sin, sqrt, tan, type, ult`).

---

## 9. Serde bridge

| Gap | Status | Prio | Effort | luna evidence |
|---|---|---|---|---|
| **No recursion guard in the deserializer → stack overflow** | missing | **P1** | medium | `util/src/serde/de.rs:26 from_value` takes no options at all. `local t={} t.self=t` drives `deserialize_any → deserialize_map → next_value_seed → deserialize_any` forever and **crashes the process** instead of erroring. (`src/compiler/parser.rs:1056 recursion_guard` shows luna knows the pattern.) mlua has `RecursionGuard` (`mlua/src/serde/de.rs:714-733`). |
| **No deserializer options at all** (`deny_unsupported_types`, `sort_keys`, empty/mixed-table policy) | missing | P2 | medium | Unsupported types are always hard errors (`de.rs:61-71`); no key ordering, so map order follows `Table::next` and is not reproducible; `is_sequence` (`de.rs:622-646`) hard-codes empty-table-as-array and always-on mixed detection, and re-walks the whole table on every node (O(n) per node). |
| **No array metatable / serializer options** | partial | P2 | medium | `util/src/serde/ser.rs:19-32 Options` has one field, `serialize_none`. `SerializeSeq::new` (:245) builds a bare `Table`, so an empty `Vec` round-trips as `{}` and cannot be distinguished from an empty map through JSON. No `serialize_unit_to_null` toggle, no arbitrary-precision detection. The `none`/`unit` singletons (`util/src/serde/markers.rs`) are the working equivalent of mlua's `null()`. |
| **`impl Serialize for Value`** | partial | P2 | medium | No `impl serde::Serialize` anywhere. Streaming *out* is reachable via `from_value::<serde_json::Value>` or `serde_transcode` over the self-describing `Deserializer` (`util/src/serde/de.rs:43-75`), but `serde_json::to_writer(w, &lua_value)` does not compile and a `Value` cannot be a field of a `#[derive(Serialize)]` struct. No `SerializableValue` per-call builder. |
| **Serializable userdata** | missing | P2 | medium | See §5. |
| **Serde error types not unified with `luna::Error`** | partial | P3 | small | `util/src/serde/{de,ser}.rs` define two independent `thiserror` types in the *second crate*; they reach `Error<'gc>` only through the blanket anyhow `From`, erasing structure unless downcast, and are not re-exported from `luna`. |
| **The whole serde bridge lives in `luna-util`** | present-different | P3 | — | `util/Cargo.toml` `default = ["serde"]`. A bare `luna` dependency has no serde bridge. |

---

## 10. Where luna is better than mlua

Stated plainly, with citations — these are real advantages, not consolation.

| Advantage | Evidence |
|---|---|
| **Fuel metering beats debug hooks for preemption.** `Fuel` (`src/fuel.rs`) is threaded through `Executor::step(ctx, &mut Fuel)` (`src/thread/executor.rs:186`) with per-instruction, per-callback and per-sequence-step debits; any callback can call `Execution::fuel().interrupt()` to suspend *immediately, resumably, without unwinding*; `Executor::stop` cancels outright. mlua's count hook can only *error out* of the script (destroying it), and its yielding variant (`set_interrupt`) is Luau-only and only fires at yieldable points. luna suspends any script at any point on its only backend. |
| **The stackless VM means the host always gets control back.** `Executor::step` returns cleanly mid-execution and can simply not be resumed. This is what makes slice-granular memory checks possible at all (§7) and what makes `Fuel` sound. |
| **`?` works on any std error inside a callback.** Blanket `impl<E: Into<anyhow::Error>> From<E> for Error<'gc>` (`src/error.rs:184`) vs mlua's `.into_lua_err()` adapter. Full chain/downcast story via `RuntimeError::{root_cause, is, downcast}`. |
| **Zero-copy string access with no guard types.** `String::to_str() -> Result<&'gc str>` and `as_bytes() -> &'gc [u8]` (`src/string.rs:142,178`) — the `'gc` brand removes the need for mlua's `BorrowedStr`/`BorrowedBytes` keep-alive wrappers entirely. |
| **Typed stashed handles.** `StashedTable`/`StashedString`/`StashedExecutor`/… (`src/stash.rs`) stay typed across arena boundaries, and `StashedValue::as_primitive`/`to_bool` inspect without re-entering — where mlua has one opaque `RegistryKey`. `DynamicRoot::drop` frees the slot, so mlua's `expire_registry_values` bookkeeping is unnecessary. |
| **Direct, safe upvalue read/write from Rust.** `Closure::upvalues()` + `UpValue::get`/`set` (`src/closure.rs:170-176,284`). mlua surfaces no general upvalue accessor at all — reaching them requires calling Lua's `debug.getupvalue`. |
| **GC-branded userdata payloads.** `UserData::new::<R: Rootable>` (`src/userdata.rs:77`) accepts any `'gc` garbage-collected type and keeps its type identity through `downcast` — more general than mlua's scope, which loses `TypeId` for non-`'static` types (`mlua/src/scope.rs:144-153`). |
| **`Thread::resume_err` is portable.** `src/thread/thread.rs:167`. mlua's `resume_error` is `#[cfg(feature = "luau")]` (`mlua/src/thread.rs:371-373`). |
| **Byte-exact memory accounting.** `total_allocation` sums GC-box bytes *plus* externally-tracked Vec/HashMap bytes; mlua's non-custom-allocator fallback is kilobyte-granular (`mlua/src/state.rs:1081`). |
| **Rust panics propagate natively.** No `catch_unwind`/`resume_unwind` machinery is needed because there are no C frames to longjmp across; a script can never observe or swallow a Rust panic, which is mlua's *non-default* (safer) setting. |
| **Tighter default posture.** No `os`, no `package`, no `debug`, no C loading, no `unsafe_new`, no FFI surface at all. `Lua::empty()` + per-library loaders that take a `Context` (callable mid-run) is finer-grained than mlua's `StdLib` bitmask. |
| **Extras with no mlua counterpart.** `Function::compose` (`src/function.rs:37`); `coroutine.continue`/`coroutine.yieldto` (`src/stdlib/coroutine.rs`); `ParseError.line_number` for REPLs (mlua's `SyntaxError` exposes only a bool); `Frozen`/`FreezeGuard`/`FrozenScope` (`util/src/freeze.rs`) returning `AccessError::Expired` instead of mlua's invalidated-userdata error. |
| **String patterns and `string.format` are complete.** `src/stdlib/string/pattern.rs` (702 lines) and `format.rs` (1222 lines) implement every character class, sets/ranges/negation, all four quantifiers, `%b`, `%f`, position captures, back-references, pattern validation; and `d i u o x X f e E g G a A c s q p` with flags/width/precision including hex-float `%a` and Lua-specific `%q`. All independently re-verified at runtime. |

---

## 11. Architecturally hard or impossible

These are consequences of the stackless + arena-GC design, not oversights. Some are worth accepting permanently.

| Item | Why it is hard | Verdict |
|---|---|---|
| **Synchronous re-entrant Rust→Lua calls** — `Table::get` honouring an `__index` *closure*, `Value::to_string` honouring `__tostring`, `Table::len` honouring `__len`, metamethod-aware `equals` | Documented at `src/table/table.rs:30-34`: "luna does not (and cannot) silently trigger running Lua code". The metamethod-aware paths return `MetaResult`/`MetaCall` (`src/meta_ops.rs:123-134`) that must be driven on an `Executor`. A plain `lua.enter` block cannot resolve an `__index` chain ending in a Lua closure. | **Accept.** Narrower than it sounds: `meta_ops::index` short-circuits whenever the raw lookup hits or `__index` is a table, `meta_ops::equal` returns inline for every case except `__eq` on two distinct tables/userdata, and `tostring` only needs the Executor when `__tostring` actually exists. |
| **`Function::call` from inside a callback** | `Executor` doc (`src/thread/executor.rs:50-66`): reentrant method calls panic. The sanctioned route is returning `CallbackReturn::Call { function, then }` or `SequencePoll::Call`. | **Accept.** A callback cannot synchronously obtain a Lua function's return value mid-body; it must be restructured as a `Sequence`. This is the direct trade for §10's resumability guarantees, but it breaks direct ports of mlua code. |
| **`Send`/`Sync` `Lua`** | The arena holds `Box<Context>` with raw-pointer interiors and an `Rc`-backed `Metrics`; `Lua` is owned by value with `&mut self` methods. Making it `Send` is a gc-arena-level change, and the `&mut`-ownership model is what makes the re-entrancy guarantees sound. | **Large / possibly permanent.** Workaround: one `Lua` per thread + message passing. |
| **`__gc` for GC-typed userdata** | gc-arena's `#[collect(no_drop)]` and `Drop` are made mutually exclusive by `__MustNotImplDrop` (`ottavino-gc-arena/src/no_drop.rs`), so non-`'static` payloads can never have a destructor. Lua-visible `__gc` additionally needs resurrection-safe two-stage finalization. | **Hard but shaped.** `src/finalizers.rs` already implements exactly the right prepare/finalize-with-resurrection structure — only threads are registered. |
| **Ephemeron (`__mode="k"`) semantics** | `GcWeak` exists and is used, so weak *values* are tractable; weak keys whose values are reachable only via the key require an ephemeron marking pass in the collector. | **Weak values: medium. Ephemerons: a collector change.** |
| **Real async (awaiting foreign futures)** | `async_callback.rs` deliberately uses `noop_waker` and panics on foreign awaits. Doing it properly means plumbing a real `Waker` through `Executor::step`, making `Executor`/`Thread` a `Future`/`Stream`, and reconciling that with `Lua::enter` taking `&mut self` (it cannot be held across an await). | **Large but tractable** — and the stackless design is an *advantage* here once the waker is plumbed, because suspension is already first-class. |
| **`collectgarbage("collect"/"stop"/"restart")` from Lua** | These need `&mut Lua`, which no callback has. Requires a request flag on the arena root, acted on by `Lua::enter` after `arena.mutate` returns. | **Medium, needs a design decision.** |
| **Traceback capture** | Must snapshot at *throw* time (where `Frame::Error` is first pushed: `src/thread/executor.rs:239/257/278/351/447/461`), because unwinding pops frames one per step (:468-491). `Error<'gc>` needs somewhere to put it, coroutine chains need `ExecutorState.thread_stack` walked, and callback frames have no name (`CallbackInner` is a fn pointer). | **Large, but every prerequisite exists** — `Frame::Lua { closure, pc }`, `opcode_line_numbers`, and the pc→line binary search already demonstrated in `upper_lua_frame`. Step one is making the frame stack publicly walkable. |
| **`debug.sethook`** | Needs a dispatch point inside the opcode loop (`src/thread/vm.rs`), which the stackless design makes awkward but not impossible. `getinfo`/`traceback` do **not** have this problem. | **Split it:** ship `getinfo`/`traceback` first; treat `sethook` separately. |
| **`Table::clear` that actually reclaims** | `Key::Dead` tombstoning (`src/table/raw.rs:548-573`) means setting keys to nil leaves buckets allocated. | **Small, but needs a rehash path.** |
| **Local variable names** | Requires emitting a scope/name table from the register allocator into `CompiledPrototype`. | **Large, and gates** `debug.getlocal`, variables panes, and `(local 'cfg')` in error messages. |

---

## Appendix A — deliberately excluded (n/a-ffi)

Considered and ruled out of scope because they exist in mlua only as bindings to a C library, a C-allocated `lua_State`, or a Luau-only VM primitive.

- **Backends & build modes:** `mlua-sys`; the `lua51`/`lua52`/`lua53`/`lua54`/`lua55`/`luajit`/`luau` feature variants; `module` mode (`Lua::skip_memory_check`); `Lua::entrypoint`/`entrypoint1`.
- **C API surface:** `Lua::unsafe_new`/`unsafe_new_with`, `get_or_init_from_ptr`, `exec_raw`/`exec_raw_lua`, `create_c_function`, `Thread::state() -> *mut lua_State`, `Function::to_pointer`, `AnyUserData::to_pointer`, `get_userdata`/`take_userdata`/`push_uninit_userdata`, the whole of `src/state/raw.rs` and `src/state/util.rs`.
- **C library loading:** `package.loadlib`, `package.cpath`, and C searchers. *(The pure-Lua half of `package` — `require`, `package.path`, `preload`, `loaded`, Lua-file searchers — is **not** excluded and appears as a P0 gap in §1.1.)*
- **Luau/LuaJIT-only stdlibs:** `StdLib::FFI`, `StdLib::JIT`, `StdLib::BUFFER`, `StdLib::VECTOR`, `StdLib::INTEGER`. `StdLib::BIT` is n/a because luna has native 5.3+ `<< >> & | ~` operators (`src/constant.rs:179-213`).
- **Luau value types:** `Value::LightUserData` (`*mut c_void`), `Value::Vector`, `Buffer`/`BufferCursor`. *(The **idea** of a script-visible mutable byte buffer is portable and could be built on `UserData`; nothing in luna does so today. The null-sentinel concept is already served by `util/src/serde/markers.rs`.)*
- **Luau table flags:** `Table::set_readonly`/`is_readonly`/`set_safeenv`. *(The portable frozen-table capability **is** tracked as a P1 gap in §7.)*
- **Luau compiler & JIT knobs:** the whole `Compiler` struct (`set_optimization_level`, `set_vector_ctor`, `add_mutable_global`, `add_library_constant`, `add_disabled_builtin`, …), `Lua::set_compiler`, `enable_jit`, `set_jit_options`, `set_fflag`.
- **Luau-only introspection/cloning:** `Function::coverage`, `Function::deep_clone`. *(Coverage as a concept is portable and `opcode_line_numbers` would support it; it is not an mlua gap on non-Luau backends.)*
- **Luau-only sandbox/interrupt:** `Lua::sandbox`, `Thread::sandbox`, `Lua::set_interrupt`/`remove_interrupt`. *(luna's `Fuel` is the superior portable answer to `set_interrupt` — see §10.)*
- **C-string interop:** `LuaString::as_bytes_with_nul` (relies on PUC's trailing-NUL guarantee).
- **Rust panic containment (`catch_unwind` → `WrappedFailure::Panic` → `resume_unwind`, `Error::PreviouslyResumedPanic`):** exists solely because a Rust unwind cannot cross `lua_error`'s longjmp. luna has no C frames, so propagation is inherent. **One residual note, not a capability gap:** `ThreadState::mode()` is derived from `frames` rather than stored so no mode flag is corrupted, but reusability of a `Lua` after a caught panic is undocumented, and `Executor::step` panics deliberately on inconsistent state (`src/thread/executor.rs:194,491`). Worth a documented policy.

---

## Appendix B — documentation corrections found during the scan

These are cheap and should be fixed regardless of feature work.

1. **`COMPATIBILITY.md:95-104` is wrong.** It marks `string.find`, `format`, `gmatch`, `gsub`, `match`, `rep` as unimplemented. All six are implemented and were re-verified at runtime (`%b()`, `%f[%a]`, back-references, position captures, `%a`, `%q`, `%g`/`%G`/`%e`, `__tostring` via `%s`). The file is stale enough to mislead.
2. **`COMPATIBILITY.md`'s `load` row is stale** — `load` is implemented at `src/stdlib/base.rs:345` (it is *incomplete*, per §1.2, but not absent).
3. **`README` claims "CPU and memory both have ceilings you set."** Only the CPU half exists (§7).
4. **`Lua::full()` loads core + `print`.** The name promises more (`src/lua.rs:158-162`).
5. **`src/io.rs skip_prefix`/`buffered_read` are written and used only by examples** — they are exactly what `loadfile` needs.
