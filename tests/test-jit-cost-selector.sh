#!/usr/bin/env bash
set -euo pipefail
no_jit=${1:?no-feature image required}
jit=${2:?jit image required}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cases=(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc oslo_predicate cold_config)
passed=0
refused=0
for configuration in no-jit/off jit/off jit/auto; do
    side=${configuration%/*}
    mode=${configuration#*/}
    image=$jit
    compiled=1
    if [[ $side == no-jit ]]; then image=$no_jit; compiled=0; fi
    for selected in all "${cases[@]}"; do
        args=(--worker --mode "$mode" --iterations 1)
        if [[ $selected != all ]]; then args+=(--case "$selected"); fi
        output=$tmp/$side-$mode-$selected
        if ! "$image" "${args[@]}" >"$output" 2>"$output.err"; then
            cat "$output" "$output.err" >&2
            exit 1
        fi
        perl - "$output" "$compiled" "$mode" "$selected" <<'PERL'
use strict;
use warnings;
my ($path,$compiled,$mode,$selected) = @ARGV;
open my $file, '<', $path or die $!;
chomp(my @lines = <$file>);
shift(@lines) eq "feature_cost_protocol=1 jit_compiled=$compiled mode=$mode" or die 'header';
my @cases = qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc oslo_predicate cold_config);
my @expected = $selected eq 'all' ? @cases : ($selected);
@lines == @expected or die "case count: $path";
for my $case (@expected) {
    shift(@lines) =~ /^case=$case ns=[1-9][0-9]* iterations=1 verified=1 native_instructions=([0-9]+)$/ or die "case protocol: $path";
    ($mode eq 'off' || $case eq 'cold_config' ? $1 == 0 : $1 > 0) or die "native proof: $path";
}
PERL
        passed=$((passed + 1))
    done
    for selected in '' unknown Oslo_predicate; do
        if "$image" --worker --mode "$mode" --iterations 1 --case "$selected" >"$tmp/refused" 2>"$tmp/refused.err"; then exit 1; fi
        test ! -s "$tmp/refused"
        refused=$((refused + 1))
    done
    for args in missing-case compare-selection; do
        command=("$image" --worker --case)
        if [[ $args == compare-selection ]]; then command=("$image" --compare missing-a missing-b --case oslo_predicate); fi
        if "${command[@]}" >"$tmp/refused" 2>"$tmp/refused.err"; then exit 1; fi
        test ! -s "$tmp/refused"
        refused=$((refused + 1))
    done
done
test "$passed" = 30
test "$refused" = 15
printf 'selector_configs=%s refused_configs=%s all_nine_cases_verified=1\n' "$passed" "$refused"
