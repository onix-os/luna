#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'JIT reproducible compiler: %s\n' "$*" >&2; exit 2; }
[[ $# -ge 1 ]] || fail 'compiler argument required'
source_root=${LUNA_REPRO_SOURCE:-}
target_root=${LUNA_REPRO_TARGET:-}
for root in "$source_root" "$target_root"; do
    case "$root" in /*) ;; *) fail 'absolute source and target prefixes required';; esac
    [[ $root != *'='* && $root != *$'\n'* ]] || fail 'unsupported prefix character'
done
compiler=$1
shift
exec "$compiler" "$@" \
    "--remap-path-prefix=$source_root=luna-source" \
    "--remap-path-prefix=$target_root=luna-build"
