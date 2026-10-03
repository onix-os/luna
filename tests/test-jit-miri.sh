#!/usr/bin/env bash
set -euo pipefail

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
cat > "$root/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*"
if [[ -n ${FAIL_FILTER:-} && " $* " == *" $FAIL_FILTER "* ]]; then
    echo 'mock Miri failure' >&2
    exit 17
fi
SH
chmod +x "$root/cargo"

run_recipe() {
    make --no-print-directory jit-miri CARGO="$root/cargo" \
        MIRI_DIR="$root/$1" > "$root/$1.log" 2>&1
}

run_recipe success
logs=("$root/success/"*.log)
test "${#logs[@]}" -eq 44
for log in "${logs[@]}"; do
    [[ ${log##*/} =~ ^[a-z0-9_-]+\.log$ ]]
done
grep -Fq 'jit::resources::tests -- --test-threads=1 --skip jit::resources::tests::installation_refusal_reclaims_generated_mappings' \
    "$root/success/jit-resources-tests.log"
grep -Fq 'finalizers::tests -- --test-threads=1' "$root/success/finalizers-tests.log"
grep -Fq 'lua::memory_tests -- --test-threads=1' "$root/success/lua-memory_tests.log"

for filter in setup jit::resources::tests jit::handoff::tests; do
    export FAIL_FILTER="$filter"
    dir="failure-${filter//::/-}"
    if run_recipe "$dir"; then
        echo "Recipe ignored failure for $filter" >&2
        exit 1
    fi
    grep -Fq 'mock Miri failure' "$root/$dir.log"
    test ! -e "$root/$dir/lua-memory_tests.log"
done
echo 'Miri recipe checks passed: 44 portable logs; 3 failures propagated'
