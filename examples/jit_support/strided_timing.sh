#!/usr/bin/env bash
set -euo pipefail
(( $# >= 4 && $# <= 6 ))
image=$(realpath "$1")
out=$2
cpu=$3
windows=$4
experiment=${5:-strided}
baseline=
case $experiment in
    strided)
        cases=(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc)
        case_variable=LUNA_STRIDED_CASE
        mode_variable=LUNA_STRIDED
        record_field=strided
        test_name=jit::payload_runtime_tests::strided_profile::timing::time_strided_runtime
        ;;
    preferred_entry)
        cases=(rust_callbacks)
        case_variable=LUNA_PREFERRED_CASE
        mode_variable=LUNA_PREFERRED_ENTRY
        record_field=preferred
        test_name=jit::payload_runtime_tests::preferred_entry::timing::time_preferred_runtime
        ;;
    callback_scratch|callback_scratch_baseline)
        cases=(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc)
        case_variable=LUNA_MEMORY_CASE
        mode_variable=LUNA_MEMORY_SELECTED
        record_field=scratch
        test_name=jit::payload_runtime_tests::strided_profile::timing::time_callback_scratch_runtime
        ;;
    *) echo 'unknown timing experiment' >&2; exit 2;;
esac
if [[ $experiment == callback_scratch_baseline ]]; then
    (( $# == 6 ))
    baseline=$(realpath "$6")
    test "$image" != "$baseline"
    test -x "$baseline/test-binary"
    test -s "$image/source-root" && test -s "$baseline/source-root"
else
    (( $# <= 5 ))
fi
[[ $cpu =~ ^[0-9]+$ && $windows =~ ^[0-9]+$ ]]
(( windows >= 3 && windows <= 1000 ))
test -x "$image/test-binary"
test ! -e "$out"
taskset -c "$cpu" true
mkdir -p "$out"
verify() {
    for current in "$image" ${baseline:+"$baseline"}; do
        if test -n "$baseline"; then sha256sum "$current/test-binary" | cmp - "$current/binary.sha256"; fi
        sha256sum -c "$current/binary.sha256"
        source_root=$PWD
        if test -f "$current/source-root"; then source_root=$(cat "$current/source-root"); fi
        (cd "$source_root"; sha256sum -c "$current/source.sha256")
    done
}
telemetry() {
    date --iso-8601=ns
    uptime
    cat /proc/stat /proc/pressure/cpu
    for field in topology/core_id topology/thread_siblings_list cpufreq/scaling_cur_freq cpufreq/cpuinfo_max_freq cpufreq/base_frequency; do
        if test -r "/sys/devices/system/cpu/cpu$cpu/$field"; then
            printf 'cpu%s %s=' "$cpu" "$field"
            cat "/sys/devices/system/cpu/cpu$cpu/$field"
        fi
    done
    ps -eo pid,ppid,comm,pcpu,pmem,stat,etime
}
verify > "$out/verification-before.log"
{ awk '/^model name/{print; exit}' /proc/cpuinfo; lscpu -e=CPU,CORE,SOCKET,NODE,ONLINE,MAXMHZ,MINMHZ; } > "$out/host-topology"
sha256sum "$image/test-binary" > "$out/binary.sha256"
if test -n "$baseline"; then sha256sum "$baseline/test-binary" >> "$out/binary.sha256"; fi
sha256sum "$0" "${BASH_SOURCE[0]%/*}/strided_timing_audit.pl" > "$out/tools.sha256"
printf 'protocol=1\nexperiment=%s\ncpu=%s\nwindows=%s\nimage=%s\nwarmups=2\nmeasured_executions=1\nscope=warm execution only; test image; not release acceptance\n' "$experiment" "$cpu" "$windows" "$image" > "$out/configuration"
if test -n "$baseline"; then
    printf 'baseline=%s\n' "$baseline" >> "$out/configuration"
    sha256sum "$image/source-root" "$image/source.sha256" "$baseline/source-root" "$baseline/source.sha256" >> "$out/tools.sha256"
fi
for (( window=0; window<windows; window++ )); do
    telemetry > "$out/$window-before.telemetry"
    for (( position=0; position<${#cases[@]}; position++ )); do
        shape=${cases[$(((position + window) % ${#cases[@]}))]}
        case $((window % 6)) in
            0) order=(a candidate b);;
            1) order=(candidate b a);;
            2) order=(b a candidate);;
            3) order=(b candidate a);;
            4) order=(candidate a b);;
            5) order=(a b candidate);;
        esac
        for label in "${order[@]}"; do
            mode=0
            test "$label" != candidate || mode=1
            worker_image=$image
            worker_case=$case_variable
            worker_mode=$mode_variable
            worker_test=$test_name
            worker_field=$record_field
            if [[ -n $baseline && $label != candidate ]]; then
                worker_image=$baseline
                worker_case=LUNA_STRIDED_CASE
                worker_mode=LUNA_STRIDED
                worker_test=jit::payload_runtime_tests::strided_profile::timing::time_strided_runtime
                worker_field=strided
            fi
            log="$out/$window-$shape-$label.log"
            printf '%s %s %s\n' "$window" "$shape" "$label" >> "$out/order.log"
            env -u LUNA_STRIDED_CASE -u LUNA_STRIDED -u LUNA_MEMORY_CASE -u LUNA_MEMORY_SELECTED -u LUNA_PREFERRED_CASE -u LUNA_PREFERRED_ENTRY \
                "$worker_case=$shape" "$worker_mode=$mode" taskset -c "$cpu" \
                "$worker_image/test-binary" "$worker_test" \
                --ignored --exact --nocapture --test-threads=1 > "$log" 2>&1
            grep -Fq 'test result: ok. 1 passed; 0 failed' "$log"
            grep -Eq "^TIMING_V1 case=$shape $worker_field=$([[ $mode == 1 ]] && echo true || echo false) elapsed_ns=[1-9][0-9]* " "$log"
        done
    done
    telemetry > "$out/$window-after.telemetry"
done
verify > "$out/verification-after.log"
sha256sum -c "$out/binary.sha256" "$out/tools.sha256" >> "$out/verification-after.log"
printf 'complete\n' > "$out/completed"
