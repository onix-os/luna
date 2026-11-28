-- geto, a declarative binary manager: a port of geto's Home Manager module. The configuration
-- lists repositories; this writes geto's manifest and runs `geto ensure` on activation.

local types = lib.types

local function repo_name(repo)
    return lib.remove_suffix(lib.basename(lib.remove_suffix(repo, "/")), ".git")
end

local entry_fields = {
    repo = types.string,
    name = types.optional(types.string),
    tag = types.optional(types.string),
    tags = types.default(types.list(types.string), {}),
    path = types.optional(types.string),
    provider = types.optional(types.string),
    description = types.optional(types.string),
    patch = types.default(types.boolean, true),
}

-- In `binaries`, an entry's name defaults to its key.
local function binary(key)
    return types.record(lib.merge(entry_fields, { name = types.default(types.string, key) }))
end

local function under_home(name)
    return function(config)
        return config.home.directory .. name
    end
end

return {
    options = { programs = { geto = {
        enable = lib.enable_option("the geto declarative binary manager"),
        command = lib.option{
            type = types.string,
            default = "geto",
            description = "The geto command to run.",
        },
        install_dir = lib.option{
            type = types.string,
            default_from = under_home("/.local/bin"),
            description = "Directory where managed binaries are installed.",
        },
        config_file = lib.option{
            type = types.string,
            default_from = function(config)
                return config.xdg.config_home .. "/geto/list.json"
            end,
            description = "Where the generated manifest is written.",
        },
        state_file = lib.option{
            type = types.string,
            default_from = under_home("/.local/state/geto/config.state.json"),
            description = "geto's mutable state: versions, hashes and selected assets.",
        },
        add_to_path = lib.option{
            type = types.boolean,
            default = true,
            description = "Add the install directory to the session PATH.",
        },
        entries = lib.option{
            type = types.list(types.one_of(types.string, types.record(entry_fields))),
            default = {},
            description = "Repositories, or entries with a repository, to install.",
            example = { "github.com/rust-lang/mdBook", { repo = "github.com/git-town/git-town", tag = "essential" } },
        },
        binaries = lib.option{
            type = types.map(binary),
            default = {},
            description = "Entries keyed by their installed name.",
            example = { mdbook = { repo = "github.com/rust-lang/mdBook" } },
        },
    } } },

    config = function(config)
        local cfg = config.programs.geto
        local function normalize(entry)
            if type(entry) == "string" then
                entry = { repo = entry, tags = {}, patch = true }
            end
            local name = entry.name or repo_name(entry.repo)
            local tags = #entry.tags > 0 and entry.tags or { entry.tag or "default" }
            local path = entry.path or cfg.install_dir .. "/" .. name
            return {
                path = path,
                manifest = {
                    path = path,
                    tags = tags,
                    url = entry.repo,
                    provider = entry.provider,
                    description = entry.description,
                    patch = entry.patch,
                },
            }
        end
        local entries = lib.map(lib.concat(cfg.entries, lib.values(cfg.binaries)), normalize)
        local manifest = {
            default_path = cfg.install_dir,
            bins = lib.from_pairs(lib.map(entries, function(entry)
                return { entry.path, entry.manifest }
            end)),
        }
        local q = lib.escape_shell
        local ensure = string.format(lib.dedent([[
            mkdir -p %s %s
            GETO_CONFIG_FILE=%s GETO_STATE_FILE=%s GETO_DEFAULT_PATH=%s \
              GETO_NONINTERACTIVE=1 %s --tag all ensure]]),
            q(cfg.install_dir), q(lib.dirname(cfg.state_file)),
            q(cfg.config_file), q(cfg.state_file), q(cfg.install_dir), q(cfg.command))
        return lib.mk_if(cfg.enable, {
            home = {
                packages = { cfg.command },
                files = {
                    [lib.remove_prefix(cfg.config_file, config.home.directory .. "/")] =
                        lib.to_json(manifest, { pretty = true }) .. "\n",
                },
                activation = { geto = ensure },
                session_path = lib.mk_if(cfg.add_to_path, { cfg.install_dir }),
            },
        })
    end,
}
