#!/usr/bin/env bash
set -euo pipefail
(( $# == 4 || $# == 5 ))
image=$(realpath "$1")
out=$2
cpu=$3
windows=$4
experiment=${5:-strided}
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
    *) echo 'unknown timing experiment' >&2; exit 2;;
esac
[[ $cpu =~ ^[0-9]+$ && $windows =~ ^[0-9]+$ ]]
(( windows >= 3 && windows <= 1000 ))
test -x "$image/test-binary"
test ! -e "$out"
taskset -c "$cpu" true
mkdir -p "$out"
verify() {
    sha256sum -c "$image/binary.sha256" "$image/source.sha256"
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
sha256sum "$0" "${BASH_SOURCE[0]%/*}/strided_timing_audit.pl" > "$out/tools.sha256"
printf 'protocol=1\nexperiment=%s\ncpu=%s\nwindows=%s\nimage=%s\nwarmups=2\nmeasured_executions=1\nscope=warm execution only; test image; not release acceptance\n' "$experiment" "$cpu" "$windows" "$image" > "$out/configuration"
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
            log="$out/$window-$shape-$label.log"
            printf '%s %s %s\n' "$window" "$shape" "$label" >> "$out/order.log"
            env "$case_variable=$shape" "$mode_variable=$mode" taskset -c "$cpu" \
                "$image/test-binary" "$test_name" \
                --ignored --exact --nocapture --test-threads=1 > "$log" 2>&1
            grep -Fq 'test result: ok. 1 passed; 0 failed' "$log"
            grep -Eq "^TIMING_V1 case=$shape $record_field=$([[ $mode == 1 ]] && echo true || echo false) elapsed_ns=[1-9][0-9]* " "$log"
        done
    done
    telemetry > "$out/$window-after.telemetry"
done
verify > "$out/verification-after.log"
sha256sum -c "$out/binary.sha256" "$out/tools.sha256" >> "$out/verification-after.log"
printf 'complete\n' > "$out/completed"
