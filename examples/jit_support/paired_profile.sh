#!/usr/bin/env bash
set -euo pipefail

[[ $# == 2 ]] || { echo 'Expected binary and new output directory' >&2; exit 2; }
[[ -f $1 && -x $1 ]] || { echo 'Missing benchmark executable' >&2; exit 2; }
binary=$(realpath -- "$1")
directory=$2
mkdir -p -- "$(dirname -- "$directory")"
mkdir -- "$directory"
sha256sum -- "$binary" > "$directory/binary.sha256"
nm --demangle --defined-only "$binary" > "$directory/symbols.log" 2>&1
grep -E ' [tT] jit_bench::report$' "$directory/symbols.log" > /dev/null || {
    echo 'Benchmark lacks the report boundary symbol' >&2
    exit 2
}
{ rustc -vV; cargo --version; uname -sm; valgrind --version; } > "$directory/environment.log"
printf 'context=full\nmode=paired\nsamples=11\ncollection=*Executor>::step\ndump_before=jit_bench::report\n' > "$directory/configuration"
status=0
valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no \
    --toggle-collect='*Executor>::step' --dump-before='jit_bench::report' \
    --cache-sim=yes --branch-sim=yes --dump-instr=yes \
    --callgrind-out-file="$directory/profile.callgrind" \
    "$binary" --mode paired --samples 11 > "$directory/run.log" 2>&1 || status=$?
sha256sum --check "$directory/binary.sha256" > "$directory/binary-after.log" || exit 4
(( status == 0 )) || exit "$status"
bash examples/jit_support/annotate_paired_profile.sh "$directory"
sha256sum --check "$directory/binary.sha256" >> "$directory/binary-after.log"
