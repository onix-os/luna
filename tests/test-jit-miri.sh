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
count=
case " $* " in
    *' jit::compiler_wire '*) count=8 ;;
    *' jit::compiler_request::tests '*) count=8 ;;
    *' jit::compiler_reply::tests '*) count=7 ;;
    *' jit::global_box::string_tests '*) count=5 ;;
    *' jit::compiler_reply::publication_tests '*) count=4 ;;
    *' jit::resources::reservation_tests '*) count=3 ;;
    *' jit::resources::compaction_delta_tests '*) count=3 ;;
    *' jit::compiler_stream::tests '*) count=7 ;;
    *' jit::backend::artifact::wire::tests '*) count=5 ;;
    *' jit::backend::artifact::link::tests '*) count=7 ;;
    *' jit::backend::artifact::target::tests '*) count=7 ;;
    *' jit::native_fault::tests '*) count=6 ;;
    *' jit::backend::native_memory::tests '*) count=2 ;;
    *' thread::vm::global_index '*) count=2 ;;
esac
if [[ -n $count ]]; then
    if [[ -n ${EMPTY_FILTER:-} && " $* " == *" $EMPTY_FILTER "* ]]; then count=0; fi
    printf 'test result: ok. %s passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' "$count"
fi
SH
chmod +x "$root/cargo"

run_recipe() {
    make --no-print-directory jit-miri CARGO="$root/cargo" \
        MIRI_DIR="$root/$1" > "$root/$1.log" 2>&1
}

run_recipe success
logs=("$root/success/"*.log)
test "${#logs[@]}" -eq 68
for log in "${logs[@]}"; do
    [[ ${log##*/} =~ ^[a-z0-9_-]+\.log$ ]]
done
grep -Fq 'jit::resources::tests -- --test-threads=1 --skip jit::resources::tests::installation_refusal_reclaims_generated_mappings' \
    "$root/success/jit-resources-tests.log"
grep -Fq 'finalizers::tests -- --test-threads=1' "$root/success/finalizers-tests.log"
grep -Fq 'lua::memory_tests -- --test-threads=1' "$root/success/lua-memory_tests.log"
grep -Fq 'jit::scoped_helpers::tests -- --test-threads=1' "$root/success/jit-scoped_helpers-tests.log"

transport_filters=(
    jit::compiler_wire
    jit::compiler_request::tests
    jit::compiler_reply::tests
    jit::compiler_stream::tests
    jit::backend::artifact::wire::tests
    jit::backend::artifact::link::tests
    jit::backend::artifact::target::tests
    jit::native_fault::tests
    jit::backend::native_memory::tests
    jit::helpers::existing_entry_tests
    thread::vm::global_index
    jit::array_window::
    jit::backend::specialized::tests
    jit::entry_admission::tests
    jit::abi::transport_inputs::tests
    jit::readonly_lookup::tests
    jit::readonly_lookup::scalar_entry
    jit::readonly_lookup::binding::tests
    jit::readonly_lookup::binding::integer_resume::tests
    table::raw::code_read_hint_tests::strong_peek
)
for filter in "${transport_filters[@]}"; do
    grep -Fq "$filter -- --test-threads=1" \
        "$root/success/${filter//::/-}.log"
done

publication_filters=(jit::global_box::string_tests jit::compiler_reply::publication_tests)
for filter in "${publication_filters[@]}"; do
    grep -Fq "$filter -- --test-threads=1" "$root/success/jit-error-publication.log"
done

workspace_filter=jit::resources::reservation_tests
grep -Fq "$workspace_filter -- --test-threads=1" "$root/success/jit-worker-workspace.log"
compaction_filter=jit::resources::compaction_delta_tests
grep -Fq "$compaction_filter -- --test-threads=1" "$root/success/jit-compaction-delta.log"

for filter in setup jit::resources::tests jit::handoff::tests jit::scoped_helpers::tests "${transport_filters[@]}" "${publication_filters[@]}" "$workspace_filter" "$compaction_filter"; do
    export FAIL_FILTER="$filter"
    dir="failure-${filter//::/-}"
    if run_recipe "$dir"; then
        echo "Recipe ignored failure for $filter" >&2
        exit 1
    fi
    grep -Fq 'mock Miri failure' "$root/$dir.log"
    test ! -e "$root/$dir/lua-memory_tests.log"
done
unset FAIL_FILTER
for filter in jit::compiler_wire jit::compiler_request::tests jit::compiler_reply::tests jit::compiler_stream::tests jit::backend::artifact::wire::tests jit::backend::artifact::link::tests jit::backend::artifact::target::tests jit::native_fault::tests jit::backend::native_memory::tests thread::vm::global_index "${publication_filters[@]}" "$workspace_filter" "$compaction_filter"; do
    export EMPTY_FILTER="$filter"
    dir="empty-${filter//::/-}"
    if run_recipe "$dir"; then
        echo "Recipe accepted zero selected models for $filter" >&2
        exit 1
    fi
    grep -Fq 'test result: ok. 0 passed;' "$root/$dir.log"
    test ! -e "$root/$dir/lua-memory_tests.log"
done
echo 'Miri recipe checks passed: 68 portable logs; 28 failures propagated; 14 zero-model results rejected'
