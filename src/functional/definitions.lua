---@meta luna-functional
-- Type definitions for luna's functional states, for lua-language-server. `luna definitions`
-- prints this file; put it in a directory listed in `workspace.library` of `.luarc.json`.

---@class luna.Type
---@field name string
---@field optional? boolean
---@field check fun(value: any, path: string): any
---@field merge? fun(definitions: luna.Definition[], path: string): any

---@class luna.Definition
---@field value any
---@field priority integer
---@field order integer
---@field from string

---@class luna.OptionSpec
---@field type? luna.Type The option's type; `lib.types.any` by default.
---@field default? any The value when no module defines one.
---@field default_from? fun(config: table): any A default computed from the configuration.
---@field description? string
---@field example? any

---@class luna.Option: luna.OptionSpec

---@alias luna.ModuleValue luna.Module|table|fun(args: table): (luna.Module|table)

---@class luna.Module
---@field imports? luna.ModuleValue[] More modules, as values from `import`.
---@field options? table Option declarations made with `lib.option`, nested by name.
---@field config? table|fun(config: table, args: table): table? Definitions, or a function of the settled configuration returning them.
---@field _name? string The name errors use for this module.

---@class luna.ModulesSpec
---@field modules luna.ModuleValue[]
---@field args? table Passed to module functions and config functions, with `lib`.
---@field rounds? integer How often config functions may run before the configuration must settle; 16 by default.

---@class luna.OptionDoc
---@field type string
---@field default? any
---@field computed? boolean
---@field description? string
---@field example? any
---@field declared string

---The configuration's inputs, frozen. A root file returning a function is called with them.
---@type table
inputs = {}

---Evaluate `name`, relative to the root, once; return its frozen result. Names ending in `.json`
---or `.toml` are parsed as data.
---@param name string
---@return any
function import(name) end

---The bytes of the file `name`, relative to the root.
---@param name string
---@return string
function read_file(name) end

---The entries of the directory `name` (`"."` for the root), mapped to their kind.
---@param name string
---@return table<string, "file"|"directory"|"other">
function read_dir(name) end

---@class luna.lib
lib = {}

---@param value any
---@return boolean
function lib.is_list(value) end

---Nix's `//`: later maps win, key by key.
---@param ... table
---@return table
function lib.merge(...) end

---Nix's `recursiveUpdate`: maps merge recursively; lists are replaced.
---@param base table
---@param override table
---@return table
function lib.deep_merge(base, override) end

---@generic T, U
---@param list T[]
---@param f fun(value: T, index: integer): U
---@return U[]
function lib.map(list, f) end

---@generic T
---@param list T[]
---@param keep fun(value: T, index: integer): boolean
---@return T[]
function lib.filter(list, keep) end

---@generic T, A
---@param list T[]
---@param initial A
---@param f fun(accumulator: A, value: T): A
---@return A
function lib.fold(list, initial, f) end

---@param ... any[]
---@return any[]
function lib.concat(...) end

---@param lists any[][]
---@return any[]
function lib.flatten(lists) end

---@param first integer
---@param last integer
---@return integer[]
function lib.range(first, last) end

---@generic T
---@param list T[]
---@param test fun(value: T, index: integer): boolean
---@return T?, integer?
function lib.find(list, test) end

---@generic T
---@param list T[]
---@param test fun(value: T, index: integer): boolean
---@return boolean
function lib.any(list, test) end

---@generic T
---@param list T[]
---@param test fun(value: T, index: integer): boolean
---@return boolean
function lib.all(list, test) end

---@param list any[]
---@param value any
---@return boolean
function lib.contains(list, value) end

---@generic T
---@param list T[]
---@return T[]
function lib.unique(list) end

---@generic T
---@param list T[]
---@param less? fun(a: T, b: T): boolean
---@return T[]
function lib.sort(list, less) end

---@generic T
---@param list T[]
---@param key_of fun(value: T): any
---@return table<any, T[]>
function lib.group_by(list, key_of) end

---@param map table
---@return any[]
function lib.keys(map) end

---@param map table
---@return any[]
function lib.values(map) end

---@param map table
---@param f fun(value: any, key: any): any
---@return table
function lib.map_values(map, f) end

---@param map table
---@param keep fun(value: any, key: any): boolean
---@return table
function lib.filter_values(map, keep) end

---@param list { [1]: any, [2]: any }[]
---@return table
function lib.from_pairs(list) end

---@param text string
---@param separator string
---@return string[]
function lib.split(text, separator) end

---@param list string[]
---@param separator? string
---@return string
function lib.join(list, separator) end

---@param text string
---@param from string
---@param to string
---@return string
function lib.replace(text, from, to) end

---@param text string
---@param prefix string
---@return boolean
function lib.starts_with(text, prefix) end

---@param text string
---@param suffix string
---@return boolean
function lib.ends_with(text, suffix) end

---@param text string
---@param prefix string
---@return string
function lib.remove_prefix(text, prefix) end

---@param text string
---@param suffix string
---@return string
function lib.remove_suffix(text, suffix) end

---@param text string
---@return string
function lib.trim(text) end

---Remove the indentation non-blank lines share, and a blank first line, like Nix's `''`.
---@param text string
---@return string
function lib.dedent(text) end

---@param path string
---@return string
function lib.basename(path) end

---@param path string
---@return string
function lib.dirname(path) end

---A shell word for `text`.
---@param text string
---@return string
function lib.escape_shell(text) end

---Deep, cycle-safe structural equality.
---@param a any
---@param b any
---@return boolean
function lib.equal(a, b) end

---A mutable deep copy.
---@generic T
---@param value T
---@return T
function lib.copy(value) end

---Replace nodes by identity and rebuild only what contains them.
---@generic T
---@param tree T
---@param replace table|fun(node: table): table?
---@return T
function lib.rewrite(tree, replace) end

---@param value any
---@param options? { pretty?: boolean }
---@return string
function lib.to_json(value, options) end

---@param value table
---@return string
function lib.to_toml(value) end

---@param value any
---@return string
function lib.to_yaml(value) end

---@param value table
---@return string
function lib.to_ini(value) end

---@param text string
---@return any
function lib.from_json(text) end

---@param text string
---@return table
function lib.from_toml(text) end

---A definition with a priority; lower wins. Plain definitions have 100.
---@param priority integer
---@param value any
---@return any
function lib.mk_override(priority, value) end

---Priority 1000: loses to plain definitions, beats the option's default.
---@param value any
---@return any
function lib.mk_default(value) end

---Priority 50: beats every other definition.
---@param value any
---@return any
function lib.mk_force(value) end

---@param condition boolean
---@param value any
---@return any
function lib.mk_if(condition, value) end

---@param values any[]
---@return any
function lib.mk_merge(values) end

---A list definition with an order; lower comes first. Plain definitions have 1000.
---@param order integer
---@param value any[]
---@return any
function lib.mk_order(order, value) end

---@param value any[]
---@return any
function lib.mk_before(value) end

---@param value any[]
---@return any
function lib.mk_after(value) end

---Check `value` against `schema`, returning it with defaults filled in.
---@param value any
---@param schema luna.Type
---@param name? string
---@return any
function lib.check(value, schema, name) end

---@param spec? luna.OptionSpec
---@return luna.Option
function lib.option(spec) end

---@param value any
---@return boolean
function lib.is_option(value) end

---A boolean option defaulting to false.
---@param description string
---@return luna.Option
function lib.enable_option(description) end

---@class luna.types
---@field any luna.Type
---@field string luna.Type
---@field integer luna.Type
---@field number luna.Type
---@field boolean luna.Type
---@field func luna.Type
lib.types = {}

---@param name string
---@param test fun(value: any): boolean
---@return luna.Type
function lib.types.custom(name, test) end

---@param choices any[]
---@return luna.Type
function lib.types.enum(choices) end

---@param item luna.Type
---@return luna.Type
function lib.types.list(item) end

---A map with string keys; `item` may be a function from the key to the value's type.
---@param item luna.Type|fun(key: string): luna.Type
---@return luna.Type
function lib.types.map(item) end

---@param item luna.Type
---@return luna.Type
function lib.types.optional(item) end

---@param item luna.Type
---@param fallback any
---@return luna.Type
function lib.types.default(item, fallback) end

---@param ... luna.Type
---@return luna.Type
function lib.types.one_of(...) end

---@param fields table<string, luna.Type>
---@param options? { open?: boolean }
---@return luna.Type
function lib.types.record(fields, options) end

---@class luna.modules
lib.modules = {}

---Merge the modules' definitions into the configuration.
---@param spec luna.ModulesSpec
---@return table
function lib.modules.evaluate(spec) end

---Every declared option by dotted name.
---@param spec luna.ModulesSpec
---@return table<string, luna.OptionDoc>
function lib.modules.options(spec) end
