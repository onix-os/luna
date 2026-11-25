use strict;
use warnings;
use File::Temp qw(tempdir);
use File::Find;
use File::Path qw(make_path);
use File::Basename qw(dirname);
use File::Copy qw(copy);
use File::Spec;
use JSON::PP;

my ($root) = @ARGV;
my $auditor = File::Spec->rel2abs('examples/jit_support/elapsed_audit.pl');
sub write_file {
    my ($path, $text) = @_;
    make_path(dirname($path));
    open my $f, '>', $path or die $!;
    print {$f} $text;
    close $f or die $!;
}
sub read_file {
    open my $f, '<', $_[0] or die $!;
    local $/;
    return <$f>;
}
if (!defined $root) {
    $root = tempdir(CLEANUP => 1);
    my @cases = qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc);
    my @orders = ([qw(a candidate b)], [qw(candidate b a)], [qw(b a candidate)], [qw(b candidate a)], [qw(candidate a b)], [qw(a b candidate)]);
    my @fields = qw(case mode samples median_ns min_ns max_ns native_entries region_entries region_fragments code_lookups code_leases native_instructions interpreted_instructions interpreted_slices guard_exits helper_instructions helper_declines table_reads table_writes upvalue_reads upvalue_writes allocations compilation_requests compilation_failures cache_evictions cache_eviction_refusals code_bytes metadata_bytes metadata_peak_bytes metadata_refusals registration_refusals snapshot_bytes snapshot_peak_bytes);
    for my $variant (qw(baseline candidate)) {
        my $hash = ($variant eq 'baseline' ? 'a' : 'b') x 64;
        write_file("$root/$variant/binary.sha256", "$hash  $root/$variant/jit_bench\n");
    }
    my $order = '';
    for my $round (0..5) {
        for my $cpu ($round % 2 ? (16,0) : (0,16)) {
            for my $label (@{$orders[($round + $cpu/16) % 6]}) {
                my $row = "$root/elapsed/cpu$cpu-round$round-$label";
                my $hash = ($label eq 'candidate' ? 'b' : 'a') x 64;
                write_file("$row/launch.sha256", "$hash  $root/elapsed/launch/bench\n");
                for my $case ($round % 2 ? reverse(@cases) : @cases) {
                    $order .= "$cpu $round $label $case\n";
                    my $log = '';
                    for my $mode (qw(off auto)) {
                        my %values = map { $_ => 0 } @fields;
                        @values{qw(case mode samples)} = ($case, $mode, 51);
                        my $time = $mode eq 'off' ? 200000 : 100000;
                        @values{qw(median_ns min_ns max_ns)} = ($time, $time, $time);
                        @values{qw(native_entries native_instructions)} = $mode eq 'off' ? (0,0) : (1,100);
                        $values{interpreted_instructions} = 100;
                        $log .= join(' ', map { "$_=$values{$_}" } @fields)."\n";
                    }
                    my $threshold = $case =~ /^(integer_loop|float_loop)$/ ? 2 : $case =~ /^(array_table|closure_upvalue)$/ ? 1.25 : 1/1.2;
                    $log .= sprintf("paired_case=%s ratio_of_medians=2.0000 median_paired_speedup=2.0000 min_paired_speedup=2.0000 max_paired_speedup=2.0000 minimum_speedup=%.4f result=passed\n", $case, $threshold);
                    write_file("$row/$case.log", $log);
                }
            }
        }
    }
    write_file("$root/elapsed/order.log", $order);
    write_file("$root/elapsed/completed", "complete\n");
}
my $fixture = tempdir(CLEANUP => 1);
find({no_chdir=>1, wanted=>sub {
    my $relative = substr($File::Find::name, length("$root/elapsed"));
    return if $relative =~ m{^/launch(?:/|$)};
    if (-d $File::Find::name) { make_path("$fixture$relative"); }
    elsif (-f $File::Find::name) { copy($File::Find::name, "$fixture$relative") or die $!; }
}}, "$root/elapsed");
sub run {
    my $pid = fork();
    defined $pid or die $!;
    if (!$pid) {
        open STDOUT, '>', "$fixture/out" or die $!;
        open STDERR, '>', "$fixture/error" or die $!;
        exec $^X, $auditor, $root, $fixture;
        die $!;
    }
    waitpid($pid, 0);
    return $?;
}
sub accepted {
    run() == 0 or die "valid fixture rejected: ".read_file("$fixture/error");
    my $result = decode_json(read_file("$fixture/out"));
    $result->{commands} == 252 && $result->{measured_executions} == 25704
        && @{$result->{results}} == 28 && !$result->{counter_mismatches} or die "result scope\n";
}
accepted();
my $log = 'cpu0-round0-candidate/rust_callbacks.log';
my $array = 'cpu0-round0-candidate/array_table.log';
my @mutations = (
    ['order.log', sub { s/^.*\n// }, qr/collection order mismatch/],
    ['cpu0-round0-candidate/launch.sha256', sub { s/^[0-9a-f]{64}/'0'x64/e }, qr/launch identity mismatch/],
    [$log, sub { s/samples=51 /samples=50 / }, qr/identity mismatch/],
    [$log, sub { s/^(case=.*mode=auto .*?)native_instructions=\d+ /${1}native_instructions=0 /m }, qr/native coverage mismatch/],
    [$log, sub { s/^(case=.*?)snapshot_peak_bytes=(\d+)/$1.'snapshot_peak_bytes='.($2+1)/me }, qr/execution.resource counter mismatch/],
    [$log, sub { s/^(case=.*)/$1 samples=51/m }, qr/invalid workload field/],
    [$log, sub { s/^(case=.*mode=off .*)\n//m }, qr/workload rows missing/],
    [$log, sub { s/^(paired_case=.*result=)(passed|failed)/$1.($2 eq 'passed' ? 'failed' : 'passed')/me }, qr/gate mismatch/],
    [$array, sub { s/minimum_speedup=1\.2500/minimum_speedup=1.0500/ }, qr/gate threshold/],
    [$log, sub { s/ minimum_speedup=\S+// }, qr/gate fields/],
    [$log, sub { s/^(paired_case=.*)/$1 result=passed/m }, qr/gate fields/],
    [$log, sub { s/^(paired_case=.*)/$1 unknown=1/m }, qr/gate fields/],
    [$log, sub { s/ratio_of_medians=\S+/ratio_of_medians=0.0000/ }, qr/gate ratio/],
    [$log, sub { s/median_paired_speedup=\S+/median_paired_speedup=-1.0000/ }, qr/gate numeric field/],
    [$log, sub { s/min_paired_speedup=\S+/min_paired_speedup=999999.0000/ }, qr/gate dispersion/],
    [$log, sub { s/max_paired_speedup=\S+/max_paired_speedup=nan/ }, qr/gate numeric field/],
    [$log, sub { s/paired_case=rust_callbacks/paired_case=array_table/ }, qr/gate identity/],
);
for my $mutation (@mutations) {
    my ($relative, $mutate, $expected) = @$mutation;
    my $path = "$fixture/$relative";
    my $original = read_file($path);
    local $_ = $original;
    $mutate->() or die "mutation did not apply: $relative\n";
    write_file($path, $_);
    run() != 0 && read_file("$fixture/error") =~ $expected or die "wrong refusal: $relative\n";
    write_file($path, $original);
}
my $original = read_file("$fixture/$array");
for my $numerator (110000, 124990, 125000, 125010) {
    my @lines = split /\n/, $original;
    for (@lines) {
        if (/^case=/) {
            my $time = /mode=off / ? $numerator : 100000;
            s/(median_ns|min_ns|max_ns)=\d+/$1=$time/g;
        } elsif (/^paired_case=/) {
            my $ratio = $numerator / 100000;
            $_ = sprintf('paired_case=array_table ratio_of_medians=%.4f median_paired_speedup=%.4f min_paired_speedup=%.4f max_paired_speedup=%.4f minimum_speedup=1.2500 result=%s', ($ratio)x4, $numerator >= 125000 ? 'passed' : 'failed');
        }
    }
    write_file("$fixture/$array", join("\n", @lines)."\n");
    accepted();
}
write_file("$fixture/$array", $original);
unlink "$fixture/completed" or die $!;
run() != 0 && read_file("$fixture/error") =~ /incomplete collection/ or die "incomplete fixture accepted\n";
write_file("$fixture/completed", "complete\n");
accepted();
printf "elapsed audit: valid/restored fixtures, four table boundaries and %d targeted refusals passed\n", scalar(@mutations)+1;
