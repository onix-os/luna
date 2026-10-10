/native_region8boundary:$/ { name="region_boundary" }
/Admitted13invoke_rooted:$/ { name="rooted_call" }
name != "" && /Block counts:/ {
    counts=$0
    sub(/^.*Block counts: /,"",counts)
    gsub(/[^0-9]+/," ",counts)
    n=split(counts,values," ")
    for (i=1; i<=n; i++) totals[name]+=values[i]
    seen[name]++
    name=""
}
END {
    for (name in totals) printf "%s blocks_total=%.0f records=%d\n",name,totals[name],seen[name]
    if (!seen["region_boundary"] || !seen["rooted_call"] || totals["region_boundary"]<=0 || totals["rooted_call"]<=0) {
        print "PGO training must execute region boundaries and rooted calls" > "/dev/stderr"
        exit 2
    }
}
