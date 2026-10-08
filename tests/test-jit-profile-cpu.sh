#!/usr/bin/env bash
set -euo pipefail
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
cpu=$(awk '/^Cpus_allowed_list:/{split($2, cpus, /[-,]/); print cpus[1]}' /proc/self/status)
make --no-print-directory jit-profile-cpu-check PROFILE_CPU="$cpu"
taskset -c "$cpu" make --no-print-directory jit-profile-cpu-check
for invalid in '' -1 invalid "$cpu,$cpu" "$cpu-$cpu" 999999999; do
    if make --no-print-directory jit-profile-cpu-check PROFILE_CPU="$invalid" > "$root/refusal" 2>&1; then
        echo "Accepted invalid CPU: $invalid" >&2
        exit 1
    fi
done
for target in jit-bench-profile-run jit-cost-profile-run; do
    make --no-print-directory -n "$target" PROFILE_CPU="$cpu" > "$root/recipe"
    grep -Fq "taskset -c '$cpu' valgrind" "$root/recipe"
    grep -Fq "printf 'cpu=%s\\n' '$cpu'" "$root/recipe"
    if make --no-print-directory "$target" PROFILE_CPU=invalid > "$root/refusal" 2>&1; then
        echo "Accepted invalid profile CPU: $target" >&2
        exit 1
    fi
    grep -Fq 'PROFILE_CPU must select one CPU' "$root/refusal"
done
printf 'Profile CPU checks passed: explicit/default affinity, six invalid selections, two profile recipes\n'
