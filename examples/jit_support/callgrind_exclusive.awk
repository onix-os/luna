BEGIN { OFS=" " }
/^positions:/ { if ($0!="positions: instr line") bad=1; positions++ }
/^events:/ { if ($0!="events: Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw Bc Bcm Bi Bim") bad=1; events++ }
/^summary:/ {
    summaries++
    if (NF!=2 && NF!=14) bad=1
    if (NF==2 && $2!="0") bad=1
    for (i=2; i<=NF; i++) {
        if ($i !~ /^[0-9]+$/ || length($i)>15) bad=1
        expected[i-2]=$i+0
    }
}
/^calls=/ {
    if (pending || $0 !~ /^calls=[0-9]+( |$)/) bad=1
    pending=1
    split($1, call, "=")
    zero=(call[2]+0==0)
    print
    next
}
$1 ~ /^(0x[[:xdigit:]]+|[+-]?[0-9]+|\*)$/ {
    if (NF<3 || NF>15 || $2 !~ /^(0x[[:xdigit:]]+|[+-]?[0-9]+|\*)$/) bad=1
    for (i=3; i<=NF; i++) {
        if ($i !~ /^[0-9]+$/ || length($i)>15) bad=1
        if (!pending) self[i-3]+=$i
    }
    if (pending && zero) { print $1,$2,0; zero_calls++ }
    else print
    pending=0
    next
}
{
    if (pending && NF) bad=1
    print
}
END {
    if (pending || positions!=1 || events!=1 || summaries!=1) bad=1
    for (i=0; i<13; i++) if (self[i]+0 != expected[i]+0) bad=1
    if (bad) {
        print "Exclusive profile verification failed: malformed records or self-count mismatch" > "/dev/stderr"
        exit 4
    }
    printf "Exclusive profile verified: Ir=%.0f zero_call_records=%d\n", self[0], zero_calls > "/dev/stderr"
}
