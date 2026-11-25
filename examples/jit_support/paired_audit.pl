use strict;
use warnings;
use JSON::PP;

my @summary_keys = qw(case mode samples median_ns min_ns max_ns native_entries region_entries region_fragments code_lookups code_leases native_instructions interpreted_instructions interpreted_slices guard_exits helper_instructions helper_declines table_reads table_writes upvalue_reads upvalue_writes allocations compilation_requests compilation_failures cache_evictions cache_eviction_refusals code_bytes metadata_bytes metadata_peak_bytes metadata_refusals registration_refusals snapshot_bytes snapshot_peak_bytes);
my @gate_keys = qw(paired_case ratio_of_medians median_paired_speedup min_paired_speedup max_paired_speedup minimum_speedup result);
my %limits = (integer_loop=>2, float_loop=>2, array_table=>1.25, closure_upvalue=>1.25,
    polymorphic_metamethod=>1/1.2, rust_callbacks=>1/1.2, allocation_gc=>1/1.2,
    oslo_predicate=>undef, cold_config=>1/1.15);

sub fields {
    my ($line, @keys) = @_;
    my %fields;
    for (split / /, $line) {
        /^([^=]+)=(\S+)$/ && !exists $fields{$1} or die "invalid or duplicate field\n";
        $fields{$1} = $2;
    }
    join(',', sort keys %fields) eq join(',', sort @keys) or die "unexpected fields\n";
    return \%fields;
}

sub upper_median { my @v = sort {$a<=>$b} @_; return $v[int(@v/2)]; }
sub rounded {
    my ($printed, $actual) = @_;
    $printed =~ /^\d+\.\d{4}$/ && abs($printed-$actual) <= 0.000050000001
        or die "rounded statistic mismatch\n";
}

sub audit {
    my ($path) = @_;
    open my $file, '<', $path or die "$path: $!";
    my (%cases, @order, $current, $phase);
    while (my $line = <$file>) {
        chomp $line;
        if ($line =~ /^paired_sample_case=/) {
            my $row = fields($line, qw(paired_sample_case sample first off_ns auto_ns));
            my $name = $row->{paired_sample_case};
            exists $limits{$name} or die "unknown workload\n";
            if (!defined $current) {
                !exists $cases{$name} or die "duplicate workload\n";
                $current = $name;
                $phase = 'samples';
                push @order, $name;
                $cases{$name} = {off=>[], auto=>[]};
            }
            $current eq $name && $phase eq 'samples' or die "sample ordering\n";
            my $sample = @{$cases{$name}{off}};
            $row->{sample} =~ /^(0|[1-9][0-9]*)$/ && $row->{sample} == $sample
                && $row->{first} eq ($sample%2 ? 'auto' : 'off') or die "sample ordering\n";
            for my $mode (qw(off auto)) {
                $row->{$mode.'_ns'} =~ /^[1-9][0-9]*$/ or die "invalid sample duration\n";
                push @{$cases{$name}{$mode}}, 0+$row->{$mode.'_ns'};
            }
        } elsif ($line =~ /^case=/) {
            defined $current or die "summary lacks raw samples\n";
            my $row = fields($line, @summary_keys);
            my $mode = $phase eq 'samples' ? 'off' : $phase eq 'off' ? 'auto' : '';
            $row->{case} eq $current && $row->{mode} eq $mode or die "summary ordering\n";
            for my $key (grep {$_ ne 'case' && $_ ne 'mode'} @summary_keys) {
                $row->{$key} =~ /^(0|[1-9][0-9]*)$/ or die "invalid summary number\n";
            }
            my @raw = sort {$a<=>$b} @{$cases{$current}{$mode}};
            @raw >= 3 && @raw <= 1000 && $row->{samples} == @raw
                && $row->{median_ns} == upper_median(@raw)
                && $row->{min_ns} == $raw[0] && $row->{max_ns} == $raw[-1]
                or die "raw timing summary mismatch\n";
            if ($mode eq 'off' || $current eq 'cold_config') {
                !$row->{native_entries} && !$row->{native_instructions} or die "unexpected native work\n";
            } else {
                $row->{native_entries} > 0 && $row->{native_instructions} > 0 or die "missing native work\n";
            }
            $phase = $mode;
        } elsif ($line =~ /^paired_case=/) {
            defined $current && $phase eq 'auto' or die "gate ordering\n";
            my $gate = fields($line, @gate_keys);
            $gate->{paired_case} eq $current or die "gate workload\n";
            my $case = $cases{$current};
            my @ratios = sort {$a<=>$b} map {$case->{off}[$_] / $case->{auto}[$_]} 0..$#{$case->{off}};
            my $ratio = upper_median(@{$case->{off}}) / upper_median(@{$case->{auto}});
            rounded($gate->{ratio_of_medians}, $ratio);
            rounded($gate->{median_paired_speedup}, upper_median(@ratios));
            rounded($gate->{min_paired_speedup}, $ratios[0]);
            rounded($gate->{max_paired_speedup}, $ratios[-1]);
            my $minimum = $limits{$current};
            $gate->{minimum_speedup} eq (defined $minimum ? sprintf('%.4f', $minimum) : 'unscored')
                or die "gate threshold\n";
            my $result = !defined $minimum ? 'unscored' : $ratio >= $minimum ? 'passed' : 'failed';
            $gate->{result} eq $result or die "gate result\n";
            $case->{samples} = scalar @ratios;
            $case->{ratio_of_medians} = $ratio;
            $case->{result} = $result;
            $current = undef;
        }
    }
    close $file or die "$path: $!";
    @order && !defined $current or die "incomplete paired capture\n";
    return {path=>$path, cases=>[map {{case=>$_, %{$cases{$_}}}} @order]};
}

@ARGV or die "usage: paired_audit.pl LOG...\n";
my @logs = map {audit($_)} @ARGV;
print JSON::PP->new->canonical->pretty->encode({native_raw_medians_recomputed=>JSON::PP::true,
    native_paired_dispersion_recomputed=>JSON::PP::true,
    independent_runtime_counter_oracle=>JSON::PP::false,
    full_performance_acceptance=>JSON::PP::false, logs=>\@logs});
