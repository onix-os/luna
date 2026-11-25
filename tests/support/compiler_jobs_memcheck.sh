set -eu -o pipefail

cargo=$1
valgrind=$2
evidence=$3
target=$4
root=$PWD
test ! -e "$evidence"
mkdir -p "$evidence"
git rev-parse HEAD >"$evidence/base-commit"
git diff --binary HEAD >"$evidence/source.patch"
git ls-files -z src derive util worker Cargo.toml Cargo.lock build.rs |
    sort -z | xargs -0 sha256sum >"$evidence/source.sha256"
rustc -vV >"$evidence/toolchain.txt"
sha256sum "$root/tests/support/compiler_jobs_memcheck.sh" \
    "$root/tests/support/compiler_jobs_memcheck.pl" \
    "$root/tests/test-jit-compiler-jobs-memcheck.pl" >"$evidence/producer.sha256"
"$valgrind" --version >"$evidence/valgrind.txt"
flags=()
if [[ -n $target ]]; then flags=(--target "$target"); fi
"$cargo" test --locked -p luna --all-features --lib "${flags[@]}" --no-run \
    --message-format=json-render-diagnostics >"$evidence/build.jsonl"
mapfile -t binaries < <(jq -r 'select(.reason=="compiler-artifact" and .target.name=="luna" and .target.kind==["lib"] and .profile.test==true and .executable!=null) | .executable' "$evidence/build.jsonl")
test "${#binaries[@]}" = 1
binary=${binaries[0]}
sha256sum "$binary" >"$evidence/executable.sha256"
"$binary" jit::compiler_jobs::tests --list >"$evidence/selected.txt"
set +e
"$valgrind" --tool=memcheck --trace-children=no --error-exitcode=97 \
    --leak-check=full --errors-for-leak-kinds=definite,indirect \
    "$binary" jit::compiler_jobs::tests --test-threads=1 >"$evidence/memcheck.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" >"$evidence/memcheck.status"
test "$status" = 0
perl "$root/tests/support/compiler_jobs_memcheck.pl" "$evidence/selected.txt" "$evidence/memcheck.log" >"$evidence/report.json"
sha256sum -c "$evidence/source.sha256" "$evidence/executable.sha256" \
    "$evidence/producer.sha256" >"$evidence/identity-after.txt"
cat "$evidence/report.json"
