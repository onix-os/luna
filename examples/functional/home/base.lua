-- The part of Home Manager other modules build on: the home directory, the files to write into
-- it, shell snippets to run afterwards, and directories to add to PATH. Build the files with
--   luna build home.lua --input-string home="$HOME" --out "$HOME" --attr home.files
-- and then run ~/.local/share/luna/activate.

local types = lib.types

return {
    options = {
        home = {
            directory = lib.option{ type = types.string, description = "The home directory." },
            packages = lib.option{
                type = types.list(types.string),
                default = {},
                description = "Commands the configuration expects to be installed.",
            },
            files = lib.option{
                type = types.map(types.one_of(
                    types.string,
                    types.record{ text = types.string, executable = types.default(types.boolean, false) }
                )),
                default = {},
                description = "Files to write, by path relative to the home directory.",
            },
            activation = lib.option{
                type = types.map(types.string),
                default = {},
                description = "Shell snippets the activation script runs, in name order.",
            },
            session_path = lib.option{
                type = types.list(types.string),
                default = {},
                description = "Directories added to PATH by ~/.config/luna/env.sh.",
            },
        },
        xdg = {
            config_home = lib.option{
                type = types.string,
                default_from = function(config)
                    return config.home.directory .. "/.config"
                end,
                description = "The XDG configuration directory.",
            },
        },
    },
    config = function(config)
        local home = config.home
        local steps = lib.map(lib.sort(lib.keys(home.activation)), function(name)
            return "# " .. name .. "\n" .. home.activation[name] .. "\n"
        end)
        local path = lib.join(lib.map(home.session_path, lib.escape_shell), ":")
        return {
            home = {
                files = lib.mk_merge({
                    {
                        [".local/share/luna/activate"] = {
                            text = "#!/bin/sh\nset -eu\n" .. lib.join(steps),
                            executable = true,
                        },
                    },
                    lib.mk_if(#home.session_path > 0, {
                        [".config/luna/env.sh"] = "export PATH=" .. path .. ':"$PATH"\n',
                    }),
                }),
            },
        }
    end,
}
