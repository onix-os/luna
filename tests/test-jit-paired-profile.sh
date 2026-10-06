#!/usr/bin/env bash
set -euo pipefail

bash -n examples/jit_support/paired_profile.sh examples/jit_support/verify_paired_profile.sh examples/jit_support/annotate_paired_profile.sh tests/test-jit-paired-profile.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
fixture="$root/fixture"
mkdir "$fixture"
cases=(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc oslo_predicate cold_config)
events='Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw Bc Bcm Bi Bim'
printf 'luna=fixture target=x86_64-linux opt_level=3 mode=paired\n' > "$fixture/run.log"
for name in "${cases[@]}"; do
    printf 'case=%s mode=off samples=11 native_instructions=0\n' "$name" >> "$fixture/run.log"
    native=1
    [[ $name != cold_config ]] || native=0
    printf 'case=%s mode=auto samples=11 native_instructions=%s\n' "$name" "$native" >> "$fixture/run.log"
    printf 'paired_case=%s result=failed\n' "$name" >> "$fixture/run.log"
done
printf '==42== Collected : 900 0 0 0 0 0 0 0 0 0 0 0 0\n' >> "$fixture/run.log"
for ((part=1; part<=19; part++)); do
    file="$fixture/profile.callgrind.$part"
    trigger='--dump-before=jit_bench::report'
    values=0
    if (( part % 2 && part < 19 )); then values='100 0 0 0 0 0 0 0 0 0 0 0 0'; fi
    if (( part == 19 )); then file="$fixture/profile.callgrind"; trigger='Program termination'; fi
    printf 'pid: 42\ncmd: fixture --mode paired --samples 11\npart: %s\ndesc: Trigger: %s\npositions: instr line\nevents: %s\nsummary: %s\nfn=(1) model\n0x1000 0 %s\ntotals: %s\n' "$part" "$trigger" "$events" "$values" "$values" "$values" > "$file"
done
verify() {
    bash examples/jit_support/verify_paired_profile.sh "$1"
}
verify "$fixture"
[[ $(wc -l < "$fixture/parts.tsv") == 10 ]]
[[ $(awk -F '\t' '$1=="closure_upvalue" {print $2}' "$fixture/parts.tsv") == 7 ]]
checks=0
reset_case() {
    checks=$((checks+1))
    directory="$root/case-$checks"
    cp -r "$fixture" "$directory"
}
refuse() {
    result=0
    verify "$directory" > "$root/refusal.log" 2>&1 || result=$?
    [[ $result == 4 ]] || { cat "$root/refusal.log"; echo "Accepted invalid case $checks" >&2; exit 1; }
}
reset_case; rm "$directory/profile.callgrind.7"; refuse
reset_case; cp "$directory/profile.callgrind.1" "$directory/profile.callgrind.20"; refuse
reset_case; rm "$directory/profile.callgrind"; refuse
reset_case; sed -i 's/part: 7/part: 8/' "$directory/profile.callgrind.7"; refuse
reset_case; sed -i 's/--dump-before=jit_bench::report/Program termination/' "$directory/profile.callgrind.7"; refuse
reset_case; sed -i 's/pid: 42/pid: 43/' "$directory/profile.callgrind.7"; refuse
reset_case; sed -i 's/cmd: fixture/cmd: changed/' "$directory/profile.callgrind.7"; refuse
reset_case; printf 'cmd: fixture --mode paired --samples 11\n' >> "$directory/profile.callgrind.1"; refuse
reset_case; sed -i 's/^summary:.*/summary: 0/;s/^totals:.*/totals: 0/' "$directory/profile.callgrind.7"; refuse
reset_case; sed -i 's/^summary:.*/summary: 100 0 0 0 0 0 0 0 0 0 0 0 0/;s/^totals:.*/totals: 100 0 0 0 0 0 0 0 0 0 0 0 0/' "$directory/profile.callgrind.8"; refuse
reset_case; sed -i 's/^summary:.*/summary: 0 1 0 0 0 0 0 0 0 0 0 0 0/;s/^totals:.*/totals: 0 1 0 0 0 0 0 0 0 0 0 0 0/' "$directory/profile.callgrind"; refuse
reset_case; sed -i 's/events: Ir Dr/events: Dr Ir/' "$directory/profile.callgrind.7"; refuse
reset_case; printf 'summary: 0\n' >> "$directory/profile.callgrind.7"; refuse
reset_case; sed -i 's/totals: 100/totals: 99/' "$directory/profile.callgrind.7"; refuse
reset_case; sed -i 's/summary: 100/summary: -1/' "$directory/profile.callgrind.7"; refuse
reset_case; sed -i '/Collected/d' "$directory/run.log"; refuse
reset_case; sed -i 's/Collected : 900/Collected : 901/' "$directory/run.log"; refuse
reset_case; printf '==42== Collected : 900 0 0 0 0 0 0 0 0 0 0 0 0\n' >> "$directory/run.log"; refuse
reset_case; sed -i '/case=closure_upvalue mode=auto/d' "$directory/run.log"; refuse
reset_case; sed -i 's/case=integer_loop mode=off/case=integer_loop mode=auto/' "$directory/run.log"; refuse
reset_case; sed -i 's/case=closure_upvalue /case=array_table /' "$directory/run.log"; refuse
reset_case; sed -i 's/samples=11/samples=10/' "$directory/run.log"; refuse
reset_case; sed -i 's/native_instructions=1/native_instructions=0/' "$directory/run.log"; refuse
reset_case; sed -i 's/case=integer_loop mode=off samples=11 native_instructions=0/case=integer_loop mode=off samples=11 native_instructions=1/' "$directory/run.log"; refuse
reset_case; sed -i 's/case=cold_config mode=auto samples=11 native_instructions=0/case=cold_config mode=auto samples=11 native_instructions=1/' "$directory/run.log"; refuse
reset_case; printf 'luna=fixture target=x86_64-linux opt_level=3 mode=paired\n' >> "$directory/run.log"; refuse
reset_case; sed -i 's/opt_level=3/opt_level=s/' "$directory/run.log"; refuse

model="$root/exclusive-model"
cat > "$model" <<'PROFILE'
positions: instr line
events: Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw Bc Bcm Bi Bim
summary: 100 20 5 0 0 0 0 0 0 0 0 0 0
fn=(1) model
0x1000 0 40 10 2
cfn=(2) carried
calls=0 0x2000 0
+4 0 900 180 45
cfn=(3) called
calls=3 0x3000 0
+8 0 500 100 25
+4 0 60 10 3
totals: 100 20 5 0 0 0 0 0 0 0 0 0 0
PROFILE
sha256sum "$model" > "$root/model.sha256"
normalize() {
    awk -f examples/jit_support/callgrind_exclusive.awk "$1"
}
normalize "$model" > "$root/model-normalized"
sed 's/^+4 0 900 180 45$/+4 0 0/' "$model" > "$root/model-expected"
cmp "$root/model-normalized" "$root/model-expected"
sha256sum --check "$root/model.sha256"
normalization_checks=0
reset_model() {
    normalization_checks=$((normalization_checks+1))
    cp "$model" "$root/bad-model"
}
refuse_model() {
    result=0
    normalize "$root/bad-model" > "$root/bad-normalized" 2> "$root/bad-normalized.log" || result=$?
    [[ $result == 4 ]] || { echo "Accepted invalid model $normalization_checks" >&2; exit 1; }
}
reset_model; sed -i '/^positions:/d' "$root/bad-model"; refuse_model
reset_model; printf 'events: Ir\n' >> "$root/bad-model"; refuse_model
reset_model; printf 'calls=0 0x2000 0\n' >> "$root/bad-model"; refuse_model
reset_model; sed -i 's/+4 0 60/+4 0 invalid/' "$root/bad-model"; refuse_model
reset_model; sed -i 's/summary: 100/summary: 101/' "$root/bad-model"; refuse_model
reset_model; sed -i 's/positions: instr line/positions: line/' "$root/bad-model"; refuse_model
reset_model; sed -i '/calls=0/a calls=1 0x2000 0' "$root/bad-model"; refuse_model
reset_model; sed -i 's/calls=3/calls=-3/' "$root/bad-model"; refuse_model
reset_model; sed -i 's/+4 0 60 10 3/+4 0/' "$root/bad-model"; refuse_model
reset_model; sed -i 's/+4 0 60/+4 0 -60/' "$root/bad-model"; refuse_model
reset_model; sed -i 's/summary: 100/summary: invalid/' "$root/bad-model"; refuse_model
sed 's/calls=0 /calls=00 /' "$model" > "$root/zero-padded-model"
normalize "$root/zero-padded-model" > "$root/zero-padded-normalized"
grep -Fxq '+4 0 0' "$root/zero-padded-normalized"

mkdir "$root/bin"
cat > "$root/bin/nm" <<'SH'
#!/usr/bin/env bash
if [[ ${MOCK_NO_SYMBOL:-0} == 0 ]]; then printf '000000000001 T jit_bench::report\n'; fi
SH
for command in rustc cargo uname callgrind_annotate; do
    printf '#!/usr/bin/env bash\nprintf "mock tool\\n"\n' > "$root/bin/$command"
done
cat > "$root/bin/valgrind" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ $1 == --version ]]; then echo 'mock valgrind'; exit 0; fi
printf '%s\n' "$*" >> "$MOCK_INVOCATIONS"
printf '%s\n' "$@" > "$MOCK_INVOCATIONS.args"
[[ $* == *'--mode paired --samples 11' && $* != *'--check'* ]]
for arg in "$@"; do
    case $arg in --callgrind-out-file=*) directory=${arg#*=}; directory=${directory%/*} ;; esac
done
cp "$MOCK_FIXTURE"/profile.callgrind* "$directory/"
cat "$MOCK_FIXTURE/run.log"
if [[ ${MOCK_MUTATE:-0} == 1 ]]; then printf '\nchanged\n' >> "$MOCK_BINARY"; fi
exit "${MOCK_EXIT:-0}"
SH
chmod +x "$root/bin/"*
export MOCK_FIXTURE="$fixture" MOCK_INVOCATIONS="$root/invocations" MOCK_BINARY="$root/benchmark"
printf '#!/usr/bin/env bash\nexit 0\n' > "$MOCK_BINARY"
chmod +x "$MOCK_BINARY"
runner() {
    PATH="$root/bin:$PATH" bash examples/jit_support/paired_profile.sh "$1" "$2"
}
relative_binary=$(realpath --relative-to="$PWD" "$MOCK_BINARY")
runner "$relative_binary" "$root/success"
grep -Fxq "$relative_binary" "$MOCK_INVOCATIONS.args"
grep -Fxq "invocation=$relative_binary" "$root/success/configuration"
grep -Fxq "artifact=$MOCK_BINARY" "$root/success/configuration"
[[ $(find "$root/success" -name 'annotation-*.log' | wc -l) == 9 ]]
if runner "$MOCK_BINARY" "$root/success" > "$root/existing.log" 2>&1; then exit 1; fi
[[ $(wc -l < "$MOCK_INVOCATIONS") == 1 ]]
if runner "$root/missing" "$root/missing-output" > "$root/missing.log" 2>&1; then exit 1; fi
[[ ! -e $root/missing-output ]]
export MOCK_NO_SYMBOL=1
if runner "$MOCK_BINARY" "$root/no-symbol" > "$root/no-symbol.log" 2>&1; then exit 1; fi
[[ $(wc -l < "$MOCK_INVOCATIONS") == 1 ]]
export MOCK_NO_SYMBOL=0 MOCK_EXIT=23
result=0; runner "$MOCK_BINARY" "$root/profiler-failure" > "$root/failure.log" 2>&1 || result=$?
[[ $result == 23 && -s $root/profiler-failure/binary-after.log ]]
export MOCK_MUTATE=1
result=0; runner "$MOCK_BINARY" "$root/mutated-failure" > "$root/mutation.log" 2>&1 || result=$?
[[ $result == 4 ]]
export MOCK_EXIT=0
result=0; runner "$MOCK_BINARY" "$root/mutated-success" > "$root/mutation-success.log" 2>&1 || result=$?
[[ $result == 4 ]]
printf 'Paired profile checks passed: %s malformed partitions, %s malformed self-cost models and runner lifecycle checks\n' "$checks" "$normalization_checks"
