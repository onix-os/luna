#!/usr/bin/env bash
set -euo pipefail
shell=${1:?shell required}
repo=$(cd "$(dirname "$0")/.." && pwd -P)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export REPRO_CALLS=$tmp/calls
cat > "$tmp/cargo" <<'CARGO'
#!/usr/bin/env bash
set -eu
if [[ $1 == --version ]]; then printf 'cargo mock\n'; exit; fi
printf 'source=%s target=%s opt=%s lto=%s cgu=%s strip=%s wrapper=%s args=%s\n' "$LUNA_REPRO_SOURCE" "$LUNA_REPRO_TARGET" "$CARGO_PROFILE_RELEASE_OPT_LEVEL" "$CARGO_PROFILE_RELEASE_LTO" "$CARGO_PROFILE_RELEASE_CODEGEN_UNITS" "$CARGO_PROFILE_RELEASE_STRIP" "$RUSTC_WRAPPER" "$*" >> "$REPRO_CALLS"
[[ ${REPRO_FAIL:-0} == 0 ]] || exit 17
example= triple=
previous=
for arg in "$@"; do
    [[ $previous != --example ]] || example=$arg
    [[ $previous != --target ]] || triple=$arg
    previous=$arg
done
directory=$CARGO_TARGET_DIR/${triple:+$triple/}release/examples
mkdir -p "$directory"
printf '#!/usr/bin/env bash\nexit 0\n' > "$directory/$example"
chmod +x "$directory/$example"
CARGO
chmod +x "$tmp/cargo"
run() {
    make --no-print-directory -C "$repo" SHELL="$shell" jit-repro-images \
        CARGO="$tmp/cargo" JIT_REPRO_TARGET_DIR="$tmp/cache" JIT_REPRO_DIR="$tmp/images" "$@"
}
refuse() {
    local result=0 count
    count=$(wc -l < "$REPRO_CALLS")
    run "$@" > "$tmp/refused.log" 2>&1 || result=$?
    test "$result" = 2
    test "$(wc -l < "$REPRO_CALLS")" = "$count"
}
run > "$tmp/success.log" 2>&1
test "$(cat "$tmp/images/completed")" = complete
test "$(wc -l < "$REPRO_CALLS")" = 3
sha256sum -c "$tmp/images/images.sha256" > "$tmp/images-verified.log"
test ! -d "$tmp/images/.building"
grep -Fq 'opt=3 lto=true cgu=1 strip=true' "$REPRO_CALLS"
grep -Fq -- '--no-default-features --features jit --example jit_feature_cost' "$REPRO_CALLS"
run > "$tmp/repeat.log" 2>&1
test "$(wc -l < "$REPRO_CALLS")" = 6
refuse JIT_BENCH_OPT=bad
refuse JIT_BENCH_OPT=s
refuse TARGET=i686-unknown-linux-musl
RUSTC_WRAPPER=conflicting refuse
RUSTC_WORKSPACE_WRAPPER=conflicting refuse
mkdir "$tmp/unmarked"
printf 'preserve\n' > "$tmp/unmarked/user-data"
refuse JIT_REPRO_TARGET_DIR="$tmp/unmarked"
test "$(cat "$tmp/unmarked/user-data")" = preserve
mkdir "$tmp/images/.building"
refuse
rmdir "$tmp/images/.building"
cp "$repo/examples/jit_support/rustc_repro.sh" "$tmp/wrapper"
printf '\n' >> "$tmp/wrapper"
chmod +x "$tmp/wrapper"
refuse JIT_REPRO_WRAPPER="$tmp/wrapper"
result=0
REPRO_FAIL=1 run > "$tmp/compiler-failure.log" 2>&1 || result=$?
test "$result" = 2
grep -Fq 'Error 17' "$tmp/compiler-failure.log"
test ! -f "$tmp/images/completed"
test ! -f "$tmp/images/images.sha256"
test ! -f "$tmp/images/binary-sha256.log"
test ! -d "$tmp/images/.building"
make --no-print-directory -C "$repo" SHELL="$shell" jit-repro-images CARGO="$tmp/cargo" \
    JIT_BENCH_OPT=s TARGET=i686-unknown-linux-musl \
    JIT_REPRO_TARGET_DIR="$tmp/cross-cache" JIT_REPRO_DIR="$tmp/cross-images" > "$tmp/cross.log" 2>&1
test -x "$tmp/cross-cache/i686-unknown-linux-musl/release/examples/jit_bench"
test "$(cat "$tmp/cross-images/completed")" = complete
grep -Fq 'opt=s lto=true cgu=1 strip=true' "$REPRO_CALLS"
grep -Fq -- '--target i686-unknown-linux-musl' "$REPRO_CALLS"
printf 'jit_repro_build=passed incremental=1 refusals=8 compiler_failure=propagated target_forwarding=exact\n'
