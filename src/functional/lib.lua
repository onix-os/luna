-- The `lib` global of functional states: merging, list and map helpers, structural equality,
-- identity rewriting, encodings, schemas and modules. Every function returns new tables and
-- leaves its arguments unchanged, so it works on frozen data.

local native = ...
local lib = {}
local types = {}

local function is_list(value)
    if type(value) ~= "table" then
        return false
    end
    local count = 0
    for _ in pairs(value) do
        count = count + 1
    end
    return count > 0 and count == #value
end

local function describe(value)
    local kind = type(value)
    if kind == "string" then
        return string.format("string %q", value)
    elseif kind == "number" or kind == "boolean" then
        return kind .. " " .. tostring(value)
    elseif kind == "table" and is_list(value) then
        return "a list"
    end
    return kind
end

local function child(path, key)
    if type(key) == "string" and key:match("^[%a_][%w_]*$") then
        return path .. "." .. key
    elseif type(key) == "string" then
        return path .. string.format("[%q]", key)
    end
    return path .. "[" .. tostring(key) .. "]"
end

lib.is_list = is_list

-- Merging: `merge` is Nix's `//`, `deep_merge` its `recursiveUpdate`.

function lib.merge(...)
    local out = {}
    for i = 1, select("#", ...) do
        for k, v in pairs((select(i, ...))) do
            out[k] = v
        end
    end
    return out
end

function lib.deep_merge(base, override)
    local out = {}
    for k, v in pairs(base) do
        out[k] = v
    end
    for k, v in pairs(override) do
        local old = out[k]
        if type(old) == "table" and type(v) == "table" and not is_list(old) and not is_list(v) then
            out[k] = lib.deep_merge(old, v)
        else
            out[k] = v
        end
    end
    return out
end

-- Lists.

function lib.map(list, f)
    local out = {}
    for i, v in ipairs(list) do
        out[i] = f(v, i)
    end
    return out
end

function lib.filter(list, keep)
    local out = {}
    for i, v in ipairs(list) do
        if keep(v, i) then
            out[#out + 1] = v
        end
    end
    return out
end

function lib.fold(list, initial, f)
    local acc = initial
    for _, v in ipairs(list) do
        acc = f(acc, v)
    end
    return acc
end

function lib.concat(...)
    local out = {}
    for i = 1, select("#", ...) do
        for _, v in ipairs((select(i, ...))) do
            out[#out + 1] = v
        end
    end
    return out
end

function lib.flatten(lists)
    return lib.concat(table.unpack(lists))
end

function lib.range(first, last)
    local out = {}
    for i = first, last do
        out[#out + 1] = i
    end
    return out
end

function lib.find(list, test)
    for i, v in ipairs(list) do
        if test(v, i) then
            return v, i
        end
    end
    return nil
end

function lib.any(list, test)
    return lib.find(list, test) ~= nil
end

function lib.all(list, test)
    for i, v in ipairs(list) do
        if not test(v, i) then
            return false
        end
    end
    return true
end

function lib.contains(list, value)
    return lib.any(list, function(v)
        return lib.equal(v, value)
    end)
end

function lib.unique(list)
    local out = {}
    for _, v in ipairs(list) do
        if not lib.contains(out, v) then
            out[#out + 1] = v
        end
    end
    return out
end

function lib.sort(list, less)
    local out = {}
    for i, v in ipairs(list) do
        out[i] = v
    end
    table.sort(out, less)
    return out
end

function lib.group_by(list, key_of)
    local out = {}
    for _, v in ipairs(list) do
        local key = key_of(v)
        local group = out[key] or {}
        group[#group + 1] = v
        out[key] = group
    end
    return out
end

-- Maps.

function lib.keys(map)
    local out = {}
    for k in pairs(map) do
        out[#out + 1] = k
    end
    return out
end

function lib.values(map)
    local out = {}
    for _, v in pairs(map) do
        out[#out + 1] = v
    end
    return out
end

function lib.map_values(map, f)
    local out = {}
    for k, v in pairs(map) do
        out[k] = f(v, k)
    end
    return out
end

function lib.filter_values(map, keep)
    local out = {}
    for k, v in pairs(map) do
        if keep(v, k) then
            out[k] = v
        end
    end
    return out
end

function lib.from_pairs(list)
    local out = {}
    for _, pair in ipairs(list) do
        out[pair[1]] = pair[2]
    end
    return out
end

-- Strings. Separators, affixes and replacements are plain text, never patterns.

function lib.split(text, separator)
    if separator == "" then
        error("split needs a non-empty separator", 2)
    end
    local out = {}
    local start = 1
    while true do
        local first, last = string.find(text, separator, start, true)
        if first == nil then
            out[#out + 1] = string.sub(text, start)
            return out
        end
        out[#out + 1] = string.sub(text, start, first - 1)
        start = last + 1
    end
end

function lib.join(list, separator)
    return table.concat(list, separator or "")
end

function lib.replace(text, from, to)
    return table.concat(lib.split(text, from), to)
end

function lib.starts_with(text, prefix)
    return string.sub(text, 1, #prefix) == prefix
end

function lib.ends_with(text, suffix)
    return suffix == "" or string.sub(text, -#suffix) == suffix
end

function lib.remove_prefix(text, prefix)
    if lib.starts_with(text, prefix) then
        return string.sub(text, #prefix + 1)
    end
    return text
end

function lib.remove_suffix(text, suffix)
    if suffix ~= "" and lib.ends_with(text, suffix) then
        return string.sub(text, 1, #text - #suffix)
    end
    return text
end

local function is_space(byte)
    return byte == 32 or (byte >= 9 and byte <= 13)
end

function lib.trim(text)
    local first, last = 1, #text
    while first <= last and is_space(string.byte(text, first)) do
        first = first + 1
    end
    while last >= first and is_space(string.byte(text, last)) do
        last = last - 1
    end
    return string.sub(text, first, last)
end

-- Remove the indentation every non-blank line shares, and a blank first line, as Nix's `''`
-- strings do. Blank lines become empty.
function lib.dedent(text)
    local lines = lib.split(text, "\n")
    if #lines > 1 and string.find(lines[1], "^%s*$") then
        table.remove(lines, 1)
    end
    local indent = math.huge
    for _, line in ipairs(lines) do
        if string.find(line, "%S") then
            indent = math.min(indent, #string.match(line, "^[ \t]*"))
        end
    end
    local out = {}
    for i, line in ipairs(lines) do
        out[i] = string.find(line, "%S") and string.sub(line, indent + 1) or ""
    end
    return table.concat(out, "\n")
end

-- `path` without trailing slashes, keeping a lone "/".
local function strip_slashes(path)
    local last = #path
    while last > 1 and string.byte(path, last) == 47 do
        last = last - 1
    end
    return string.sub(path, 1, last)
end

function lib.basename(path)
    return string.match(strip_slashes(path), "([^/]*)$")
end

function lib.dirname(path)
    local parent = string.match(strip_slashes(path), "^(.*)/[^/]*$")
    if parent == nil then
        return "."
    elseif parent == "" then
        return "/"
    end
    return parent
end

-- A shell word for `text`: unchanged when it is only safe characters, otherwise single-quoted.
function lib.escape_shell(text)
    if string.find(text, "^[%w,._+:@%%/-]+$") then
        return text
    end
    return "'" .. string.gsub(text, "'", "'\\''") .. "'"
end

-- Structure.

function lib.equal(a, b)
    local seen = {}
    local function same(x, y)
        if x == y then
            return true
        end
        if type(x) ~= "table" or type(y) ~= "table" then
            return false
        end
        local pairs_seen = seen[x] or {}
        if pairs_seen[y] then
            return true
        end
        pairs_seen[y] = true
        seen[x] = pairs_seen
        for k, v in pairs(x) do
            if not same(v, y[k]) then
                return false
            end
        end
        for k in pairs(y) do
            if x[k] == nil then
                return false
            end
        end
        return true
    end
    return same(a, b)
end

function lib.copy(value)
    local copies = {}
    local function copy(node)
        if type(node) ~= "table" then
            return node
        end
        local existing = copies[node]
        if existing ~= nil then
            return existing
        end
        local out = {}
        copies[node] = out
        for k, v in pairs(node) do
            out[copy(k)] = copy(v)
        end
        local metatable = getmetatable(node)
        if metatable ~= nil then
            setmetatable(out, metatable)
        end
        return out
    end
    return copy(value)
end

-- Rewriting: replace nodes by identity and rebuild only what contains them, sharing the rest.
-- `replace` is a table keyed by the nodes to replace, or a function returning a replacement or
-- nil. Each node is visited once, so shared nodes stay shared.

function lib.rewrite(tree, replace)
    local lookup = type(replace) == "table" and function(node)
        return replace[node]
    end or replace
    local done = {}
    local function walk(node)
        if type(node) ~= "table" then
            return node
        end
        local existing = done[node]
        if existing ~= nil then
            return existing
        end
        local replacement = lookup(node)
        if replacement ~= nil then
            done[node] = replacement
            return replacement
        end
        done[node] = node
        local changed = {}
        local any = false
        for k, v in pairs(node) do
            local new = walk(v)
            if new ~= v then
                changed[k] = new
                any = true
            end
        end
        if not any then
            return node
        end
        local out = {}
        for k, v in pairs(node) do
            local new = changed[k]
            if new ~= nil then
                out[k] = new
            else
                out[k] = v
            end
        end
        local metatable = getmetatable(node)
        if metatable ~= nil then
            setmetatable(out, metatable)
        end
        done[node] = out
        return out
    end
    return walk(tree)
end

-- Encodings: text in canonical key order, and parsed data.

function lib.to_json(value, options)
    return native.encode(value, "json", options ~= nil and options.pretty == true)
end

function lib.to_toml(value)
    return native.encode(value, "toml")
end

function lib.to_yaml(value)
    return native.encode(value, "yaml")
end

function lib.to_ini(value)
    return native.encode(value, "ini")
end

function lib.from_json(text)
    return native.decode(text, "json")
end

function lib.from_toml(text)
    return native.decode(text, "toml")
end

-- Definitions: values modules give an option, each with a priority (lower wins) and an order
-- (lower comes first in merged lists). The `mk_*` wrappers set them.

local OPTION, WRAP = {}, {}
local NORMAL, ORDER, OPTION_DEFAULT = 100, 1000, 1500

local function wrap(kind, fields)
    fields.kind = kind
    return setmetatable(fields, WRAP)
end

function lib.mk_override(priority, value)
    return wrap("override", { priority = priority, content = value })
end

function lib.mk_default(value)
    return lib.mk_override(1000, value)
end

function lib.mk_force(value)
    return lib.mk_override(50, value)
end

function lib.mk_if(condition, value)
    if type(condition) ~= "boolean" then
        error("mk_if expects a boolean condition, got " .. describe(condition), 2)
    end
    return wrap("if", { condition = condition, content = value })
end

function lib.mk_merge(values)
    return wrap("merge", { contents = values })
end

function lib.mk_order(order, value)
    return wrap("order", { order = order, content = value })
end

function lib.mk_before(value)
    return lib.mk_order(500, value)
end

function lib.mk_after(value)
    return lib.mk_order(1500, value)
end

local function definition(value, priority, order, from)
    return { value = value, priority = priority, order = order, from = from }
end

local function expand_into(value, priority, order, from, out)
    if getmetatable(value) ~= WRAP then
        out[#out + 1] = definition(value, priority, order, from)
    elseif value.kind == "if" then
        if value.condition then
            expand_into(value.content, priority, order, from, out)
        end
    elseif value.kind == "merge" then
        for _, content in ipairs(value.contents) do
            expand_into(content, priority, order, from, out)
        end
    elseif value.kind == "override" then
        expand_into(value.content, value.priority, order, from, out)
    else
        expand_into(value.content, priority, value.order, from, out)
    end
    return out
end

local function expand(defs)
    local out = {}
    for _, def in ipairs(defs) do
        expand_into(def.value, def.priority, def.order, def.from, out)
    end
    return out
end

-- The unwrapped definitions with the winning priority.
local function winning(defs)
    local out = expand(defs)
    local best = lib.fold(out, math.huge, function(lowest, def)
        return math.min(lowest, def.priority)
    end)
    return lib.filter(out, function(def)
        return def.priority == best
    end)
end

local function sources(defs)
    return table.concat(
        lib.unique(lib.map(defs, function(def)
            return def.from
        end)),
        ", "
    )
end

local function shape(value)
    if type(value) ~= "table" then
        return type(value)
    elseif next(value) == nil then
        return "empty"
    elseif is_list(value) then
        return "list"
    end
    return "map"
end

local function merge_equal(defs, path)
    for i = 2, #defs do
        if not lib.equal(defs[1].value, defs[i].value) then
            local lines = { path .. ": conflicting definitions" }
            for _, def in ipairs(defs) do
                lines[#lines + 1] = "  " .. describe(def.value) .. " in " .. def.from
            end
            error(table.concat(lines, "\n"), 0)
        end
    end
    return defs[1].value
end

local function merge_with(t, defs, path)
    local winners = winning(defs)
    if #winners == 0 then
        return nil
    end
    return (t.merge or merge_equal)(winners, path)
end

local function merge_lists(defs, path)
    local ordered = {}
    for index, def in ipairs(defs) do
        local kind = shape(def.value)
        if kind ~= "list" and kind ~= "empty" then
            error(path .. ": expected a list, got " .. describe(def.value) .. " in " .. def.from, 0)
        end
        ordered[index] = { def = def, index = index }
    end
    table.sort(ordered, function(a, b)
        if a.def.order ~= b.def.order then
            return a.def.order < b.def.order
        end
        return a.index < b.index
    end)
    local out = {}
    for _, entry in ipairs(ordered) do
        for _, value in ipairs(entry.def.value) do
            out[#out + 1] = value
        end
    end
    return out
end

-- Definitions of a map grouped by key, each keeping the priority and order it came with.
local function by_key(defs, path)
    local keyed = {}
    for _, def in ipairs(defs) do
        local kind = shape(def.value)
        if kind == "map" or kind == "empty" then
            for key, value in pairs(def.value) do
                local list = keyed[key] or {}
                keyed[key] = list
                list[#list + 1] = definition(value, def.priority, def.order, def.from)
            end
        else
            error(path .. ": expected a map, got " .. describe(def.value) .. " in " .. def.from, 0)
        end
    end
    return keyed
end

-- `item` is the type of every value, or a function from a key to the type of its value.
local function item_type(item, key)
    if type(item) == "function" then
        return item(key)
    end
    return item
end

local function merge_map(item, defs, path)
    local out = {}
    for key, list in pairs(by_key(defs, path)) do
        out[key] = merge_with(item_type(item, key), list, child(path, key))
    end
    return out
end

local function merge_any(defs, path)
    local lists, maps, others = false, false, false
    for _, def in ipairs(defs) do
        local kind = shape(def.value)
        lists = lists or kind == "list"
        maps = maps or kind == "map"
        others = others or (kind ~= "list" and kind ~= "map" and kind ~= "empty")
    end
    if others or (lists and maps) then
        return merge_equal(defs, path)
    elseif lists then
        return merge_lists(defs, path)
    end
    return merge_map(types.any, defs, path)
end

-- Schemas: `lib.check(value, type)` returns the value with defaults filled in, or raises an error
-- naming the exact path and what was expected there. Each type also merges the definitions modules
-- give an option of that type: lists concatenate, maps and records merge by key, and anything else
-- must be defined equally everywhere.

local function fail(path, expected, value)
    error(path .. ": expected " .. expected .. ", got " .. describe(value), 0)
end

local function scalar(name, expected, test)
    return {
        name = name,
        check = function(value, path)
            if not test(value) then
                fail(path, expected, value)
            end
            return value
        end,
    }
end

types.any = {
    name = "any",
    merge = merge_any,
    check = function(value)
        return value
    end,
}
types.string = scalar("string", "a string", function(v)
    return type(v) == "string"
end)
types.integer = scalar("integer", "an integer", function(v)
    return math.type(v) == "integer"
end)
types.number = scalar("number", "a number", function(v)
    return type(v) == "number"
end)
types.boolean = scalar("boolean", "a boolean", function(v)
    return type(v) == "boolean"
end)
types.func = scalar("function", "a function", function(v)
    return type(v) == "function"
end)

function types.custom(name, test)
    return scalar(name, name, test)
end

function types.enum(choices)
    local names = lib.map(choices, function(choice)
        return string.format("%q", choice)
    end)
    local expected = "one of " .. table.concat(names, ", ")
    return scalar("enum", expected, function(v)
        return lib.contains(choices, v)
    end)
end

function types.list(item)
    return {
        name = "list of " .. item.name,
        merge = merge_lists,
        check = function(value, path)
            if type(value) ~= "table" or (next(value) ~= nil and not is_list(value)) then
                fail(path, "a list of " .. item.name, value)
            end
            local out = {}
            for i, v in ipairs(value) do
                out[i] = item.check(v, child(path, i))
            end
            return out
        end,
    }
end

function types.map(item)
    local name = type(item) == "function" and "map" or "map of " .. item.name
    return {
        name = name,
        merge = function(defs, path)
            return merge_map(item, defs, path)
        end,
        check = function(value, path)
            if type(value) ~= "table" or is_list(value) then
                fail(path, "a " .. name, value)
            end
            local out = {}
            for k, v in pairs(value) do
                if type(k) ~= "string" then
                    error(child(path, k) .. ": map keys must be strings", 0)
                end
                out[k] = item_type(item, k).check(v, child(path, k))
            end
            return out
        end,
    }
end

function types.optional(item)
    return {
        name = "optional " .. item.name,
        optional = true,
        merge = item.merge,
        check = function(value, path)
            if value == nil then
                return nil
            end
            return item.check(value, path)
        end,
    }
end

function types.default(item, fallback)
    return {
        name = item.name,
        optional = true,
        merge = item.merge,
        check = function(value, path)
            if value == nil then
                return item.check(fallback, path)
            end
            return item.check(value, path)
        end,
    }
end

function types.one_of(...)
    local alternatives = { ... }
    local names = lib.map(alternatives, function(t)
        return t.name
    end)
    local expected = table.concat(names, " or ")
    return {
        name = expected,
        check = function(value, path)
            for _, alternative in ipairs(alternatives) do
                local ok, result = pcall(alternative.check, value, path)
                if ok then
                    return result
                end
            end
            fail(path, expected, value)
        end,
    }
end

function types.record(fields, options)
    local open = options ~= nil and options.open == true
    return {
        name = "record",
        merge = function(defs, path)
            local out = {}
            for key, list in pairs(by_key(defs, path)) do
                local field = fields[key]
                if field == nil and not open then
                    error(child(path, key) .. ": unknown field, defined in " .. sources(list), 0)
                end
                out[key] = merge_with(field or types.any, list, child(path, key))
            end
            return out
        end,
        check = function(value, path)
            if type(value) ~= "table" or is_list(value) then
                fail(path, "a record", value)
            end
            local out = {}
            for name, field in pairs(fields) do
                local v = value[name]
                if v == nil and not field.optional then
                    error(child(path, name) .. ": missing required " .. field.name, 0)
                end
                out[name] = field.check(v, child(path, name))
            end
            for k, v in pairs(value) do
                if fields[k] == nil then
                    if not open then
                        error(child(path, k) .. ": unknown field", 0)
                    end
                    out[k] = v
                end
            end
            return out
        end,
    }
end

function lib.check(value, schema, name)
    return schema.check(value, name or "value")
end

lib.types = types

-- Modules, as in NixOS: each declares options and defines values for them, and the definitions
-- are merged by type and priority. A module is a table of `imports`, `options` and `config`, or a
-- function of the evaluation's arguments returning one; a table with neither `options` nor
-- `config` is all config. `config` may be a function of the final configuration: those functions
-- run again until it stops changing, and a last strict round reports errors against it.

local modules = {}
local MODULE_KEYS = { imports = true, options = true, config = true, _name = true }
local ARGUMENTS = {
    __index = function(_, key)
        if key == "config" then
            error("module functions run before the configuration exists; read it from "
                .. "`config = function(config) ... end`", 2)
        end
    end,
}

-- `default_from`, a function of the configuration, computes a default from other options.
function lib.option(spec)
    spec = spec or {}
    return setmetatable({
        type = spec.type or types.any,
        default = spec.default,
        default_from = spec.default_from,
        description = spec.description,
        example = spec.example,
    }, OPTION)
end

function lib.is_option(value)
    return getmetatable(value) == OPTION
end

function lib.enable_option(description)
    return lib.option{
        type = types.boolean,
        default = false,
        description = "Whether to enable " .. description .. ".",
    }
end

-- A module is named by the file that returned it, or by its place in the list.
local function module_name(module, parent, index, site)
    local imported = native.imported(module)
    if imported ~= nil then
        return imported
    elseif parent == nil then
        return "module " .. index .. (site and " at " .. site or "")
    end
    return "import " .. index .. " of " .. parent
end

local function module_parts(value, from)
    if type(value) ~= "table" or is_list(value) then
        error(from .. ": a module is a table of imports, options and config, got "
            .. describe(value), 0)
    end
    from = value._name or from
    local config = value.config
    if value.options == nil and config == nil then
        config = {}
        for key, item in pairs(value) do
            if key ~= "imports" and key ~= "_name" then
                config[key] = item
            end
        end
    else
        for key in pairs(value) do
            if not MODULE_KEYS[key] then
                error(from .. ": unexpected key " .. describe(key) .. " next to options and config", 0)
            end
        end
    end
    local imports = value.imports or {}
    if shape(imports) ~= "list" and shape(imports) ~= "empty" then
        error(from .. ": imports must be a list of modules", 0)
    end
    return { from = from, imports = imports, options = value.options or {}, config = config or {} }
end

local function collect(list, args, out, seen, parent, site)
    for index, module in ipairs(list) do
        if type(module) == "string" then
            error((parent or "modules") .. ": modules are values; write import(\"" .. module
                .. "\") instead of the name", 0)
        end
        if not seen[module] then
            seen[module] = true
            local from = module_name(module, parent, index, site)
            local value = module
            if type(module) == "function" then
                value = module(args)
            end
            local parts = module_parts(value, from)
            out[#out + 1] = parts
            collect(parts.imports, args, out, seen, parts.from, site)
        end
    end
    return out
end

local function declare(tree, options, path, from, declared)
    if shape(options) ~= "map" and shape(options) ~= "empty" then
        error(from .. ": options must be a table of options, got " .. describe(options), 0)
    end
    for key, value in pairs(options) do
        local name = child(path, key)
        local existing = tree[key]
        if getmetatable(value) == OPTION then
            if existing ~= nil then
                error(name .. ": declared in " .. declared[name] .. " and in " .. from, 0)
            end
            tree[key] = value
            declared[name] = from
        elseif shape(value) == "map" or shape(value) == "empty" then
            if getmetatable(existing) == OPTION then
                error(name .. ": declared as an option in " .. declared[name]
                    .. " and as a set of options in " .. from, 0)
            end
            if existing == nil then
                existing = {}
                tree[key] = existing
                declared[name] = from
            end
            declare(existing, value, name, from, declared)
        else
            error(name .. ": expected an option or a set of options in " .. from .. ", got "
                .. describe(value), 0)
        end
    end
end

local function option_value(option, defs, path, config)
    local default = option.default
    if option.default_from ~= nil then
        default = option.default_from(config)
    end
    if default ~= nil then
        defs = lib.concat(defs, { definition(default, OPTION_DEFAULT, ORDER, "the default") })
    end
    local winners = winning(defs)
    if #winners == 0 then
        if option.type.optional then
            return option.type.check(nil, path)
        end
        error(path .. ": no value is defined and the option has no default", 0)
    end
    local merged = (option.type.merge or merge_equal)(winners, path)
    local ok, value = pcall(option.type.check, merged, path)
    if not ok then
        error(tostring(value) .. "\n  defined in " .. sources(winners), 0)
    end
    return value
end

-- The configuration for an option tree. Outside strict rounds an option that fails is left unset.
local function settle(node, defs, path, strict, config)
    if getmetatable(node) == OPTION then
        if strict then
            return option_value(node, defs, path, config)
        end
        local ok, value = pcall(option_value, node, defs, path, config)
        if ok then
            return value
        end
        return nil
    end
    local keyed = {}
    for _, def in ipairs(expand(defs)) do
        local kind = shape(def.value)
        if kind == "map" or kind == "empty" then
            for key, value in pairs(def.value) do
                if node[key] ~= nil then
                    local list = keyed[key] or {}
                    keyed[key] = list
                    list[#list + 1] = definition(value, def.priority, def.order, def.from)
                elseif strict then
                    error(child(path, key) .. ": no such option, defined in " .. def.from, 0)
                end
            end
        elseif strict then
            error(path .. ": expected a set of options, got " .. describe(def.value) .. " in "
                .. def.from, 0)
        end
    end
    local out = {}
    for key, sub in pairs(node) do
        out[key] = settle(sub, keyed[key] or {}, child(path, key), strict, config)
    end
    return out
end

-- The first path where two configurations differ. Closures compare by code and captures.
local function difference(a, b, path, seen)
    if a == b or (a ~= a and b ~= b) then
        return nil
    end
    if type(a) == "function" and type(b) == "function" and native.same_code(a, b) then
        return difference(native.captures(a), native.captures(b), path, seen)
    end
    if type(a) ~= "table" or type(b) ~= "table" then
        return path
    end
    local pairs_seen = seen[a] or {}
    if pairs_seen[b] then
        return nil
    end
    pairs_seen[b] = true
    seen[a] = pairs_seen
    for key, value in pairs(a) do
        local found = difference(value, b[key], child(path, key), seen)
        if found ~= nil then
            return found
        end
    end
    for key in pairs(b) do
        if a[key] == nil then
            return child(path, key)
        end
    end
    return nil
end

local function computes(node)
    if getmetatable(node) == OPTION then
        return node.default_from ~= nil
    end
    return lib.any(lib.values(node), computes)
end

local function prepare(spec)
    local args = setmetatable(lib.merge(spec.args or {}, { lib = lib }), ARGUMENTS)
    local list = collect(spec.modules or {}, args, {}, {}, nil, native.caller())
    local tree, declared = {}, {}
    for _, module in ipairs(list) do
        declare(tree, module.options, "options", module.from, declared)
    end
    return args, list, tree, declared
end

-- Evaluate `spec.modules` (with `spec.args` passed to module functions and config functions) to
-- the merged configuration. `spec.rounds` bounds how often config functions run, 16 by default.
function modules.evaluate(spec)
    local args, list, tree = prepare(spec)
    local static, dynamic = {}, {}
    for _, module in ipairs(list) do
        if type(module.config) == "function" then
            dynamic[#dynamic + 1] = module
        else
            static[#static + 1] = definition(module.config, NORMAL, ORDER, module.from)
        end
    end
    if #dynamic == 0 and not computes(tree) then
        return settle(tree, static, "config", true, {})
    end
    local function round(config, strict)
        native.freeze(config)
        local defs = lib.concat(static)
        for _, module in ipairs(dynamic) do
            local ok, value = true, nil
            if strict then
                value = module.config(config, args)
            else
                ok, value = pcall(module.config, config, args)
            end
            if ok and value ~= nil then
                defs[#defs + 1] = definition(value, NORMAL, ORDER, module.from)
            end
        end
        return settle(tree, defs, "config", strict, config)
    end
    -- Two rounds agree when their results match, taking the configurations each was computed
    -- from as equal wherever a closure captured them.
    local function differs(new, old, from, before)
        local seen = {}
        if before ~= nil then
            seen[from] = { [before] = true }
        end
        return difference(new, old, "config", seen)
    end
    local limit = spec.rounds or 16
    local config = settle(tree, static, "config", false, {})
    local before = nil
    for _ = 1, limit do
        local next_config = round(config, false)
        if differs(next_config, config, config, before) == nil then
            break
        end
        config, before = next_config, config
    end
    local final = round(config, true)
    local changed = differs(final, config, config, before)
    if changed ~= nil then
        error("the configuration did not settle after " .. limit .. " rounds: " .. changed
            .. " keeps changing", 0)
    end
    return final
end

-- Every option the modules declare, by dotted name, with its type, default, description, example
-- and the module that declared it.
function modules.options(spec)
    local _, _, tree, declared = prepare(spec)
    local out = {}
    local function walk(node, name, path)
        for key, value in pairs(node) do
            local full = name == nil and tostring(key) or name .. "." .. tostring(key)
            local at = child(path, key)
            if getmetatable(value) == OPTION then
                out[full] = {
                    type = value.type.name,
                    default = value.default,
                    computed = value.default_from ~= nil or nil,
                    description = value.description,
                    example = value.example,
                    declared = declared[at],
                }
            else
                walk(value, full, at)
            end
        end
    end
    walk(tree, nil, "options")
    return out
end

lib.modules = modules

return lib
