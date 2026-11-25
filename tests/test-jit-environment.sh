#!/usr/bin/env bash
set -euo pipefail

shell=$1
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir "$root/bin"
printf '#!%s\nprintf "mock tool version\\n"\n' "$shell" > "$root/bin/rustc"
cp "$root/bin/rustc" "$root/bin/cargo"
chmod +x "$root/bin/"{rustc,cargo}
export PATH="$root/bin:$PATH"

for context in unset paths special; do
    unset CARGO_TARGET_DIR out NIX_LDFLAGS NIX_LDFLAGS_FOR_TARGET
    case "$context" in
        paths)
            export CARGO_TARGET_DIR="$root/target directory" out="$root/output directory"
            export NIX_LDFLAGS="-rpath $out/lib -L/library" NIX_LDFLAGS_FOR_TARGET=-L/target
            ;;
        special)
            export out="$root/another output"
            export NIX_LDFLAGS=$'-rpath /path with spaces\n-lsymbol;$(not_a_command)'
            export NIX_LDFLAGS_FOR_TARGET=$'-Wl,quoted\\value\tother'
            ;;
    esac
    make --no-print-directory SHELL="$shell" CARGO="$root/bin/cargo" environment > "$root/$context.log"
    grep -Fx "Build directory: $(pwd -P)" "$root/$context.log"
    grep -Fx "Cargo target directory: ${CARGO_TARGET_DIR:-default}" "$root/$context.log"
    grep -Fx "Nix output: ${out:-unset}" "$root/$context.log"
    for variable in NIX_LDFLAGS NIX_LDFLAGS_FOR_TARGET; do
        printf -v expected '%s=%q' "$variable" "${!variable-}"
        grep -Fx "$expected" "$root/$context.log"
        test "$(grep -c "^$variable=" "$root/$context.log")" = 1
    done
done
mkdir "$root/project"
cp Makefile Cargo.toml "$root/project/"
(
    cd "$root/project"
    make --no-print-directory SHELL="$shell" CARGO="$root/bin/cargo" jit-bench-build
)
report="$root/project/target/jit-evidence/bench-environment.log"
grep -Fx "Build directory: $root/project" "$report"
grep -Fx "Cargo target directory: $root/project/target" "$report"
printf -v expected 'NIX_LDFLAGS=%q' "$NIX_LDFLAGS"
grep -Fx "$expected" "$report"
printf 'build environment: three contexts and native-build capture verified\n'
