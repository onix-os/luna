#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
parser="$root/examples/jit_support/pgo_coverage.awk"
cat > "$out/valid" <<'EOF'
  prefix_native_region8boundary:
    Hash: 0x1
    Counters: 2
    Block counts: [2, 3]
  prefix_Admitted13invoke_rooted:
    Hash: 0x2
    Counters: 1
    Block counts: [7]
  prefix_native_region7Session15complete_inline:
    Hash: 0x3
    Counters: 1
    Block counts: [11]
EOF
awk -f "$parser" "$out/valid" > "$out/result"
for expected in 'region_boundary blocks_total=5 records=1' 'rooted_call blocks_total=7 records=1' 'inline_batch blocks_total=11 records=1'; do
    grep -Fx "$expected" "$out/result"
done
reject() {
    local status=0
    awk -f "$parser" "$out/rejected" > "$out/stdout" 2> "$out/stderr" || status=$?
    [[ $status == 2 ]]
}
for symbol in native_region8boundary Admitted13invoke_rooted native_region7Session15complete_inline; do
    sed "s/$symbol/unrelated/" "$out/valid" > "$out/rejected"
    reject
done
for count in 0 '' '-1' '1, unknown' '1, -2'; do
    sed "s/\[11\]/[$count]/" "$out/valid" > "$out/rejected"
    reject
done
sed '/Block counts: \[11\]/i\  unrelated:' "$out/valid" > "$out/rejected"
reject
printf 'PGO coverage parser: valid totals and nine refusals passed\n'
