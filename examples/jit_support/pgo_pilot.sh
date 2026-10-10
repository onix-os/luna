#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 && -n $1 ]] || { echo 'Set PGO_DIR to a new evidence directory' >&2; exit 2; }
[[ -z ${RUSTFLAGS:-} && -z ${CARGO_ENCODED_RUSTFLAGS:-} ]] || {
    echo 'PGO pilot requires unset Rust flags' >&2; exit 2;
}
host=$(rustc -vV | sed -n 's/^host: //p')
[[ $host == x86_64-unknown-linux-gnu ]] || { echo 'Pilot requires GNU x86_64' >&2; exit 2; }
profdata="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata"
[[ -x $profdata ]] || { echo 'Use nix develop .#pgo' >&2; exit 2; }
out=$(realpath -m "$1")
[[ $out != *[[:space:]]* ]] || { echo 'PGO directory cannot contain whitespace' >&2; exit 2; }
mkdir -p "$(dirname "$out")"
mkdir "$out"
trap 'status=$?; printf "%s\n" "$status" > "$out/exit"' EXIT
mkdir "$out/raw"
git rev-parse HEAD > "$out/revision"
git diff HEAD > "$out/source.patch"
git ls-files -z --cached --others --exclude-standard | sort -zu | xargs -0 sha256sum > "$out/source.sha256"
{ rustc -vV; cargo --version; "$profdata" --version; uname -sm; } > "$out/environment.log"
export CARGO_PROFILE_RELEASE_OPT_LEVEL=3
export CARGO_PROFILE_RELEASE_LTO=true
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_STRIP=true
export LUNA_BENCH_OPT_LEVEL=3
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
printf 'target=%s\nfeatures=jit,async\nopt_level=3\nlto=true\ncodegen_units=1\nstrip=true\njobs=%s\ntraining=scripts,callback,metamethods,jit_native,jit_regions\nmodes=off,auto,force\nbenchmark_training=false\n' "$host" "$CARGO_BUILD_JOBS" > "$out/configuration.log"
args=(--locked --release --target "$host" --features jit,async)
tests=(--test scripts --test callback --test metamethods --test jit_native --test jit_regions)
export CARGO_TARGET_DIR="$out/build"
export RUSTFLAGS="-Cprofile-generate=$out/raw"
export LLVM_PROFILE_FILE="$out/raw/%m-%p.profraw"
for mode in off auto force; do
    echo "PGO training: $mode"
    LUNA_TEST_JIT_MODE=$mode cargo test "${args[@]}" "${tests[@]}" -- --test-threads=1 > "$out/train-$mode.log" 2>&1
done
raw=("$out/raw/"*.profraw)
[[ -f ${raw[0]} ]]
sha256sum "${raw[@]}" > "$out/raw.sha256"
"$profdata" merge -o "$out/merged.profdata" "${raw[@]}" > "$out/merge.log" 2>&1
"$profdata" show "$out/merged.profdata" > "$out/profile-summary.log"
"$profdata" show --all-functions --counts "$out/merged.profdata" > "$out/profile-counts.log"
awk -f examples/jit_support/pgo_coverage.awk "$out/profile-counts.log" > "$out/coverage.log"
sha256sum "$out/merged.profdata" > "$out/profile.sha256"
unset LLVM_PROFILE_FILE
unset RUSTFLAGS
echo 'PGO uninstrumented control build'
cargo build "${args[@]}" --example jit_bench > "$out/control-build.log" 2>&1
cp "$CARGO_TARGET_DIR/$host/release/examples/jit_bench" "$out/control"
export RUSTFLAGS="-Cprofile-use=$out/merged.profdata -Cllvm-args=-pgo-warn-missing-function"
echo 'PGO candidate build'
cargo build "${args[@]}" --example jit_bench > "$out/use-build.log" 2>&1
cp "$CARGO_TARGET_DIR/$host/release/examples/jit_bench" "$out/candidate"
for mode in off auto force; do
    echo "PGO correctness: $mode"
    LUNA_TEST_JIT_MODE=$mode cargo test "${args[@]}" "${tests[@]}" -- --test-threads=1 > "$out/check-$mode.log" 2>&1
done
sha256sum "$out/control" "$out/candidate" > "$out/binaries.sha256"
sha256sum --check "$out/source.sha256" > "$out/source-after.log"
sha256sum --check "$out/raw.sha256" "$out/profile.sha256" > "$out/profile-after.log"
echo "PGO artifacts ready: $out (not performance acceptance)"
