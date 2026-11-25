#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
target=${1:-}
evidence=${2:-target/jit-evidence/compiler-worker/host}
static=${3:-false}
case "$static" in
    false) ;;
    true)
        if [[ -z $target ]]; then target=$(rustc -vV | sed -n 's/^host: //p'); fi
        case "$target" in
            x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;;
            *) echo 'Static GNU producer requires a GNU native target' >&2; exit 2 ;;
        esac
        ;;
    *) echo 'WORKER_STATIC_GNU must be true or false' >&2; exit 2 ;;
esac
case "$target" in
    x86_64-unknown-linux-gnu|x86_64-unknown-linux-musl) arch=x86_64 ;;
    aarch64-unknown-linux-gnu) arch=aarch64 ;;
    '') arch=$(uname -m) ;;
    *) echo 'Producer conformance requires a declared native target' >&2; exit 2 ;;
esac
if [[ $(uname -s) != Linux || $(uname -m) != "$arch" || ! $arch =~ ^(x86_64|aarch64)$ ]]; then
    echo 'Producer conformance requires matching Linux hardware' >&2
    exit 2
fi
mkdir -p "$evidence"
run=$(mktemp -d "$evidence/run-XXXXXX")
run=$(cd "$run" && pwd)
{ git rev-parse HEAD; git status --porcelain; rustc -vV; printf 'target=%s\n' "$target"; } >"$run/environment.log"
{
    sha256sum Cargo.toml Cargo.lock Makefile derive/Cargo.toml util/Cargo.toml
    find src tests examples derive/src util/src -type f -print0 | sort -z | xargs -0 sha256sum
} >"$run/source.sha256"
read -r -a cargo <<<"${CARGO:-cargo}"
args=()
if [[ -n $target ]]; then args+=(--target "$target"); fi
build_env=()
if [[ $static == true ]]; then
    encoded=${CARGO_ENCODED_RUSTFLAGS:-}
    if [[ ! -v CARGO_ENCODED_RUSTFLAGS && -n ${RUSTFLAGS:-} ]]; then
        read -r -a flags <<<"$RUSTFLAGS"
        printf -v encoded '%s\x1f' "${flags[@]}"
        encoded=${encoded%$'\x1f'}
    fi
    flags=(-C target-feature=+crt-static)
    if [[ -n ${LUNA_WORKER_GLIBC_STATIC:-} ]]; then
        if [[ ! -f $LUNA_WORKER_GLIBC_STATIC/libc.a ]]; then
            echo 'Static GNU archive is missing' >&2
            exit 2
        fi
        flags+=(-L "native=$LUNA_WORKER_GLIBC_STATIC")
    fi
    for flag in "${flags[@]}"; do encoded="${encoded:+$encoded$'\x1f'}$flag"; done
    build_env=("CARGO_ENCODED_RUSTFLAGS=$encoded")
    printf 'static-gnu=true\nbuild-rustflags=%q\n' "$encoded" >>"$run/environment.log"
fi
env "${build_env[@]}" "${cargo[@]}" build --locked -p luna --features jit-worker-bootstrap --bin luna-jit-worker --release "${args[@]}" --message-format=json >"$run/build.jsonl"
binary=$(jq -ers --arg manifest "$root/Cargo.toml" '
    [.[] | select(.reason == "compiler-artifact" and .manifest_path == $manifest
        and .target.name == "luna-jit-worker" and .target.kind == ["bin"]
        and .profile.test == false and .executable != null)]
    | if length == 1 then .[0].executable else error("expected one source-bound non-test helper") end
' "$run/build.jsonl")
cp --reflink=auto "$binary" "$run/luna-jit-worker"
chmod a-w "$run/luna-jit-worker"
readelf -l "$run/luna-jit-worker" >"$run/program-headers.log"
readelf -d "$run/luna-jit-worker" >"$run/dynamic.log"
if [[ $static == true ]]; then
    if grep -q INTERP "$run/program-headers.log" || grep -q NEEDED "$run/dynamic.log"; then
        echo 'Static GNU producer has runtime ELF dependencies' >&2
        exit 1
    fi
fi
sha256sum "$run/luna-jit-worker" >"$run/image.sha256"
LUNA_JIT_WORKER_IMAGE="$run/luna-jit-worker" make --no-print-directory \
    CARGO="${CARGO:-cargo}" TARGET="$target" \
    jit-compiler-process jit-compiler-artifact-parent jit-compiler-code-owner jit-compiler-artifact \
    2>&1 | tee "$run/conformance.log"
sha256sum -c "$run/image.sha256" "$run/source.sha256" >"$run/verified-after.log"
printf 'Compiler producer evidence: %s\n' "$run"
