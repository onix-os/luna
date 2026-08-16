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

## Base

| Status | Function                                                       | Differences                                                                                                                            | Notes |
| ------ | -------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ----- |
| 🔵     | `assert(v[, message])`                                         |                                                                                                                                        |       |
| 🔵     | `collectgarbage("count")`                                      |                                                                                                                                        |       |
| 🔵    | `collectgarbage("collect")`                                    |                                                                                                                                        |       |
| 🔵    | `collectgarbage("stop")`                                       |                                                                                                                                        |       |
| 🔵    | `collectgarbage("restart")`                                    |                                                                                                                                        |       |
| 🟡    | `collectgarbage("step"[, memkb])`                              | `memkb` is ignored; a step is one incremental slice.                                                                                                                                        |       |
| 🟡    | `collectgarbage("isrunning")`                                  | Answers from the collector's debt rather than a stored flag.                                                                                                                                        |       |
| 🤷‍♀️     | `collectgarbage("incremental"[, gcpause, stepmult, stepsize])` |                                                                                                                                        |       |
| 🤷‍♀️     | `collectgarbage("generational"[, minormult, majormult])`       |                                                                                                                                        |       |
| 🔵    | `dofile([filename])`                                           |                                                                                                                                        |       |
| 🟡     | `error(message)`                                               | Due to `level` not being implemented for, all calls here give the same result as PUC-Lua `error(message, 0)` (or any invalid `level`). |       |
| 🟡    | `error(message, level)`                                        | The `level` argument is accepted and ignored: luna has no source positions to prefix yet.                                                                                                                                        |       |
| 🔵    | `_G` (value)                                                   |                                                                                                                                        |       |
| 🔵     | `getmetatable(object)`                                         |                                                                                                                                        |       |
| 🟡     | `ipairs(t)`                                                    | PUC-Lua returns `iter, table, 0`, where as luna returns `iter, table`.                                                              |       |
| 🔵    | `load(chunk[, chunkname, mode, env])`                          |                                                                                                                                        |       |
| 🔵    | `loadfile([filename, mode, env])`                              |                                                                                                                                        |       |
| 🔵     | `next(table [, index])`                                        |                                                                                                                                        |       |
| 🔵     | `pairs(t)`                                                     | By default, PUC-Lua return `iter, table, nil` where as luna returns `iter, table`.                                                  |       |
| 🔵     | `pcall(f, args...)`                                            |                                                                                                                                        |       |
| 🔵     | `print(args...)`                                               |                                                                                                                                        |       |
| 🔵    | `rawequal(v1, v2)`                                             |                                                                                                                                        |       |
| 🔵     | `rawget(table, index)`                                         |                                                                                                                                        |       |
| 🔵    | `rawlen(v)`                                                    |                                                                                                                                        |       |
| 🔵     | `rawset(table, index, value)`                                  |                                                                                                                                        |       |
| 🔵     | `select(index, args...)`                                       |                                                                                                                                        |       |
| 🔵     | `setmetatable(table, metatable)`                               |                                                                                                                                        |       |
| 🔵    | `tonumber(e[, base])`                                          |                                                                                                                                        |       |
| 🟡     | `tostring(v)`                                                  | luna does not use the metatable field `__name` by default, while PUC-Lua does.                                                      |       |
| 🔵     | `type(v)`                                                      |                                                                                                                                        |       |
| 🔵    | `_VERSION` (value)                                             |                                                                                                                                        |       |
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
| ⚫️️   | `config` (value)                     |                                                                                                 |       |
| ❗     | `cpath` (value)                      |                                                                                                 |       |
| 🔵   | `loaded` (value)                     |                                                                                                 |       |
| ❗     | `loadlib(libname, funcname)`         |                                                                                                 |       |
| 🔵   | `path` (value)                       |                                                                                                 |       |
| 🔵   | `preload` (value)                    |                                                                                                 |       |
| ⚫️️   | `searchers` (value)                  | This implementation will _definitely_ differ from PUC-Lua as luna does not support C loaders |       |
| ⚫️️   | `searchpath(name, path[, sep, rep])` |                                                                                                 |       |

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
| ⚫️️   | `codepoints(s[, i, j, lax])` |             |       |
| 🔵   | `len(s[, i, j, lax])`        |             |       |
| 🔵   | `offset(s, n[, i])`          |             |       |

## Table

| Status | Function                     | Differences | Notes |
| ------ | ---------------------------- | ----------- | ----- |
| 🔵     | `concat(list[, sep, i, j])`  |             | Supports the `__concat` metamethod |
| 🔵     | `insert(list, [pos,] value)` |             |       |
| 🔵     | `move(a1, f, e, t[, a2])`    |             | Currently implemented with a Lua polyfill |
| 🔵     | `pack(args...)`              |             |       |
| 🔵     | `remove(list[, pos])`        |             |       |
| 🔵     | `sort(list[, comp])`         |             | Currently implemented with a Lua polyfill using a simple merge sort, rather than PUC-Rio Lua's quicksort impl |
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

I see a module in the code repo that is labelled the IO library, but it only creates the `print` global, which is not the IO module (as understood from the Lua Manual).

| Status | Function                      | Differences                                                                                                                 | Notes |
| ------ | ----------------------------- | --------------------------------------------------------------------------------------------------------------------------- | ----- |
| 🔵    | `close([file])`               |                                                                                                                             |       |
| ⚫️    | `flush()`                     |                                                                                                                             |       |
| ⚫️    | `input([file])`               |                                                                                                                             |       |
| 🔵    | `lines([filename, args...])`  |                                                                                                                             |       |
| 🔵    | `open(filename [, mode])`     |                                                                                                                             |       |
|        | `output([file])`              |                                                                                                                             |       |
| ⚫️/❗ | `popen(prog[, mode])`         | Might be classifiable as "C weirdness" or it's just creating another process which kinda feels as icky as the OS module imo |       |
| 🔵    | `read(args...)`               |                                                                                                                             |       |
| ⚫️    | `tmpfile()`                   |                                                                                                                             |       |
| 🔵    | `type(obj)`                   |                                                                                                                             |       |
| 🔵    | `write(args...)`              |                                                                                                                             |       |
| 🔵    | `file:close()`                |                                                                                                                             |       |
| 🔵    | `file:flush()`                |                                                                                                                             |       |
| 🔵    | `file:lines(args...)`         |                                                                                                                             |       |
| 🔵    | `file:read(args...)`          |                                                                                                                             |       |
| 🔵    | `file:seek([whence, offset])` |                                                                                                                             |       |
| ⚫️    | `file:setvbuf(mode[, size])`  |                                                                                                                             |       |
| 🔵    | `file:write(args...)`         |                                                                                                                             |       |

## OS

IMO this module is best in its current state, but I cannot stop one from downloading the individual pixels of Henry Cavill's side profile, so...

| Status | Function                        | Differences                                                                                                                                                                                | Notes |
| ------ | ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----- |
| 🔵    | `clock()`                       |                                                                                                                                                                                            |       |
| 🟡    | `date([format, time])`          | Always UTC: luna ships no time-zone database, and `!` is accepted and ignored.                                                                                                                                                                                            |       |
| 🔵    | `difftime(t2, t1)`              |                                                                                                                                                                                            |       |
| ❗     | `execute([command])`            | Because PUC-Lua requires this to be isomorphic to ISO C `system`, I can simply put this under C weirdness!                                                                                 |       |
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
