#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf 'Paired profile verification failed: %s\n' "$*" >&2
    exit 4
}

[[ $# == 1 ]] || fail 'expected a profile directory'
directory=$1
cases=(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc oslo_predicate cold_config)
events='Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw Bc Bcm Bi Bim'
[[ -f $directory/run.log ]] || fail 'missing benchmark report'
awk -v names="${cases[*]}" '
    BEGIN { split(names, cases, " ") }
    /^luna=/ { headers++; if ($0 !~ / opt_level=3 mode=paired$/) bad=1 }
    /^case=/ {
        delete fields
        for (i=1; i<=NF; i++) { split($i, pair, "="); fields[pair[1]]=pair[2] }
        index_case=int(reports/2)+1
        mode=reports%2 ? "auto" : "off"
        if (index_case>9 || fields["case"]!=cases[index_case] || fields["mode"]!=mode || fields["samples"]!="11") bad=1
        native=fields["native_instructions"]
        if (native !~ /^[0-9]+$/ || ((mode=="off" || index_case==9) ? native!=0 : native<=0)) bad=1
        reports++
    }
    /^paired_case=/ {
        split($1, pair, "="); pairs++
        if (pairs>9 || pair[2]!=cases[pairs] || reports!=pairs*2) bad=1
    }
    END { exit (bad || headers!=1 || reports!=18 || pairs!=9) }
' "$directory/run.log" || fail 'unexpected paired benchmark reports'

shopt -s nullglob
parts=("$directory"/profile.callgrind.*)
[[ ${#parts[@]} == 18 ]] || fail 'expected eighteen report dumps'
totals=(0 0 0 0 0 0 0 0 0 0 0 0 0)
temporary=$(mktemp)
trap 'rm -f "$temporary"' EXIT
printf 'case\tpart\t%s\n' "${events// /$'\t'}" > "$temporary"
pid=
command=
for ((part=1; part<=19; part++)); do
    file="$directory/profile.callgrind.$part"
    trigger='desc: Trigger: --dump-before=jit_bench::report'
    if (( part == 19 )); then
        file="$directory/profile.callgrind"
        trigger='desc: Trigger: Program termination'
    fi
    [[ -f $file ]] || fail "missing part $part"
    actual_pid=$(sed -n 's/^pid: //p' "$file")
    actual_command=$(sed -n 's/^cmd: //p' "$file")
    [[ $actual_pid =~ ^[1-9][0-9]*$ ]] || fail "invalid pid in part $part"
    [[ $actual_command != *$'\n'* ]] || fail "duplicate command in part $part"
    if [[ -z $pid ]]; then pid=$actual_pid; command=$actual_command; fi
    [[ $actual_pid == "$pid" && $actual_command == "$command" ]] || fail 'mixed profile processes'
    [[ $actual_command == *' --mode paired --samples 11' ]] || fail 'unexpected profile command'
    values=$(awk -v part="$part" -v trigger="$trigger" -v events="$events" '
        /^part:/ { parts++; if ($0!="part: "part) bad=1 }
        /^desc: Trigger:/ { triggers++; if ($0!=trigger) bad=1 }
        /^events:/ { event_lines++; if ($0!="events: "events) bad=1 }
        /^(summary|totals):/ {
            kind=$1; lines[kind]++
            if (NF!=2 && NF!=14) bad=1
            if (NF==2 && $2!="0") bad=1
            for (i=2; i<=NF; i++) {
                if ($i !~ /^[0-9]+$/ || length($i)>15) bad=1
                counts[kind,i-1]=$i
            }
        }
        END {
            if (parts!=1 || triggers!=1 || event_lines!=1 || lines["summary:"]!=1 || lines["totals:"]!=1) bad=1
            for (i=1; i<=13; i++) if (counts["summary:",i]+0 != counts["totals:",i]+0) bad=1
            if (bad) exit 1
            for (i=1; i<=13; i++) printf "%s%s", i==1 ? "" : " ", counts["summary:",i]=="" ? "0" : counts["summary:",i]
            print ""
        }
    ' "$file") || fail "invalid counters or metadata in part $part"
    read -r -a counters <<< "$values"
    if (( part < 19 && part % 2 )); then
        (( 10#${counters[0]} > 0 )) || fail "empty workload part $part"
        printf '%s\t%s\t%s\n' "${cases[(part-1)/2]}" "$part" "${values// /$'\t'}" >> "$temporary"
    else
        for count in "${counters[@]}"; do
            (( 10#$count == 0 )) || fail "collected work outside workload parts: $part"
        done
    fi
    for ((i=0; i<13; i++)); do totals[i]=$((totals[i] + 10#${counters[i]})); done
done
collected=$(sed -n "s/^==$pid== Collected : *//p" "$directory/run.log")
[[ $collected != *$'\n'* ]] || fail 'duplicate collected totals'
read -r -a counters <<< "$collected"
[[ ${#counters[@]} == 13 ]] || fail 'missing collected totals'
for ((i=0; i<13; i++)); do
    [[ ${counters[i]} =~ ^[0-9]{1,15}$ ]] || fail 'invalid collected total'
    (( 10#${counters[i]} == totals[i] )) || fail "partition sum mismatch for event $i"
done
cp "$temporary" "$directory/parts.tsv"
printf 'Paired profile verified: nine combined Off/Auto intervals, Ir=%s\n' "${totals[0]}"
