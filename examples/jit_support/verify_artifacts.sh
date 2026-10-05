#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf 'JIT artifact verification failed: %s\n' "$*" >&2
    exit 4
}

[[ $# == 1 ]] || fail 'expected a cost artifact directory'
directory=$1
manifest="$directory/binary-sha256.log"
[[ -f $manifest ]] || fail "missing manifest: $manifest"
declare -A hashes
while read -r digest path; do
    [[ $digest =~ ^[0-9a-f]{64}$ ]] || fail 'invalid manifest digest'
    name=${path##*/}
    case $name in no-jit|jit-off) ;; *) fail "unexpected artifact: $name" ;; esac
    [[ ! -v hashes[$name] ]] || fail "duplicate artifact: $name"
    hashes[$name]=$digest
done < "$manifest"

for name in no-jit jit-off; do
    [[ -v hashes[$name] ]] || fail "missing artifact: $name"
    [[ -f $directory/$name ]] || fail "missing binary: $directory/$name"
    actual=$(sha256sum -- "$directory/$name")
    [[ ${actual%% *} == "${hashes[$name]}" ]] || fail "hash mismatch: $directory/$name"
done
printf 'JIT cost artifact hashes verified\n'
