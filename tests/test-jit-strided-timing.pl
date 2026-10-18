use strict;
use warnings;
use File::Temp qw(tempdir);
use File::Spec;
use File::Copy qw(copy);
use Cwd qw(getcwd);
use JSON::PP;

my ($perl) = File::Spec->file_name_is_absolute($^X) ? ($^X)
    : grep { -f $_ && -x $_ } map { File::Spec->rel2abs(File::Spec->catfile($_, $^X)) } File::Spec->path;
defined $perl or die "cannot resolve Perl interpreter\n";
my $root = tempdir(CLEANUP => 1);
my @cases = qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc);
my @orders = ([qw(a candidate b)], [qw(candidate b a)], [qw(b a candidate)],
    [qw(b candidate a)], [qw(candidate a b)], [qw(a b candidate)]);
sub write_file {
    my ($path, $text) = @_;
    open my $file, '>', $path or die "$path: $!";
    print $file $text;
}
sub fixture {
    my ($name, $experiment, $windows) = @_;
    $windows //= 3;
    my @selected = defined $experiment && $experiment eq 'preferred_entry' ? ('rust_callbacks') : @cases;
    my $field = !defined $experiment || $experiment eq 'strided' ? 'strided'
        : $experiment eq 'preferred_entry' ? 'preferred' : 'scratch';
    my $dir = "$root/$name";
    mkdir $dir or die $!;
    write_file("$dir/configuration", "protocol=1\ncpu=0\nwindows=$windows\n" . (defined $experiment ? "experiment=$experiment\n" : ''));
    my $cross_image = defined $experiment && $experiment eq 'callback_scratch_baseline';
    if ($cross_image) {
        write_file("$dir/configuration", "protocol=1\ncpu=0\nwindows=$windows\nexperiment=$experiment\nimage=/candidate\nbaseline=/baseline\n");
        write_file("$dir/binary.sha256", ('a' x 64) . "  /candidate/test-binary\n" . ('b' x 64) . "  /baseline/test-binary\n");
    }
    write_file("$dir/completed", "complete\n");
    write_file("$dir/verification-after.log", "image: OK\n");
    my $order = '';
    for my $window (0 .. $windows - 1) {
        write_file("$dir/$window-$_.telemetry", "cpu 1 2 3\nsome avg10=4\n") for qw(before after);
        for my $position (0 .. $#selected) {
            my $case = $selected[($position + $window) % @selected];
            for my $label (@{$orders[$window % @orders]}) {
                $order .= "$window $case $label\n";
                my $mode = $label eq 'candidate' ? 'true' : 'false';
                my $ns = $label eq 'candidate' ? 500 : 1000;
                my $counters = join ', ', (1) x 25;
                my $worker_field = $cross_image && $label ne 'candidate' ? 'strided' : $field;
                write_file("$dir/$window-$case-$label.log",
                    "TIMING_V1 case=$case $worker_field=$mode elapsed_ns=$ns counters=[$counters] code_bytes=4096 metadata_bytes=16 metadata_peak_bytes=32 snapshot_peak_bytes=64\ntest result: ok. 1 passed; 0 failed;\n");
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
        exec $perl, 'examples/jit_support/strided_timing_audit.pl', $dir;
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
for my $experiment (qw(strided preferred_entry callback_scratch callback_scratch_baseline)) {
    my $good = fixture("good-$experiment", $experiment, 6);
    die "valid $experiment screen refused" unless audit($good);
    open my $json, '<', "$good/audit.json" or die $!;
    my $report = decode_json(do { local $/; <$json> });
    die 'wrong experiment' unless $report->{experiment} eq $experiment;
    die 'wrong case count' unless @{$report->{reports}} == ($experiment eq 'preferred_entry' ? 1 : 7);
    die 'incorrect acceptance claim' if $report->{performance_acceptance};
    die 'incorrect cross-image scope' if $experiment eq 'callback_scratch_baseline' && $report->{scope} !~ /scratch-free library-test baseline; not production acceptance/;
    for my $case (@{$report->{reports}}) {
        die 'wrong ratios or counts' unless $case->{ratio}{median} == 2 && $case->{placebo}{median} == 1
            && $case->{ordinary_ns}{count} == 12 && $case->{candidate_ns}{count} == 6;
    }
    for my $fault (qw(missing duplicate zero signed mode counters native failed order telemetry incomplete experiment field)) {
        my $dir = fixture("$experiment-$fault", $experiment, 6);
        my $shape = $experiment eq 'preferred_entry' ? 'rust_callbacks' : 'integer_loop';
        my $path = "$dir/0-$shape-candidate.log";
        open my $file, '<', $path or die $!;
        my $log = do { local $/; <$file> };
        close $file;
        if ($fault eq 'missing') { unlink $path or die $!; }
        elsif ($fault eq 'order') { write_file("$dir/order.log", "wrong\n"); }
        elsif ($fault eq 'telemetry') { unlink "$dir/0-before.telemetry" or die $!; }
        elsif ($fault eq 'incomplete') { unlink "$dir/completed" or die $!; }
        elsif ($fault eq 'experiment') { write_file("$dir/configuration", "protocol=1\ncpu=0\nwindows=6\nexperiment=unknown\n"); }
        else {
            if ($fault eq 'duplicate') { $log .= $log; }
            elsif ($fault eq 'zero') { $log =~ s/elapsed_ns=500/elapsed_ns=0/; }
            elsif ($fault eq 'signed') { $log =~ s/elapsed_ns=500/elapsed_ns=-500/; }
            elsif ($fault eq 'mode') { $log =~ s/=true/=false/; }
            elsif ($fault eq 'field') { $log =~ s/(strided|preferred|scratch)=/unknown=/; }
            elsif ($fault eq 'counters') { $log =~ s/counters=\[1/counters=[2/; }
            elsif ($fault eq 'native') { $log =~ s/code_bytes=4096/code_bytes=0/; }
            elsif ($fault eq 'failed') { $log =~ s/0 failed/1 failed/; }
            write_file($path, $log);
        }
        die "accepted $fault" if audit($dir);
        die "wrote success artifact for $fault" if -e "$dir/audit.json";
    }
}
for my $fault (qw(control_field control_mode baseline_identity image_manifest)) {
    my $dir = fixture("cross-$fault", 'callback_scratch_baseline', 6);
    my $path = "$dir/0-integer_loop-a.log";
    if ($fault eq 'baseline_identity') {
        write_file("$dir/configuration", "protocol=1\ncpu=0\nwindows=6\nexperiment=callback_scratch_baseline\nimage=/candidate\nbaseline=/candidate\n");
    } elsif ($fault eq 'image_manifest') {
        write_file("$dir/binary.sha256", ('a' x 64) . "  /candidate/test-binary\n");
    } else {
        open my $input, '<', $path or die $!;
        my $log = do { local $/; <$input> };
        close $input;
        $log =~ s/strided=/scratch=/ if $fault eq 'control_field';
        $log =~ s/=false/=true/ if $fault eq 'control_mode';
        write_file($path, $log);
    }
    die "accepted $fault" if audit($dir);
    die "wrote success artifact for $fault" if -e "$dir/audit.json";
}
print "Timing audit: legacy and four six-order experiments; 56 malformed screens refused\n";

my $image = "$root/runner-image";
my $bin = "$root/runner-bin";
mkdir $image or die $!;
mkdir $bin or die $!;
write_file("$bin/taskset", "#!$perl\n" . <<'TASKSET');
use strict;
use warnings;
die 'affinity option' unless shift(@ARGV) eq '-c';
die 'affinity CPU' unless shift(@ARGV) =~ /^\d+$/;
exec @ARGV;
die $!;
TASKSET
write_file("$image/test-binary", "#!$perl\n" . <<'BINARY');
use strict;
use warnings;
my $preferred = exists $ENV{LUNA_PREFERRED_ENTRY};
my $scratch = exists $ENV{LUNA_MEMORY_SELECTED};
die 'baseline selected experimental worker' if $0 =~ /baseline-image/ && ($scratch || $preferred);
my $case = $ENV{$scratch ? 'LUNA_MEMORY_CASE' : $preferred ? 'LUNA_PREFERRED_CASE' : 'LUNA_STRIDED_CASE'};
my $mode = $ENV{$scratch ? 'LUNA_MEMORY_SELECTED' : $preferred ? 'LUNA_PREFERRED_ENTRY' : 'LUNA_STRIDED'};
my $test = $scratch ? 'strided_profile::timing::time_callback_scratch_runtime'
    : $preferred ? 'preferred_entry::timing::time_preferred_runtime' : 'strided_profile::timing::time_strided_runtime';
die 'test selection' unless $ARGV[0] eq "jit::payload_runtime_tests::$test";
die 'case and mode required' unless defined $case && defined $mode && $mode =~ /^[01]$/;
my $field = $scratch ? 'scratch' : $preferred ? 'preferred' : 'strided';
my $selected = $mode ? 'true' : 'false';
my $ns = $mode ? 500 : 1000;
my $counters = join ', ', (1) x 25;
print "TIMING_V1 case=$case $field=$selected elapsed_ns=$ns counters=[$counters] code_bytes=4096 metadata_bytes=16 metadata_peak_bytes=32 snapshot_peak_bytes=64\n";
print "test result: ok. 1 passed; 0 failed;\n";
BINARY
chmod 0755, "$bin/taskset", "$image/test-binary" or die $!;
open my $checksums, '-|', 'sha256sum', "$image/test-binary" or die $!;
my $manifest = do { local $/; <$checksums> };
close $checksums or die 'checksum failure';
write_file("$image/binary.sha256", $manifest);
write_file("$image/source.sha256", $manifest);
write_file("$image/source-root", getcwd() . "\n");
my $baseline = "$root/baseline-image";
mkdir $baseline or die $!;
copy("$image/test-binary", "$baseline/test-binary") or die $!;
chmod 0755, "$baseline/test-binary" or die $!;
open my $baseline_checksums, '-|', 'sha256sum', "$baseline/test-binary" or die $!;
my $baseline_manifest = do { local $/; <$baseline_checksums> };
close $baseline_checksums or die 'checksum failure';
write_file("$baseline/binary.sha256", $baseline_manifest);
write_file("$baseline/source.sha256", $baseline_manifest);
write_file("$baseline/source-root", getcwd() . "\n");
for my $experiment (qw(strided preferred_entry callback_scratch callback_scratch_baseline)) {
    my $out = "$root/runner-$experiment";
    my $pid = fork();
    die $! unless defined $pid;
    if (!$pid) {
        local $ENV{PATH} = "$bin:$ENV{PATH}";
        delete @ENV{qw(LUNA_STRIDED_CASE LUNA_STRIDED LUNA_PREFERRED_CASE LUNA_PREFERRED_ENTRY LUNA_MEMORY_CASE LUNA_MEMORY_SELECTED)};
        open STDOUT, '>', "$root/runner-$experiment.stdout" or die $!;
        open STDERR, '>', "$root/runner-$experiment.stderr" or die $!;
        exec 'bash', 'examples/jit_support/strided_timing.sh', $image, $out, 0, 3, $experiment,
            ($experiment eq 'callback_scratch_baseline' ? ($baseline) : ());
        die $!;
    }
    waitpid($pid, 0);
    unless ($? == 0) {
        for my $stream (qw(stdout stderr)) {
            open my $log, '<', "$root/runner-$experiment.$stream" or die $!;
            print STDERR do { local $/; <$log> };
        }
        die "runner failed $experiment";
    }
    die "runner output audit failed $experiment" unless audit($out);
}
print "Timing runner: four experiment selectors, telemetry and record checks passed with synthetic workers\n";
for my $fault (qw(missing_root bad_source wrong_binary same_image)) {
    my $out = "$root/rejected-$fault";
    unlink "$baseline/source-root" or die $! if $fault eq 'missing_root';
    write_file("$baseline/source.sha256", ('0' x 64) . "  $baseline/test-binary\n") if $fault eq 'bad_source';
    write_file("$baseline/binary.sha256", $manifest) if $fault eq 'wrong_binary';
    my $pid = fork();
    die $! unless defined $pid;
    if (!$pid) {
        local $ENV{PATH} = "$bin:$ENV{PATH}";
        open STDOUT, '>', "$root/rejected-$fault.stdout" or die $!;
        open STDERR, '>', "$root/rejected-$fault.stderr" or die $!;
        exec 'bash', 'examples/jit_support/strided_timing.sh', $image, $out, 0, 3,
            'callback_scratch_baseline', ($fault eq 'same_image' ? $image : $baseline);
        die $!;
    }
    waitpid($pid, 0);
    die "runner accepted $fault" if $? == 0 || -e "$out/completed" || -e "$out/order.log";
    write_file("$baseline/source-root", getcwd() . "\n");
    write_file("$baseline/source.sha256", $baseline_manifest);
    write_file("$baseline/binary.sha256", $baseline_manifest);
}
print "Timing runner: four invalid cross-image inputs refused before sampling\n";
