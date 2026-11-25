#!/usr/bin/env bash
set -euo pipefail
fail() { printf 'JIT reproducible build: %s\n' "$*" >&2; exit 2; }
[[ $# == 7 ]] || fail 'expected cargo, source, target, artifacts, opt, triple, wrapper'
cargo=$1
source_root=$(cd "$2" && pwd -P)
target_root=$3
artifacts=$4
opt=$5
triple=$6
wrapper=$7
[[ $opt == 3 || $opt == s ]] || fail 'opt must be 3 or s'
[[ -z ${RUSTC_WRAPPER:-} && -z ${RUSTC_WORKSPACE_WRAPPER:-} ]] || fail 'unset existing compiler wrappers'
[[ -x $wrapper ]] || fail 'remapping wrapper is not executable'
for path in "$source_root" "$target_root" "$artifacts"; do
    [[ $path == /* && $path != *'='* && $path != *$'\n'* ]] || fail 'absolute unambiguous paths required'
done
[[ $artifacts != "$source_root" && $target_root != "$source_root" && $target_root != "$artifacts" ]] || fail 'source, target and artifacts must be separate'
expected=$(mktemp)
locks=()
cleanup() { rm -f "$expected"; for lock in "${locks[@]}"; do rmdir "$lock" || :; done; }
trap cleanup EXIT
wrapper_hash=$(sha256sum "$wrapper")
builder_hash=$(sha256sum "$0")
printf 'protocol=luna-jit-repro-v1\nsource=%s\nwrapper_sha256=%s\nbuilder_sha256=%s\nopt=%s\ntriple=%s\nRUSTFLAGS=%s\nCARGO_ENCODED_RUSTFLAGS=%s\n' \
    "$source_root" "${wrapper_hash%% *}" "${builder_hash%% *}" "$opt" "$triple" "${RUSTFLAGS:-}" "${CARGO_ENCODED_RUSTFLAGS:-}" > "$expected"
for directory in "$target_root" "$artifacts"; do
    mkdir -p "$directory"
    mkdir "$directory/.building" || fail 'cache or artifacts are locked; use fresh directories if an interrupted build left a lock'
    locks+=("$directory/.building")
done
for directory in "$target_root" "$artifacts"; do
    if [[ -e $directory/.luna-jit-repro ]]; then
        cmp -s "$expected" "$directory/.luna-jit-repro" || fail 'incompatible cache or artifacts; select fresh directories'
    else
        [[ -z $(find "$directory" -mindepth 1 -maxdepth 1 ! -name .building -print -quit) ]] || fail 'unmarked directory is not empty'
        cp "$expected" "$directory/.luna-jit-repro"
    fi
done
export RUSTC_WRAPPER=$wrapper LUNA_REPRO_SOURCE=$source_root LUNA_REPRO_TARGET=$target_root
export CARGO_TARGET_DIR=$target_root
export CARGO_PROFILE_RELEASE_OPT_LEVEL=$opt CARGO_PROFILE_RELEASE_LTO=true
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=true
target_args=()
release=$target_root/release/examples
if [[ -n $triple ]]; then
    target_args=(--target "$triple")
    release=$target_root/$triple/release/examples
fi
cd "$source_root"
rm -f "$artifacts/images.sha256" "$artifacts/binary-sha256.log" "$artifacts/completed"
{ find src -type f -print0; printf '%s\0' Cargo.toml Cargo.lock examples/jit_bench.rs examples/jit_feature_cost.rs examples/jit_support/workloads.rs; } |
    sort -z | xargs -0 sha256sum > "$artifacts/source.sha256"
LUNA_BENCH_OPT_LEVEL=$opt "$cargo" build --locked --release --features jit --example jit_bench "${target_args[@]}"
cp "$release/jit_bench" "$artifacts/native-bench"
"$cargo" build --locked --release --no-default-features --example jit_feature_cost "${target_args[@]}"
cp "$release/jit_feature_cost" "$artifacts/no-jit"
"$cargo" build --locked --release --no-default-features --features jit --example jit_feature_cost "${target_args[@]}"
cp "$release/jit_feature_cost" "$artifacts/jit-off"
cp "$expected" "$artifacts/configuration"
{ rustc -vV; "$cargo" --version; } > "$artifacts/toolchain"
sha256sum -c "$artifacts/source.sha256" > "$artifacts/source-verification.log"
sha256sum "$artifacts/"{native-bench,no-jit,jit-off} > "$artifacts/images.sha256"
sha256sum "$artifacts/"{no-jit,jit-off} > "$artifacts/binary-sha256.log"
printf 'complete\n' > "$artifacts/completed"
