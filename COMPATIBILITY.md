- ⚫️️ = unimplemented
- 🟡 = differing
- 🔵 = implemented
- ❗= will not implement
- 🤷‍♀️ = low importance

"Implemented" means "near 1:1 PUC-Lua behavior"[^0].

"Differing" means that there is an implementation, but it doesn't correspond to PUC-Lua behavior.

"Unimplemented" means there is no implementation (when used, `nil` is found) _or_
that calling the implementation with the corresponding arguments will error where in PUC-Lua it does not.

"Will Not Implement" is for functions that will not be implemented due to a fundamental difference between luna and PUC-Lua.

"Low Importance" is for things that, while technically implementable, will
likely not be implemented due to differences between luna and PUC-Lua.

**NOTE**: `(a[, b, c])` corresponds to the Lua docs' `(a[, b[, c]])` usage.

## Language

The Lua 5.4 language itself is complete. Every item here is exercised by `tests/`, and the list is
what a 5.4 conformance pass actually asks about rather than a summary.

| Status | Feature | Notes |
| ------ | ------- | ----- |
| 🔵 | Integers and floats as distinct subtypes | `math.type`, integer overflow wraps to `math.mininteger`, `//` and `%` follow the integer/float rules |
| 🔵 | Integer division `//`, and `%` on both subtypes | |
| 🔵 | Bitwise `&` `\|` `~` `<<` `>>` and unary `~` | Reject strings, as 5.4 does — no coercion |
| 🔵 | `goto` and labels | Including jumping out of nested loops |
| 🔵 | `<const>` and `<close>` attributes | `<close>` runs on every exit: normal, `break`/`return`, error unwinding, and coroutine close |
| 🔵 | Varargs, `...`, `select('#', ...)` | `nil`s counted correctly |
| 🔵 | Full metamethod set | See Metamethods below |
| 🔵 | Coroutines | Full round-trip resume/yield with values in both directions, `close`, `isyieldable`, `running`, status transitions |
| 🔵 | Proper tail calls | The stackless design makes these free; unbounded tail recursion does not grow the frame stack |
| 🔵 | Lexical scoping and upvalues | Including upvalues shared between closures, and read/write from re-entrant Rust callbacks |
| 🔵 | String coercion in arithmetic | Integer-preserving, as in 5.4 |
| 🔵 | Long strings and long comments, `\z`, `\x`, `\u{}` escapes | |
| 🟡 | Source positions | Runtime errors carry `chunk:line:`. There is no column information, and `getinfo` reports no `name`/`namewhat` |

## Limits

A host embedding luna runs scripts it may not trust, so the cases where PUC-Lua hangs or dies are
bounded here instead. Each raises an ordinary catchable error.

| Limit | Value | PUC-Lua | Notes |
| ----- | ----- | ------- | ----- |
| Call depth | 100,000 frames | ~200 C levels | `Lua::set_max_call_depth` changes it. Proper tail calls do not count, in either implementation |
| String length | 1 GiB | address space | Applies to `..`, `string.rep` and `string.format`; PUC-Lua's equivalent is an out-of-memory abort |
| `__index`/`__newindex` chain | 2,000 links | 2,000 (`MAXTAGLOOP` is 2,000 in 5.4) | A cyclic chain errors with `'__index' chain too long` rather than looping |
| Execution time | `Fuel` | none | The host, not the script, decides how long a slice runs. PUC-Lua has no equivalent — infinite tail recursion runs forever there |
| `table.sort` length | 2^31 | 2^31 | PUC-Lua's own "array too big". A table's border is not a count — `t[1]=true; t[1<<62]=true` reports `#t` as 2^62 — so a border past a million sorts through the interruptible Lua path rather than preallocating |
| `os.time` fields | each fits an `i32` | C `int` | Rejected with `field 'year' is out-of-bound`, as PUC-Lua does |

## Base

| Status | Function                                                       | Differences                                                                                                                            | Notes |
| ------ | -------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ----- |
| 🔵     | `assert(v[, message])`                                         |                                                                                                                                        |       |
| 🔵     | `collectgarbage("count")`                                      |                                                                                                                                        |       |
| 🔵    | `collectgarbage("collect")`                                    | Collection cannot run while the arena is borrowed, so the verb is a request carried out at the slice boundary — which it also forces, so the effect lands before the next statement. | |
| 🔵    | `collectgarbage("stop")`                                       |                                                                                                                                        |       |
| 🔵    | `collectgarbage("restart")`                                    |                                                                                                                                        |       |
| 🟡    | `collectgarbage("step"[, memkb])`                              | `memkb` is ignored; a step is one incremental slice.                                                                                                                                        |       |
| 🔵    | `collectgarbage("isrunning")`                                  | | |
| 🤷‍♀️     | `collectgarbage("incremental"[, gcpause, stepmult, stepsize])` |                                                                                                                                        |       |
| 🤷‍♀️     | `collectgarbage("generational"[, minormult, majormult])`       |                                                                                                                                        |       |
| 🔵    | `dofile([filename])`                                           |                                                                                                                                        |       |
| 🔵     | `error(message)`                                               | | |
| 🔵    | `error(message, level)`                                        | Levels 0, 1 and 2 all resolve. `level` counts every activation, so a native caller (`pcall(error, "x")`) contributes no position, as in PUC-Rio. | |
| 🔵    | `_G` (value)                                                   |                                                                                                                                        |       |
| 🔵     | `getmetatable(object)`                                         |                                                                                                                                        |       |
| 🔵     | `ipairs(t)`                                                    | | |
| 🔵    | `load(chunk[, chunkname, mode, env])`                          |                                                                                                                                        |       |
| 🔵    | `loadfile([filename, mode, env])`                              |                                                                                                                                        |       |
| 🔵     | `next(table [, index])`                                        |                                                                                                                                        |       |
| 🟡     | `pairs(t)`                                                     | Returns `iter, table`; PUC-Lua returns `iter, table, nil`. The third value is `nil` either way, so `for k, v in pairs(t)` is unaffected. | |
| 🔵     | `pcall(f, args...)`                                            |                                                                                                                                        |       |
| 🔵     | `print(args...)`                                               |                                                                                                                                        |       |
| 🔵    | `rawequal(v1, v2)`                                             |                                                                                                                                        |       |
| 🔵     | `rawget(table, index)`                                         |                                                                                                                                        |       |
| 🔵    | `rawlen(v)`                                                    |                                                                                                                                        |       |
| 🔵     | `rawset(table, index, value)`                                  |                                                                                                                                        |       |
| 🔵     | `select(index, args...)`                                       |                                                                                                                                        |       |
| 🔵     | `setmetatable(table, metatable)`                               |                                                                                                                                        |       |
| 🔵    | `tonumber(e[, base])`                                          |                                                                                                                                        |       |
| 🔵     | `tostring(v)`                                                  | | |
| 🔵     | `type(v)`                                                      |                                                                                                                                        |       |
| 🟡    | `_VERSION` (value)                                             | `"luna"`, not `"Lua 5.4"`. The language targeted is 5.4. | |
| 🔵    | `warn(msg, args...)`                                           |                                                                                                                                        |       |
| 🔵    | `xpcall(f, msgh, args...)`                                     |                                                                                                                                        |       |

[^0]: Hedging b/c I don't know PUC-Lua like my reverse palm, and there might be differing behaviors if you poke both implementations to death, but that's not what this document is for.

## Coroutine

| Status | Function                | Differences | Notes |
| ------ | ----------------------- | ----------- | ----- |
| 🔵   | `close(co)`             |             |       |
| 🔵     | `create(f)`             |             |       |
| 🔵   | `isyieldable([co])`     |             |       |
| 🔵     | `resume(co[, vals...])` |             |       |
| 🔵     | `running()`             |             |       |
| 🔵     | `status(co)`            |             |       |
| 🔵   | `wrap(f)`               |             |       |
| 🔵     | `yield(args...)`        |             |       |

## Package

| Status | Function                             | Differences                                                                                     | Notes |
| ------ | ------------------------------------ | ----------------------------------------------------------------------------------------------- | ----- |
| 🔵   | (global) `require(modname)`          |                                                                                                 |       |
| 🔵   | `config` (value)                     | The five documented lines. The last two describe a C loader, which luna has not.                 |       |
| ❗     | `cpath` (value)                      |                                                                                                 |       |
| 🔵   | `loaded` (value)                     |                                                                                                 |       |
| ❗     | `loadlib(libname, funcname)`         |                                                                                                 |       |
| 🔵   | `path` (value)                       |                                                                                                 |       |
| 🔵   | `preload` (value)                    |                                                                                                 |       |
| 🟡   | `searchers` (value)                  | Two entries — `preload`, then the Lua file searcher — reflecting what `require` consults. Descriptive rather than a hook: replacing an entry does not change `require`, whose search is native. PUC-Lua's C and all-in-one searchers do not exist here. |       |
| 🔵   | `searchpath(name, path[, sep, rep])` | Returns the path found, or `nil` plus the list of candidates tried.                              |       |

## String

| Status | Function                          | Differences | Notes |
| ------ | --------------------------------- | ----------- | ----- |
| 🔵   | `byte(s[, i, j])`                 |             |       |
| 🔵   | `char(args...)`                   |             |       |
| 🟡   | `dump(function[, strip])`         | Chunks only — a nested function reaches `_ENV` through its parent and cannot be reloaded on its own, so it is refused rather than dumped unusably. `strip` drops local names. | |
| 🔵   | `find(s, pattern[, init, plain])` |             |       |
| 🔵   | `format(formatstring, args...)`   |             |       |
| 🔵   | `gmatch(s, pattern[, init])`      |             |       |
| 🔵   | `gsub(s, pattern, repl[, n])`     |             |       |
| 🔵     | `len(s)`                          |             |       |
| 🔵   | `lower(s)`                        |             |       |
| 🔵   | `match(s, pattern[, init])`       |             |       |
| 🔵   | `pack(fmt, values...)`            |             |       |
| 🔵   | `packsize(fmt)`                   |             |       |
| 🔵   | `rep(s, n[, sep])`                |             |       |
| 🔵   | `reverse(s)`                      |             |       |
| 🔵   | `sub(s, i[, j])`                  |             |       |
| 🔵   | `unpack(fmt, s[, pos])`           |             |       |
| 🔵   | `upper(s)`                        |             |       |

## UTF8

| Status | Function                     | Differences | Notes |
| ------ | ---------------------------- | ----------- | ----- |
| 🔵   | `char(args..)`               |             |       |
| 🔵   | `charpattern` (value)        |             |       |
| 🔵   | `codes(s[, lax])`            |             |       |
| 🔵   | `codepoint(s[, i, j, lax])`  |             |       |
| 🔵   | `len(s[, i, j, lax])`        |             |       |
| 🔵   | `offset(s, n[, i])`          |             |       |

## Table

| Status | Function                     | Differences | Notes |
| ------ | ---------------------------- | ----------- | ----- |
| 🔵     | `concat(list[, sep, i, j])`  |             | Supports the `__concat` metamethod |
| 🔵     | `insert(list, [pos,] value)` |             |       |
| 🔵     | `move(a1, f, e, t[, a2])`    |             | Native when neither table has a metatable; falls back to a Lua implementation when `__index`/`__newindex` may run |
| 🔵     | `pack(args...)`              |             |       |
| 🔵     | `remove(list[, pos])`        |             |       |
| 🔵     | `sort(list[, comp])`         |             | Native for the default ordering on a metatable-free list of all numbers or all strings; a comparator or mixed types fall back to a Lua merge sort |
| 🔵     | `unpack(list[, i, j])`       |             |       |

## Math

I'm not going over these with a fine-tooth comb, if it exists (and takes the specified number of arguments), it's considered implemented. (Except for "basic" identities like $\cos(0) = 1$ and stuff like that.)

| Status | Function             | Differences | Notes |
| ------ | -------------------- | ----------- | ----- |
| 🔵     | `abs(x)`             |             |       |
| 🔵     | `acos(x)`            |             |       |
| 🔵     | `asin(x)`            |             |       |
| 🔵     | `atan(y[, x])`       |             |       |
| 🔵     | `ceil(x)`            |             |       |
| 🔵     | `cos(x)`             |             |       |
| 🔵     | `deg(x)`             |             |       |
| 🔵     | `exp(x)`             |             |       |
| 🔵     | `floor(x)`           |             |       |
| 🔵     | `fmod(x, y)`         |             |       |
| 🔵     | `huge` (value)       |             |       |
| 🔵     | `log(x[, base])`     |             |       |
| 🔵     | `max(x, args...)`    |             |       |
| 🔵     | `maxinteger` (value) |             |       |
| 🔵     | `min(x, args...)`    |             |       |
| 🔵     | `mininteger` (value) |             |       |
| 🔵     | `modf(x)`            |             |       |
| 🔵     | `pi` (value)         |             |       |
| 🔵     | `rad(x)`             |             |       |
| 🔵     | `random([m, n])`     |             |       |
| 🔵     | `randomseed([x, y])` |             |       |
| 🔵     | `sin(x)`             |             |       |
| 🔵     | `sqrt(x)`            |             |       |
| 🔵     | `tan(x)`             |             |       |
| 🔵     | `tointeger(x)`       |             |       |
| 🔵     | `type(x)`            |             |       |
| 🔵     | `ult(m, n)`          |             |       |

## I/O

File handles are real: `io.open` returns a userdata with a metatable, backed by `std::fs`. `popen`
shells out through `std::process`, and `io.input`/`io.output` redirect the default streams that
`io.read`, `io.write`, `io.lines()` and `io.flush` use. The one thing that does not do what it says
is `setvbuf`, below.

| Status | Function                      | Differences                                                                                                                 | Notes |
| ------ | ----------------------------- | --------------------------------------------------------------------------------------------------------------------------- | ----- |
| 🔵    | `close([file])`               |                                                                                                                             |       |
| 🔵    | `flush()`                     | Flushes the current output stream.                                                                                          |       |
| 🔵    | `input([file])`               | A handle or a filename; `io.read` and `io.lines()` follow it.                                                                |       |
| 🔵    | `lines([filename, args...])`  |                                                                                                                             |       |
| 🔵    | `open(filename [, mode])`     |                                                                                                                             |       |
| 🔵    | `output([file])`              | A handle or a filename; `io.write` and `io.flush` follow it.                                                                |       |
| 🔵    | `popen(prog[, mode])`         | Over `std::process`, not C `popen`. Read and write modes; `close` reports the exit status.                                  |       |
| 🔵    | `read(args...)`               |                                                                                                                             |       |
| 🔵    | `tmpfile()`                   | Created then immediately unlinked, so it disappears when the handle is dropped.                                              |       |
| 🔵    | `type(obj)`                   |                                                                                                                             |       |
| 🔵    | `write(args...)`              |                                                                                                                             |       |
| 🔵    | `file:close()`                |                                                                                                                             |       |
| 🔵    | `file:flush()`                |                                                                                                                             |       |
| 🔵    | `file:lines(args...)`         |                                                                                                                             |       |
| 🔵    | `file:read(args...)`          |                                                                                                                             |       |
| 🔵    | `file:seek([whence, offset])` |                                                                                                                             |       |
| 🟡    | `file:setvbuf(mode[, size])`  | Accepted and reported as succeeding, but the mode is not honoured: reads go through a `BufReader` and writes go straight out. |       |
| 🔵    | `file:write(args...)`         |                                                                                                                             |       |

## OS

IMO this module is best in its current state, but I cannot stop one from downloading the individual pixels of Henry Cavill's side profile, so...

| Status | Function                        | Differences                                                                                                                                                                                | Notes |
| ------ | ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----- |
| 🔵    | `clock()`                       |                                                                                                                                                                                            |       |
| 🟡    | `date([format, time])`          | Always UTC: luna ships no time-zone database, and `!` is accepted and ignored.                                                                                                                                                                                            |       |
| 🔵    | `difftime(t2, t1)`              |                                                                                                                                                                                            |       |
| 🟡    | `execute([command])`            | Runs through `/bin/sh -c` rather than ISO C `system`, so it is POSIX-only. With no argument, reports that a shell is available. |       |
| 🔵    | `exit([code, close])`           | Probably a❗, but I cannae tell you want to do                                                                                                                                             |       |
| 🔵    | `getenv(varname)`               | ...what is this a shell script?                                                                                                                                                            |       |
| 🔵    | `remove(filename)`              |                                                                                                                                                                                            |       |
| 🔵    | `rename(oldname, newname)`      |                                                                                                                                                                                            |       |
| ❗     | `setlocale(locale[, category])` | This is _explictly_ not going to be implemented according to the README, along with its C weirdness brethren, I just have problems with the rest of this module. _Personnel_ problems \\s. |       |
| 🔵    | `time([table])`                 |                                                                                                                                                                                            |       |
| 🔵    | `tmpname()`                     |                                                                                                                                                                                            |       |

## Metamethods

| Status | Metamethod | Implementation Notes / Differences | Notes |
| ------ | ---------- | ---------------------------------- | ----- |
| 🔵 | `__index`, `__newindex` | Both the table and function forms, chained. A chain of *tables* is followed in one step rather than one executor round-trip per link; a cycle is caught by the chain limit under Limits. | |
| 🔵 | arithmetic, `__concat`, `__len`, `__eq`, `__lt`, `__le`, `__unm` | | |
| 🔵 | `__call` | | |
| 🔵 | `__tostring`, `__name` | | |
| 🔵 | `__metatable` | Protects the metatable from `getmetatable` and `setmetatable`. | |
| 🔵 | `__pairs` | | |
| 🔵 | `__close` | Runs on every scope exit, including error unwinding and coroutine close. | |
| 🔵 | `__gc` | On tables and userdata. Runs during collection, once per object; may resurrect, and is not re-run if it does. An erroring handler is reported through `warn` and does not abort collection. As in PUC-Rio, an object with a finalizer needs two collection cycles to be reclaimed. | |
| 🔵 | `__mode` | `"v"`, `"k"` and `"kv"`. Weak keys use ephemeron marking. See below. | |

### `__mode`

Weak *values* and weak *keys* are both implemented, by representation rather than by clearing: a
weak table's slots hold weak pointers, so a cleared entry cannot be read as a dangling one. Entries
whose weak side has been collected vanish from `get`, `next`, iteration and `#`.

| Mode | Keys | Values |
| --- | --- | --- |
| `"v"` | strong | weak |
| `"k"` | weak | weak during marking, restored for entries whose key survived (see below) |
| `"kv"` | weak | weak |

**Strings are never removed**, on either side, as Lua 5.4 §2.5.4 requires. A string is collectable
but it is a *value*: two equal strings are the same string, so one of them being unreferenced says
nothing about the entry. Held strongly, and compared by content — a weak table answers `t[b]` for a
key stored as an `a` with equal bytes, which it would not if the key were matched by address.

A weak table keeps every entry in its map part rather than its array part, including integer keys
and values reached through `table.insert`; the array part holds values strongly and cannot express
an entry that has gone away. The cost is that `#` on a weak table searches for its border through
`get` instead of a binary search over the array.

**Weak keys use real ephemeron marking.** The naive implementation — weak key, strong value — leaks
in exactly the case weak keys are reached for: an object-to-metadata table where the metadata refers
back to the object. The value is strong, so it keeps the key alive, so the entry can never be
collected. luna therefore holds a `"k"` table's values weakly *during marking*, so that marking
cannot reach a key through its own value, and then puts back the values of entries whose key
survived independently. That pass repeats until the set of live keys stops growing, because reviving
one value can make another table's key reachable. `tests/weak_tables.rs` pins the cases a single
pass would get wrong: a value referring to its own key, two entries referring to each other's keys,
and an anchored chain that must survive end to end.

`"kv"` is deliberately *not* given that treatment: with both sides weak, a value must die when
nothing else holds it however alive its key is, so restoring it would be wrong.

**What it costs.** A weak-key table's values survive one collection cycle longer than strictly
necessary, the same as an object with a `__gc` handler, because the pass roots them for the rest of
the cycle it runs in. Carrying weak keys also widened the table's internal key representation, a
cost paid by every table rather than only weak ones. (Earlier revisions of this file quoted 3% and
6% for that; those figures predate the current key representation, which stores the insertion hash
and keeps string keys strong, and have not been re-measured.)

`__mode` is read once, when the metatable is attached. Changing it afterwards does not retroactively
weaken or strengthen entries. PUC-Rio calls this undefined; this is luna's answer.

## Debug

Mostly implemented, including the parts that need a dispatch point inside the opcode loop
(`sethook`) and a register-to-name table from the compiler (`getlocal`/`setlocal`). What is missing
is the registry and uservalue accessors, and the call and return hook masks — those would have to
fire from every path that pushes or pops a frame, several of which have no context to call Lua
from, so they are rejected rather than accepted and silently ignored.

Note for anyone sandboxing: `debug.setlocal` writes into another frame's stack slot, so a script
holding `debug` can rewrite its caller's variables. `Lua::load_debug` is separate from
`Lua::load_core` for that reason.

| Status | Function                                  | Implementation Notes / Differences                                                                                                                                                                        | Notes |
| ------ | ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----- |
| ⚫️    | `debug()`                                 |                                                                                                                                                                                                           |       |
| 🟡     | `gethook([thread])`                       | Returns the hook and its mask. No `thread` argument: hooks are per-`Lua`, not per-coroutine.                                                                                              |       |
| 🟡     | `getinfo([thread, ]f[, what])`            | Level or function. Reports `source`, `short_src`, `what`, `currentline`, `linedefined`, `lastlinedefined`, `nparams`, `isvararg`, `nups`, `func`. No `name`/`namewhat`. The `what` filter argument is ignored; every field is always returned.                                                        |       |
| 🟡     | `getlocal([thread, ]f, local)`            | Takes a level, not a function: naming a function's parameters without an activation is the less useful half. Locals are indexed in declaration order and filtered to those live at the frame's current instruction. Needs `debug_locals` to have been on when the chunk compiled — it is by default. No `thread` argument. |       |
| 🔵     | `getmetatable(value)`                     | Ignores `__metatable`, which is the point of it.                                                                                                                                                          |       |
| ⚫️    | `getregistry()`                           |                                                                                                                                                                                                           |       |
| 🟡     | `getupvalue(f, up)`                       | luna keeps no upvalue names, so the returned name is the index as a string.                                                                                                                               |       |
| ⚫️    | `getuservalue(u, n)`                      |                                                                                                                                                                                                           |       |
| 🟡     | `sethook([thread, ] hook, mask[, count])` | Line (`"l"`) and count hooks. Call (`"c"`) and return (`"r"`) masks are **rejected with an error** rather than accepted and ignored — they would have to fire from every path that pushes or pops a frame, several of which have no context to call Lua from. No `thread` argument: a hook is per-`Lua`. A hook is suppressed while it runs, so one that runs Lua cannot trigger itself. |       |
| 🟡     | `setlocal([thread, ]level, local, value)` | As `getlocal`; writes through to the variable and returns its name. No `thread` argument.                                                                                                  |       |
| 🟡     | `setmetatable(value, table)`              | Tables only, not any value. Interesting thing to note is that this is _not_ the base library `setmetatable`, as `debug.setmetatable`'s first argument accepts any Lua value, while `setmetatable`'s first argument _must_ be a table. |       |
| 🟡     | `setupvalue(f, up, value)`                | As `getupvalue`, the returned name is the index.                                                                                                                                                          |       |
| ⚫️    | `setuservalue(udata, value, n)`           |                                                                                                                                                                                                           |       |
| 🟡     | `traceback([thread,][message, level])`    | Lua frames only — a Rust callback has no frame to describe. A non-string message is returned untouched, as PUC-Rio does. No `thread` or `level` argument.                                                  |       |
| ⚫️    | `upvalueid(f, n)`                         |                                                                                                                                                                                                           |       |
| ⚫️    | `upvaluejoin(f1, n1, f2, n2)`             |                                                                                                                                                                                                           |       |
