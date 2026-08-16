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
| ⚫️️   | `dump(function[, strip])`         |             |       |
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

File handles are real: `io.open` returns a userdata with a metatable, backed by `std::fs`. `popen` shells
out through `std::process`. What is missing is the default-stream plumbing — `io.input`/`io.output`
and the buffering controls — not the file operations themselves.

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
| 🔵 | `__index`, `__newindex` | Both the table and function forms, chained. | |
| 🔵 | arithmetic, `__concat`, `__len`, `__eq`, `__lt`, `__le`, `__unm` | | |
| 🔵 | `__call` | | |
| 🔵 | `__tostring`, `__name` | | |
| 🔵 | `__metatable` | Protects the metatable from `getmetatable` and `setmetatable`. | |
| 🔵 | `__pairs` | | |
| 🔵 | `__close` | Runs on every scope exit, including error unwinding and coroutine close. | |
| 🔵 | `__gc` | On tables and userdata. Runs during collection, once per object; may resurrect, and is not re-run if it does. An erroring handler is reported through `warn` and does not abort collection. As in PUC-Rio, an object with a finalizer needs two collection cycles to be reclaimed. | |
| 🟡 | `__mode` | **`"v"` only.** See below. | |

### `__mode`

Weak *values* are implemented, and implemented by representation rather than by clearing: a weak
table's slots hold weak pointers, so a cleared entry cannot be read as a dangling one. Entries whose
value has been collected vanish from `get`, `next`, iteration and `#`.

Weak *keys* are **not implemented**, and a `__mode` containing `"k"` is accepted and ignored — the
keys stay strong. This is deliberate and worth understanding before relying on it:

- Correct weak keys need *ephemeron* semantics — a key must be kept alive only if it is reachable
  independently of the table, which requires the collector to iterate marking to a fixed point.
  luna does not do this yet.
- The naive version, without ephemerons, leaks precisely in the case weak keys are usually reached
  for: an object → metadata table where the metadata refers back to the object. The value is strong,
  so it pins the key, so the entry is never collected. Shipping that quietly would be worse than not
  shipping it.

So `__mode = "k"` behaves as a normal strong table, and `__mode = "kv"` behaves as weak-valued.

`__mode` is read once, when the metatable is attached. Changing it afterwards does not retroactively
weaken or strengthen entries. PUC-Rio calls this undefined; this is luna's answer.

## Debug

Partly implemented. The introspection that reads the frame chain or a closure works; the parts that
need either a hook point inside the opcode loop or a register-to-name table the compiler does not
emit do not. `Fuel` covers the count-hook use case, and covers it better.

| Status | Function                                  | Implementation Notes / Differences                                                                                                                                                                        | Notes |
| ------ | ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----- |
| ⚫️    | `debug()`                                 |                                                                                                                                                                                                           |       |
| ❗     | `gethook([thread])`                       |                                                                                                                                                                                                           |       |
| 🟡     | `getinfo([thread, ]f[, what])`            | Level or function. Reports `source`, `short_src`, `what`, `currentline`, `linedefined`, `lastlinedefined`, `nparams`, `isvararg`, `nups`, `func`. No `name`/`namewhat`. The `what` filter argument is ignored; every field is always returned.                                                        |       |
| ⚫️    | `getlocal([thread, ]f, local)`            |                                                                                                                                                                                                           |       |
| 🔵     | `getmetatable(value)`                     | Ignores `__metatable`, which is the point of it.                                                                                                                                                          |       |
| ⚫️    | `getregistry()`                           |                                                                                                                                                                                                           |       |
| 🟡     | `getupvalue(f, up)`                       | luna keeps no upvalue names, so the returned name is the index as a string.                                                                                                                               |       |
| ⚫️    | `getuservalue(u, n)`                      |                                                                                                                                                                                                           |       |
| ❗     | `sethook([thread, ] hook, mask[, count])` |                                                                                                                                                                                                           |       |
| ⚫️    | `setlocal([thread, ]level, local, value)` |                                                                                                                                                                                                           |       |
| 🟡     | `setmetatable(value, table)`              | Tables only, not any value. Interesting thing to note is that this is _not_ the base library `setmetatable`, as `debug.setmetatable`'s first argument accepts any Lua value, while `setmetatable`'s first argument _must_ be a table. |       |
| 🟡     | `setupvalue(f, up, value)`                | As `getupvalue`, the returned name is the index.                                                                                                                                                          |       |
| ⚫️    | `setuservalue(udata, value, n)`           |                                                                                                                                                                                                           |       |
| 🟡     | `traceback([thread,][message, level])`    | Lua frames only — a Rust callback has no frame to describe. A non-string message is returned untouched, as PUC-Rio does. No `thread` or `level` argument.                                                  |       |
| ⚫️    | `upvalueid(f, n)`                         |                                                                                                                                                                                                           |       |
| ⚫️    | `upvaluejoin(f1, n1, f2, n2)`             |                                                                                                                                                                                                           |       |
