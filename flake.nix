{
  description = "oslo Rust development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?rev=4c1018dae018162ec878d42fec712642d214fdfa";
    flake-utils.url = "github:numtide/flake-utils";
    nixgl.url = "github:nix-community/nixGL";
    # Rust with extra targets. nixpkgs' plain `rustc` ships only the host's std, so a
    # `--target x86_64-unknown-linux-musl` build fails on `cfg-if` with "can't find crate for
    # core" — nothing to do with the code. The static musl binary is what oslo releases, so the
    # target has to be buildable in the dev shell, not only in CI.
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    { nixpkgs, flake-utils, nixgl, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [
          rust-overlay.overlays.default
          (final: prev: {
            xorg = prev.xorg // {
              libX11 = final.libx11;
              libxcb = final.libxcb;
              libxshmfence = final.libxshmfence;
            };
          })
        ];

        pkgs = import nixpkgs {
          inherit system overlays;
          config = {
            allowUnfree = true;
            nvidia.acceptLicense = true;
          };
        };

        nvidiaVersion = builtins.getEnv "NVIDIA_VERSION";
        hasNvidia = nvidiaVersion != "";

        nixglPkgs = import "${nixgl}/default.nix" ({
          inherit pkgs;
        } // pkgs.lib.optionalAttrs hasNvidia {
          inherit nvidiaVersion;
          nvidiaHash = null;
        });

        nixGLTarget =
          if hasNvidia
          then "${nixglPkgs.nixGLNvidia}/bin/nixGLNvidia-${nvidiaVersion}"
          else "${nixglPkgs.nixGLIntel}/bin/nixGLIntel";
        nixVulkanTarget =
          if hasNvidia
          then "${nixglPkgs.nixVulkanNvidia}/bin/nixVulkanNvidia-${nvidiaVersion}"
          else "${nixglPkgs.nixVulkanIntel}/bin/nixVulkanIntel";

        nixGLAlias = pkgs.runCommand "nixGL" { } ''
          mkdir -p $out/bin
          ln -s ${nixGLTarget} $out/bin/nixGL
        '';
        nixVulkanAlias = pkgs.runCommand "nixVulkan" { } ''
          mkdir -p $out/bin
          ln -s ${nixVulkanTarget} $out/bin/nixVulkan
        '';

        guiLibs = with pkgs; [
          alsa-lib
          udev
          vulkan-loader
          libxkbcommon
          wayland
          libx11
          libxcursor
          libxi
          libxrandr
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            # One toolchain, with the musl target, replacing the separate rustc/cargo/rustfmt/
            # clippy derivations — mixing a targeted rustc with an untargeted cargo is how you get
            # a std that exists but cannot be found.
            (pkgs.rust-bin.stable.latest.default.override {
              targets = [ "x86_64-unknown-linux-musl" ];
              extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
            })
            pkgs.git-cliff
            pkgs.clang
            pkgs.mold
            pkgs.pkg-config

            # Working out where the binary and the source went. Each answers a different question,
            # and the first two answer it about *different* things — which is the reason to have
            # both rather than pick one.
            #
            #   cargo-bloat     which functions and which crates the machine code came from.
            #                   Needs symbols, so it wants a build with `strip` off.
            #   bloaty          the same question asked of a finished ELF, section by section,
            #                   and the only one of these that can diff two binaries.
            #   cargo-machete   dependencies declared and never named in the source. Fast, and
            #                   occasionally wrong about a crate reached only through a macro.
            #   cargo-udeps     the same, decided by the compiler rather than by grep, so it is
            #                   right where machete guesses — at the cost of a nightly rebuild.
            #   tokei           lines by language and by directory, for the half of this that is
            #                   about source rather than about the binary.
            pkgs.cargo-bloat
            pkgs.bloaty
            pkgs.cargo-machete
            pkgs.cargo-udeps
            pkgs.tokei

            nixGLAlias
            nixVulkanAlias
            nixglPkgs.nixGLIntel
            nixglPkgs.nixVulkanIntel
          ] ++ pkgs.lib.optionals hasNvidia [
            nixglPkgs.nixGLNvidia
            nixglPkgs.nixVulkanNvidia
          ] ++ guiLibs;

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath guiLibs;
          WGPU_VALIDATION = "0";
          WGPU_DEBUG = "0";
        };
      }
    );
}
