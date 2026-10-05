#!/usr/bin/env bash
set -euo pipefail

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
directory="$root/cost"
mkdir "$directory"
export ARTIFACT_INVOCATIONS="$root/invocations"
cat > "$root/worker" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$ARTIFACT_INVOCATIONS"
if [[ $1 == --compare ]]; then
    if [[ ${ARTIFACT_MUTATE:-0} == 1 ]]; then
        printf '\n' >> "$3"
    fi
    exit "${ARTIFACT_RESULT:-0}"
fi
if [[ ${ARTIFACT_PROOF_MUTATE:-0} == 1 ]]; then
    printf '\n' >> "${0%/*}/no-jit"
fi
printf 'mock native proof\n'
SH
chmod +x "$root/worker"

reset_artifacts() {
    cp "$root/worker" "$directory/no-jit"
    cp "$root/worker" "$directory/jit-off"
    sha256sum "$directory/no-jit" "$directory/jit-off" > "$directory/binary-sha256.log"
    rm -f "$ARTIFACT_INVOCATIONS"
}

verify() {
    bash examples/jit_support/verify_artifacts.sh "$1"
}

refuse() {
    local result=0
    verify "$1" > "$root/refusal.log" 2>&1 || result=$?
    [[ $result == 4 ]]
}

run_recipe() {
    make --no-print-directory jit-size-run COST_DIR="$directory" COST_SAMPLES=1 COST_ITERATIONS=1
}

reset_artifacts
verify "$directory"
cp -r "$directory" "$root/relocated"
verify "$root/relocated"
printf 'changed\n' >> "$root/relocated/no-jit"
refuse "$root/relocated"
verify "$directory"

printf 'changed\n' >> "$directory/no-jit"
if run_recipe > "$root/before.log" 2>&1; then exit 1; fi
grep -Fq 'hash mismatch' "$root/before.log"
[[ ! -e $ARTIFACT_INVOCATIONS ]]

reset_artifacts
export ARTIFACT_PROOF_MUTATE=1
if run_recipe > "$root/proof.log" 2>&1; then exit 1; fi
grep -Fq 'hash mismatch' "$root/proof.log"
grep -Fq -- '--mode auto' "$ARTIFACT_INVOCATIONS"
if grep -Fq -- '--compare' "$ARTIFACT_INVOCATIONS"; then exit 1; fi
export ARTIFACT_PROOF_MUTATE=0

reset_artifacts
export ARTIFACT_MUTATE=1 ARTIFACT_RESULT=2
if run_recipe > "$root/during.log" 2>&1; then exit 1; fi
grep -Fq 'hash mismatch' "$root/during.log"
grep -Fq -- '--compare' "$ARTIFACT_INVOCATIONS"

reset_artifacts
export ARTIFACT_MUTATE=0 ARTIFACT_RESULT=2
if run_recipe > "$root/gate.log" 2>&1; then exit 1; fi
grep -Fq 'JIT cost artifact hashes verified' "$root/gate.log"
grep -Fq 'Error 2' "$root/gate.log"
if grep -Fq 'hash mismatch' "$root/gate.log"; then exit 1; fi

reset_artifacts
export ARTIFACT_RESULT=0
run_recipe > "$root/success.log" 2>&1
[[ $(grep -c 'JIT cost artifact hashes verified' "$root/success.log") == 3 ]]
rm "$directory/binary-sha256.log"
refuse "$directory"
reset_artifacts
cat "$directory/binary-sha256.log" >> "$directory/binary-sha256.log.tmp"
cat "$directory/binary-sha256.log.tmp" >> "$directory/binary-sha256.log"
refuse "$directory"
printf 'JIT artifact checks passed: relocation, pre-run and post-failure verification\n'
