use strict;
use warnings;
use File::Temp qw(tempdir);
use File::Spec;
use JSON::PP;

my $directory = tempdir(CLEANUP=>1);
my $auditor = File::Spec->rel2abs('examples/jit_support/paired_audit.pl');
my @keys = qw(case mode samples median_ns min_ns max_ns native_entries region_entries region_fragments code_lookups code_leases native_instructions interpreted_instructions interpreted_slices guard_exits helper_instructions helper_declines table_reads table_writes upvalue_reads upvalue_writes allocations compilation_requests compilation_failures cache_evictions cache_eviction_refusals code_bytes metadata_bytes metadata_peak_bytes metadata_refusals registration_refusals snapshot_bytes snapshot_peak_bytes);
my @cases = qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc oslo_predicate cold_config);
sub upper { my @v = sort {$a<=>$b} @_; return $v[int(@v/2)]; }
sub fixture {
    my ($samples, $off_base, $constant) = @_;
    my $log = '';
    for my $name (@cases) {
        my (@off, @auto);
        for my $sample (0..$samples-1) {
            push @off, $off_base + ($constant ? 0 : 701*$sample);
            push @auto, 100000 + ($constant ? 0 : 307*($samples-$sample));
            $log .= sprintf("paired_sample_case=%s sample=%d first=%s off_ns=%d auto_ns=%d\n",
                $name, $sample, $sample%2 ? 'auto' : 'off', $off[-1], $auto[-1]);
        }
        for my $mode (qw(off auto)) {
            my @times = sort {$a<=>$b} ($mode eq 'off' ? @off : @auto);
            my %row = map {$_=>0} @keys;
            @row{qw(case mode samples median_ns min_ns max_ns)} = ($name, $mode, $samples, upper(@times), $times[0], $times[-1]);
            @row{qw(native_entries native_instructions)} = $mode eq 'auto' && $name ne 'cold_config' ? (1,100) : (0,0);
            $log .= join(' ', map {"$_=$row{$_}"} @keys)."\n";
        }
        my $minimum = $name =~ /^(integer_loop|float_loop)$/ ? 2 : $name =~ /^(array_table|closure_upvalue)$/ ? 1.25 : $name eq 'cold_config' ? 1/1.15 : 1/1.2;
        my $ratio = upper(@off)/upper(@auto);
        my @ratios = sort {$a<=>$b} map {$off[$_]/$auto[$_]} 0..$#off;
        $log .= sprintf("paired_case=%s ratio_of_medians=%.4f median_paired_speedup=%.4f min_paired_speedup=%.4f max_paired_speedup=%.4f minimum_speedup=%s result=%s\n",
            $name, $ratio, upper(@ratios), $ratios[0], $ratios[-1],
            $name eq 'oslo_predicate' ? 'unscored' : sprintf('%.4f',$minimum),
            $name eq 'oslo_predicate' ? 'unscored' : $ratio >= $minimum ? 'passed' : 'failed');
    }
    return $log;
}
sub write_log {
    open my $f, '>', "$directory/capture.log" or die $!;
    print {$f} $_[0];
    close $f or die $!;
}
sub read_file { open my $f, '<', $_[0] or die $!; local $/; return <$f>; }
sub run {
    my $pid = fork();
    defined $pid or die $!;
    if (!$pid) {
        open STDOUT, '>', "$directory/result.json" or die $!;
        open STDERR, '>', "$directory/error" or die $!;
        exec $^X, $auditor, "$directory/capture.log";
        die $!;
    }
    waitpid($pid,0);
    return $?;
}
sub accepted {
    run()==0 or die 'valid capture rejected: '.read_file("$directory/error");
    my $result = decode_json(read_file("$directory/result.json"));
    $result->{native_raw_medians_recomputed} && $result->{native_paired_dispersion_recomputed}
        && !$result->{full_performance_acceptance} && @{$result->{logs}[0]{cases}}==9
        or die "audit scope\n";
}
for my $samples (3,4,11,1000) {
    write_log(fixture($samples,200000));
    accepted();
}
my $valid = fixture(11,200000);
my @mutations = (
    [sub {s/^(paired_sample_case=.*\n)//m}, qr/sample ordering/],
    [sub {s/ sample=1 / sample=0 /}, qr/sample ordering/],
    [sub {s/ sample=0 first=off/ sample=0 first=auto/}, qr/sample ordering/],
    [sub {s/ off_ns=200000/ off_ns=0/}, qr/invalid sample duration/],
    [sub {s/ auto_ns=103377/ auto_ns=nan/}, qr/invalid sample duration/],
    [sub {s/(paired_sample_case=[^\n]+)/$1 off_ns=200000/}, qr/duplicate field/],
    [sub {s/ sample=5 / sample=05 /}, qr/sample ordering/],
    [sub {s/samples=11 /samples=10 /}, qr/raw timing summary mismatch/],
    [sub {s/median_ns=203505/median_ns=203506/}, qr/raw timing summary mismatch/],
    [sub {s/min_ns=200000/min_ns=200001/}, qr/raw timing summary mismatch/],
    [sub {s/max_ns=207010/max_ns=207011/}, qr/raw timing summary mismatch/],
    [sub {s/median_paired_speedup=\S+/median_paired_speedup=1.0000/}, qr/rounded statistic/],
    [sub {s/min_paired_speedup=\S+/min_paired_speedup=1.0000/}, qr/rounded statistic/],
    [sub {s/max_paired_speedup=\S+/max_paired_speedup=1.0000/}, qr/rounded statistic/],
    [sub {s/ratio_of_medians=\S+/ratio_of_medians=1.0000/}, qr/rounded statistic/],
    [sub {s/minimum_speedup=1.2500/minimum_speedup=1.0500/}, qr/gate threshold/],
    [sub {s/minimum_speedup=unscored/minimum_speedup=1.0000/}, qr/gate threshold/],
    [sub {s/result=failed/result=passed/}, qr/gate result/],
    [sub {s/^(case=.*mode=auto .*?)native_instructions=100 /${1}native_instructions=0 /m}, qr/missing native work/],
    [sub {s/^(paired_case=.*\n)//m}, qr/sample ordering/],
    [sub {s/^(paired_case=cold_config.*\n)//m}, qr/incomplete paired capture/],
    [sub {s/^(case=.*\n)//m}, qr/summary ordering/],
    [sub {$_=''; 1}, qr/incomplete paired capture/],
);
for my $mutation (@mutations) {
    local $_ = $valid;
    $mutation->[0]->() or die "mutation did not apply\n";
    write_log($_);
    run()!=0 && read_file("$directory/error") =~ $mutation->[1]
        or die 'incorrect refusal: '.read_file("$directory/error");
}
for my $off (124999,125000,125001) {
    my $boundary = fixture(3,$off,1);
    $boundary =~ s/^(?!.*(?:array_table)).*\n//mg;
    write_log($boundary);
    run()==0 or die 'boundary refusal: '.read_file("$directory/error");
    my $result = decode_json(read_file("$directory/result.json"));
    $result->{logs}[0]{cases}[0]{result} eq ($off>=125000 ? 'passed' : 'failed')
        or die "unrounded threshold mismatch\n";
}
write_log($valid);
accepted();
printf "paired raw audit: odd/even/maximum samples, nine workloads, three boundaries, valid/restored and %d refusals passed\n", scalar @mutations;
