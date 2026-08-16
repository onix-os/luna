-- oslo's own directory environment. Loaded when you `cd` here, unloaded when you leave.
--
-- Inert until `direnv allow`, and allowing it hashes the *contents* — so editing this file revokes
-- the allowance and you will be asked again. That is the point: a file that runs code when you walk
-- into a directory must not be able to change under an allowance you already gave.

-- The flake's dev shell, without entering one. This is direnv's `use flake`, reading
-- `nix print-dev-env --json` rather than eval-ing a hundred kilobytes of generated bash.
--
-- It is the slow line here: nix takes a moment even when the profile is already built. Everything
-- below is instant.
oslo.direnv.nix_develop()

-- The binaries this repository builds, ahead of anything installed. Prepended, so a `oslo` you just
-- compiled wins over one on the system — which is the whole reason to put it here, and the thing
-- that has bitten this project before: testing `target/release/oslo` while running the debug build.
--
-- Idempotent, so a reload does not grow $PATH each time.
oslo.direnv.path_add("./target/debug")
oslo.direnv.path_add("./bin")
oslo.direnv.path_add("./target/x86_64-unknown-linux-musl/release")

-- Where the checkout is, for scripts that need to find their way back to the top.
oslo.env.set("TOP_HEAD", oslo.sys.pwd())

-- A token in the environment is a token in every child process. `nix` and `gh` both read this one,
-- and neither needs it for anything done in here.
oslo.env.unset("GITHUB_TOKEN")

-- The four commands this repository is actually driven by. Aliases rather than keybindings on
-- purpose: you type `_t`, you see `_t`, and `direnv status` lists it — where a key that means
-- something different depending on which directory you are standing in tells you nothing at all.
--
-- All of these unload with the directory, so they cannot fire the wrong project's build.
oslo.env.set_alias("_b", "make build")
oslo.env.set_alias("_c", "make check")
oslo.env.set_alias("_r", "make run")
oslo.env.set_alias("_t", "make test")
oslo.env.set_alias("_v", "make verify")
-- `_b` builds with every optional feature; this one builds with none of them. Worth having beside
-- it rather than as a flag to remember, because the two answer different questions — `_b` is the
-- shell as it is meant to be used, `_m` is the floor a distribution would ship as /bin/sh.
oslo.env.set_alias("_m", "make build TYPE=minimal")
