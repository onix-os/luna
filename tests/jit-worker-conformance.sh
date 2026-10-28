#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
target=${1:-}
evidence=${2:-target/jit-evidence/compiler-worker/host}
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
"${cargo[@]}" build --locked -p luna --features jit-worker-bootstrap --bin luna-jit-worker --release "${args[@]}" --message-format=json >"$run/build.jsonl"
binary=$(jq -ers --arg manifest "$root/Cargo.toml" '
    [.[] | select(.reason == "compiler-artifact" and .manifest_path == $manifest
        and .target.name == "luna-jit-worker" and .target.kind == ["bin"]
        and .profile.test == false and .executable != null)]
    | if length == 1 then .[0].executable else error("expected one source-bound non-test helper") end
' "$run/build.jsonl")
cp --reflink=auto "$binary" "$run/luna-jit-worker"
chmod a-w "$run/luna-jit-worker"
sha256sum "$run/luna-jit-worker" >"$run/image.sha256"
LUNA_JIT_WORKER_IMAGE="$run/luna-jit-worker" make --no-print-directory \
    CARGO="${CARGO:-cargo}" TARGET="$target" \
    jit-compiler-process jit-compiler-artifact-parent jit-compiler-code-owner jit-compiler-artifact \
    2>&1 | tee "$run/conformance.log"
sha256sum -c "$run/image.sha256" "$run/source.sha256" >"$run/verified-after.log"
printf 'Compiler producer evidence: %s\n' "$run"
