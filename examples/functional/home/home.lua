-- A home configuration using the geto module:
--   luna build home.lua --input-string home="$HOME" --out "$HOME" --attr home.files

return lib.modules.evaluate{ modules = {
    import("base.lua"),
    import("geto.lua"),
    {
        home = { directory = inputs.home or "/home/me" },
        programs = { geto = {
            enable = true,
            entries = {
                "github.com/rust-lang/mdBook",
                { repo = "github.com/git-town/git-town", tag = "essential" },
            },
            binaries = {
                atuin = { repo = "github.com/atuinsh/atuin", tags = { "shell", "essential" } },
            },
        } },
    },
} }
