-- luna's directory environment. Loaded when you `cd` here, unloaded when you leave.
--
-- Inert until `direnv allow`, and allowing it hashes the *contents* — so editing this file revokes
-- the allowance and you will be asked again.

-- The flake's dev shell, without entering one: the Rust toolchain with the musl target, plus the
-- size and dependency tools. The slow line here; everything below is instant.
oslo.direnv.nix_develop()

-- Built examples, ahead of anything installed, so `interpreter` is the REPL from this checkout
-- rather than one on the system. Idempotent, so a reload does not grow $PATH each time.
oslo.direnv.path_add("./target/debug/examples")
oslo.direnv.path_add("./target/release/examples")

-- Where the checkout is, for scripts that need to find their way back to the top.
oslo.env.set("TOP_HEAD", oslo.sys.pwd())

-- A token in the environment is a token in every child process. `nix` and `gh` both read this one,
-- and neither needs it for anything done in here.
oslo.env.unset("GITHUB_TOKEN")

-- The commands this repository is driven by. All unload with the directory, so they cannot fire
-- the wrong project's build.
oslo.env.set_alias("_b", "make build")
oslo.env.set_alias("_c", "make check")
oslo.env.set_alias("_r", "make repl")
oslo.env.set_alias("_t", "make test")
oslo.env.set_alias("_v", "make verify")
