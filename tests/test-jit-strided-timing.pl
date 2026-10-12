use strict;
use warnings;
use File::Temp qw(tempdir);
use JSON::PP;

my $root = tempdir(CLEANUP => 1);
my @cases = qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc);
my @orders = ([qw(a candidate b)], [qw(candidate b a)], [qw(b a candidate)]);
sub write_file {
    my ($path, $text) = @_;
    open my $file, '>', $path or die "$path: $!";
    print $file $text;
}
sub fixture {
    my ($name) = @_;
    my $dir = "$root/$name";
    mkdir $dir or die $!;
    write_file("$dir/configuration", "protocol=1\ncpu=0\nwindows=3\n");
    write_file("$dir/completed", "complete\n");
    write_file("$dir/verification-after.log", "image: OK\n");
    my $order = '';
    for my $window (0 .. 2) {
        write_file("$dir/$window-$_.telemetry", "cpu 1 2 3\nsome avg10=4\n") for qw(before after);
        for my $position (0 .. $#cases) {
            my $case = $cases[($position + $window) % @cases];
            for my $label (@{$orders[$window]}) {
                $order .= "$window $case $label\n";
                my $mode = $label eq 'candidate' ? 'true' : 'false';
                my $ns = $label eq 'candidate' ? 500 : 1000;
                my $counters = join ', ', (1) x 25;
                write_file("$dir/$window-$case-$label.log",
                    "TIMING_V1 case=$case strided=$mode elapsed_ns=$ns counters=[$counters] code_bytes=4096 metadata_bytes=16 metadata_peak_bytes=32 snapshot_peak_bytes=64\ntest result: ok. 1 passed; 0 failed;\n");
            }
        }
    }
    write_file("$dir/order.log", $order);
    return $dir;
}
sub audit {
    my ($dir) = @_;
    my $pid = fork();
    die $! unless defined $pid;
    if (!$pid) {
        open STDOUT, '>', "$dir/audit.stdout" or die $!;
        open STDERR, '>', "$dir/audit.stderr" or die $!;
        exec $^X, 'examples/jit_support/strided_timing_audit.pl', $dir;
        die $!;
    }
    waitpid($pid, 0);
    return $? == 0;
}
my $good = fixture('good');
die 'valid screen refused' unless audit($good);
open my $json, '<', "$good/audit.json" or die $!;
my $report = decode_json(do { local $/; <$json> });
die 'wrong report count' unless @{$report->{reports}} == 7;
die 'incorrect acceptance claim' if $report->{performance_acceptance};
for my $case (@{$report->{reports}}) {
    die 'wrong ratios' unless $case->{ratio}{median} == 2 && $case->{placebo}{median} == 1;
    die 'wrong sample count' unless $case->{ordinary_ns}{count} == 6 && $case->{candidate_ns}{count} == 3;
}
for my $fault (qw(missing duplicate zero signed mode counters native failed order telemetry incomplete)) {
    my $dir = fixture($fault);
    my $path = "$dir/0-integer_loop-candidate.log";
    open my $file, '<', $path or die $!;
    my $log = do { local $/; <$file> };
    close $file;
    if ($fault eq 'missing') { unlink $path or die $!; }
    elsif ($fault eq 'order') { write_file("$dir/order.log", "wrong\n"); }
    elsif ($fault eq 'telemetry') { unlink "$dir/0-before.telemetry" or die $!; }
    elsif ($fault eq 'incomplete') { unlink "$dir/completed" or die $!; }
    else {
        if ($fault eq 'duplicate') { $log .= $log; }
        elsif ($fault eq 'zero') { $log =~ s/elapsed_ns=500/elapsed_ns=0/; }
        elsif ($fault eq 'signed') { $log =~ s/elapsed_ns=500/elapsed_ns=-500/; }
        elsif ($fault eq 'mode') { $log =~ s/strided=true/strided=false/; }
        elsif ($fault eq 'counters') { $log =~ s/counters=\[1/counters=[2/; }
        elsif ($fault eq 'native') { $log =~ s/code_bytes=4096/code_bytes=0/; }
        elsif ($fault eq 'failed') { $log =~ s/0 failed/1 failed/; }
        write_file($path, $log);
    }
    die "accepted $fault" if audit($dir);
    die "wrote success artifact for $fault" if -e "$dir/audit.json";
}
print "Strided timing audit: valid ratios and eleven malformed screens checked\n";
