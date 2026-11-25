#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
shell=$(command -v bash)

check() {
    local file=$1
    MAKEFLAGS= MFLAGS= make --no-print-directory -C "$root" -f "$file" -n \
        SHELL="$shell" CARGO=__luna_dry_cargo__ >"$temporary/default.log" || return 1
    MAKEFLAGS= MFLAGS= make --no-print-directory -C "$root" -f "$file" -n \
        SHELL="$shell" CARGO=__luna_dry_cargo__ build >"$temporary/build.log" || return 1
    cmp "$temporary/default.log" "$temporary/build.log" || return 1
    grep -Eq '^__luna_dry_cargo__ build( |$)' "$temporary/default.log" || return 1
}

check "$root/Makefile"
cat "$root/Makefile" >"$temporary/wrong-goal.mk"
printf '\n.DEFAULT_GOAL := jit-compiler-jobs-memcheck-reader-tests\n' >>"$temporary/wrong-goal.mk"
if check "$temporary/wrong-goal.mk" >"$temporary/negative.log" 2>&1; then
    printf 'wrong default goal was accepted\n' >&2
    exit 1
fi
grep -q 'differ' "$temporary/negative.log"
cat "$root/Makefile" >"$temporary/extra-step.mk"
printf '\n__luna_wrong_default: build\n\t@printf extra-step\\n\n.DEFAULT_GOAL := __luna_wrong_default\n' >>"$temporary/extra-step.mk"
if check "$temporary/extra-step.mk" >"$temporary/extra-step.log" 2>&1; then
    printf 'extra default step was accepted\n' >&2
    exit 1
fi
test -s "$temporary/extra-step.log"
check "$root/Makefile"
printf 'make default: valid/restored builds and two wrong-goal rejections passed\n'
