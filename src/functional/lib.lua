-- The `lib` global of functional states: merging, list and map helpers, structural equality,
-- identity rewriting and schemas. Every function returns new tables and leaves its arguments
-- unchanged, so it works on frozen data.

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

-- Schemas: `lib.check(value, type)` returns the value with defaults filled in, or raises an error
-- naming the exact path and what was expected there.

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
    return {
        name = "map of " .. item.name,
        check = function(value, path)
            if type(value) ~= "table" or is_list(value) then
                fail(path, "a map of " .. item.name, value)
            end
            local out = {}
            for k, v in pairs(value) do
                if type(k) ~= "string" then
                    error(child(path, k) .. ": map keys must be strings", 0)
                end
                out[k] = item.check(v, child(path, k))
            end
            return out
        end,
    }
end

function types.optional(item)
    return {
        name = "optional " .. item.name,
        optional = true,
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

return lib
