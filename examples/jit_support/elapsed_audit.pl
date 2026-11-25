use strict;
use warnings;
use JSON::PP;
my ($root, $out) = @ARGV;
$out //= "$root/elapsed";
-f "$out/completed" or die "incomplete collection\n";
open my $order, '<', "$out/order.log" or die $!;
my @actual = <$order>;
my @permutations = ([qw(a candidate b)], [qw(candidate b a)], [qw(b a candidate)], [qw(b candidate a)], [qw(candidate a b)], [qw(a b candidate)]);
my @expected;
for my $round (0..5) {
    for my $cpu ($round % 2 ? (16, 0) : (0, 16)) {
        for my $label (@{$permutations[($round + $cpu / 16) % 6]}) {
            for my $case ($round % 2 ? qw(allocation_gc rust_callbacks polymorphic_metamethod closure_upvalue array_table float_loop integer_loop) : qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc)) {
                push @expected, "$cpu $round $label $case\n";
            }
        }
    }
}
join('', @actual) eq join('', @expected) or die "collection order mismatch\n";
my @fields = qw(case mode samples median_ns min_ns max_ns native_entries region_entries region_fragments code_lookups code_leases native_instructions interpreted_instructions interpreted_slices guard_exits helper_instructions helper_declines table_reads table_writes upvalue_reads upvalue_writes allocations compilation_requests compilation_failures cache_evictions cache_eviction_refusals code_bytes metadata_bytes metadata_peak_bytes metadata_refusals registration_refusals snapshot_bytes snapshot_peak_bytes);
my %images;
for my $variant (qw(baseline candidate)) {
    open my $hash, '<', "$root/$variant/binary.sha256" or die $!;
    (<$hash> =~ /^([0-9a-f]{64}) /) or die "image hash missing\n";
    $images{$variant} = $1;
}
my (%timings, %counters, %gates);
for my $record (@actual) {
    my ($cpu, $round, $label, $case) = split /\s+/, $record;
    my $row = "$out/cpu$cpu-round$round-$label";
    open my $hash, '<', "$row/launch.sha256" or die $!;
    my $expected_hash = $images{$label eq 'candidate' ? 'candidate' : 'baseline'};
    <$hash> eq "$expected_hash  $root/elapsed/launch/bench\n" or die "launch identity mismatch\n";
    open my $log, '<', "$row/$case.log" or die $!;
    my @log = <$log>;
    my @rows = grep { /^case=/ } @log;
    @rows == 2 or die "workload rows missing\n";
    my %seen;
    for my $line (@rows) {
        my %row;
        for (split /\s+/, $line) {
            my ($key, $value) = split /=/, $_, 2;
            defined $value && !exists $row{$key} or die "invalid workload field\n";
            $row{$key} = $value;
        }
        join(',', sort keys %row) eq join(',', sort @fields) or die "unexpected fields\n";
        $row{case} eq $case && $row{samples} == 51 && $row{mode} =~ /^(off|auto)$/ or die "identity mismatch\n";
        my $mode = $row{mode};
        !$seen{$mode}++ or die "duplicate mode\n";
        for my $key (grep { $_ ne 'mode' && $_ ne 'case' } @fields) {
            $row{$key} =~ /^\d+$/ or die "invalid numeric field\n";
        }
        $row{min_ns} > 0 && $row{min_ns} <= $row{median_ns} && $row{median_ns} <= $row{max_ns} or die "invalid timings\n";
        ($mode eq 'auto' ? $row{native_instructions} > 0 : $row{native_instructions} == 0 && $row{native_entries} == 0) or die "native coverage mismatch\n";
        $timings{$cpu}{$case}{$mode}{$round}{$label} = 0 + $row{median_ns};
        delete @row{qw(min_ns median_ns max_ns)};
        my $encoded = JSON::PP->new->canonical->encode(\%row);
        $counters{$case}{$mode} //= $encoded;
        $counters{$case}{$mode} eq $encoded or die "execution/resource counter mismatch\n";
    }
    my @gate = grep { /^paired_case=/ } @log;
    @gate == 1 or die "missing gate\n";
    my %gate;
    for (split /\s+/, $gate[0]) {
        my ($key, $value) = split /=/, $_, 2;
        defined $value && !exists $gate{$key} or die "gate fields\n";
        $gate{$key} = $value;
    }
    my @gate_fields = qw(paired_case ratio_of_medians median_paired_speedup min_paired_speedup max_paired_speedup minimum_speedup result);
    join(',', sort keys %gate) eq join(',', sort @gate_fields) or die "gate fields\n";
    $gate{paired_case} eq $case && $gate{result} =~ /^(passed|failed)$/ or die "gate identity\n";
    for my $key (grep { $_ ne 'paired_case' && $_ ne 'result' } @gate_fields) {
        $gate{$key} =~ /^\d+\.\d{4}$/ or die "gate numeric field\n";
    }
    my $reported = $gate{result};
    my $ratio = $timings{$cpu}{$case}{off}{$round}{$label} / $timings{$cpu}{$case}{auto}{$round}{$label};
    my $threshold = $case =~ /^(integer_loop|float_loop)$/ ? 2 : $case =~ /^(array_table|closure_upvalue)$/ ? 1.25 : 1 / 1.2;
    $gate{minimum_speedup} eq sprintf('%.4f', $threshold) or die "gate threshold\n";
    abs($gate{ratio_of_medians} - $ratio) <= 0.000050000001 or die "gate ratio\n";
    $gate{min_paired_speedup} > 0 && $gate{min_paired_speedup} <= $gate{median_paired_speedup}
        && $gate{median_paired_speedup} <= $gate{max_paired_speedup} or die "gate dispersion\n";
    $reported eq ($ratio >= $threshold ? 'passed' : 'failed') or die "gate mismatch\n";
    ++$gates{$cpu}{$case}{$label}{$reported};
}
sub median { my @v = sort {$a <=> $b} @_; return ($v[2] + $v[3]) / 2; }
my @results;
for my $cpu (0, 16) {
    for my $case (qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc)) {
        for my $mode (qw(off auto)) {
            my (@ratios, @placebos);
            for my $round (0..5) {
                my $t = $timings{$cpu}{$case}{$mode}{$round};
                push @ratios, sqrt($t->{a} * $t->{b}) / $t->{candidate};
                push @placebos, $t->{a} / $t->{b};
            }
            push @results, {cpu=>$cpu, case=>$case, mode=>$mode, control_over_candidate=>\@ratios,
                median_ratio=>median(@ratios), placebo_ratios=>\@placebos, placebo_median=>median(@placebos)};
        }
    }
}
print JSON::PP->new->canonical->pretty->encode({scope=>'seven-workload-opt3-unstripped-diagnostic', release_acceptance=>JSON::PP::false,
    commands=>scalar(@actual), measured_executions=>scalar(@actual)*2*51, counter_mismatches=>0, results=>\@results, gates=>\%gates});
