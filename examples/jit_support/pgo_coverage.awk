/^[[:space:]]*[^[:space:]].*:$/ { name="" }
/native_region8boundary:$/ { name="region_boundary" }
/Admitted13invoke_rooted:$/ { name="rooted_call" }
/native_region.*7Session15complete_inline:$/ { name="inline_batch" }
name != "" && /Block counts:/ {
    counts=$0
    sub(/^.*Block counts: /,"",counts)
    if (counts !~ /^\[([0-9]+(, *[0-9]+)*)?\]$/) {
        malformed=1
        name=""
        next
    }
    gsub(/[^0-9]+/," ",counts)
    n=split(counts,values," ")
    for (i=1; i<=n; i++) totals[name]+=values[i]
    seen[name]++
    name=""
}
END {
    for (name in totals) printf "%s blocks_total=%.0f records=%d\n",name,totals[name],seen[name]
    if (malformed || !seen["region_boundary"] || !seen["rooted_call"] || !seen["inline_batch"] || totals["region_boundary"]<=0 || totals["rooted_call"]<=0 || totals["inline_batch"]<=0) {
        print "PGO training must cover region boundaries, rooted calls and inline batching" > "/dev/stderr"
        exit 2
    }
}
