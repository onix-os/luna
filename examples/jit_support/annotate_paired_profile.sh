#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 ]] || { echo 'Expected a paired profile directory' >&2; exit 2; }
directory=$1
bash examples/jit_support/verify_paired_profile.sh "$directory" > "$directory/verification.log"
sha256sum "$directory"/profile.callgrind* > "$directory/raw.sha256"
for part in 1 3 5 7 9 11 13 15 17; do
    awk -f examples/jit_support/callgrind_exclusive.awk "$directory/profile.callgrind.$part" > "$directory/exclusive-$part.callgrind" 2> "$directory/exclusive-$part.log"
    callgrind_annotate --inclusive=no --threshold=99 "$directory/exclusive-$part.callgrind" > "$directory/annotation-$part.log"
done
sha256sum --check "$directory/raw.sha256" > "$directory/raw-after.log"
cat "$directory/verification.log"
