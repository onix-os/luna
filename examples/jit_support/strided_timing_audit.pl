use strict;
use warnings;
use JSON::PP;

my $directory = shift or die "missing directory\n";
die "unexpected argument\n" if @ARGV;
sub slurp {
    my ($path) = @_;
    open my $file, '<', $path or die "$path: $!\n";
    local $/;
    return <$file>;
}
sub median {
    my @values = sort { $a <=> $b } @_;
    die "empty sample\n" unless @values;
    return ($values[int($#values / 2)] + $values[int(@values / 2)]) / 2;
}
sub distribution {
    my @values = sort { $a <=> $b } @_;
    return { count => scalar @values, median => median(@values),
        minimum => $values[0], maximum => $values[-1],
        p25 => $values[int($#values * .25)], p75 => $values[int($#values * .75)] };
}
die "incomplete screen\n" unless slurp("$directory/completed") eq "complete\n";
my $config = slurp("$directory/configuration");
$config =~ /^protocol=1$/m or die "invalid protocol\n";
my ($cpu) = $config =~ /^cpu=(\d+)$/m;
my ($windows) = $config =~ /^windows=(\d+)$/m;
die "invalid configuration\n" unless defined $cpu && defined $windows && $windows >= 3 && $windows <= 1000;
die "missing final verification\n" unless slurp("$directory/verification-after.log") =~ /: OK\n/;
my ($experiment) = $config =~ /^experiment=(.*)$/m;
$experiment //= 'strided';
my %fields = (strided => 'strided', preferred_entry => 'preferred', callback_scratch => 'scratch');
die "unknown timing experiment\n" unless exists $fields{$experiment};
my $field = $fields{$experiment};
my @cases = $experiment ne 'preferred_entry'
    ? qw(integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc)
    : ('rust_callbacks');
my @orders = ([qw(a candidate b)], [qw(candidate b a)], [qw(b a candidate)],
    [qw(b candidate a)], [qw(candidate a b)], [qw(a b candidate)]);
my $expected_order = '';
for my $window (0 .. $windows - 1) {
    for my $side (qw(before after)) {
        my $telemetry = slurp("$directory/$window-$side.telemetry");
        die "missing telemetry $window/$side\n" unless $telemetry =~ /^cpu\s+\d/m && $telemetry =~ /^some avg10=/m;
    }
    for my $position (0 .. $#cases) {
        my $case = $cases[($position + $window) % @cases];
        $expected_order .= "$window $case $_\n" for @{$orders[$window % @orders]};
    }
}
die "run order differs\n" unless slurp("$directory/order.log") eq $expected_order;
my @reports;
for my $case (@cases) {
    my (@ratios, @placebos, @records, @ordinary, @candidate);
    my $baseline;
    for my $window (0 .. $windows - 1) {
        my %samples;
        for my $label (qw(a candidate b)) {
            my $path = "$directory/$window-$case-$label.log";
            my $log = slurp($path);
            die "failed test $path\n" unless $log =~ /test result: ok\. 1 passed; 0 failed;/;
            my @lines = $log =~ /^(TIMING_V1 .*)$/mg;
            die "invalid record count $path\n" unless @lines == 1;
            my $mode = $label eq 'candidate' ? 'true' : 'false';
            $lines[0] =~ /^TIMING_V1 case=\Q$case\E \Q$field\E=$mode elapsed_ns=([1-9]\d*) counters=\[([\d, ]+)\] code_bytes=(\d+) metadata_bytes=(\d+) metadata_peak_bytes=(\d+) snapshot_peak_bytes=(\d+)$/
                or die "invalid record $path\n";
            my ($ns, $raw, $code, $metadata, $peak, $snapshot) = ($1, $2, $3, $4, $5, $6);
            my @counters = split /,\s*/, $raw;
            die "invalid counters $path\n" unless @counters == 25 && !grep { !/^\d+$/ } @counters;
            my $key = join ',', @counters;
            $baseline //= $key;
            die "counter mismatch $path\n" unless $baseline eq $key;
            die "missing native code $path\n" unless $code > 0 && $counters[11] > 0;
            $samples{$label} = { elapsed_ns => 0 + $ns, counters => [map { 0 + $_ } @counters],
                code_bytes => 0 + $code, metadata_bytes => 0 + $metadata,
                metadata_peak_bytes => 0 + $peak, snapshot_peak_bytes => 0 + $snapshot };
        }
        my $ratio = sqrt($samples{a}{elapsed_ns} * $samples{b}{elapsed_ns}) / $samples{candidate}{elapsed_ns};
        my $placebo = $samples{a}{elapsed_ns} / $samples{b}{elapsed_ns};
        push @ratios, $ratio;
        push @placebos, $placebo;
        push @ordinary, $samples{a}{elapsed_ns}, $samples{b}{elapsed_ns};
        push @candidate, $samples{candidate}{elapsed_ns};
        push @records, { window => $window, samples => \%samples, control_over_candidate => $ratio, control_over_control => $placebo };
    }
    my $ratio = distribution(@ratios);
    my $placebo = distribution(@placebos);
    printf "%s cpu=%d windows=%d control/candidate=%.6f [p25=%.6f p75=%.6f] placebo=%.6f [p25=%.6f p75=%.6f]\n",
        $case, $cpu, $windows, @{$ratio}{qw(median p25 p75)}, @{$placebo}{qw(median p25 p75)};
    push @reports, { case => $case, ratio => $ratio, placebo => $placebo,
        ordinary_ns => distribution(@ordinary), candidate_ns => distribution(@candidate), records => \@records };
}
open my $output, '>', "$directory/audit.json" or die "$!\n";
print $output JSON::PP->new->canonical->pretty->encode({ cpu => 0 + $cpu, windows => 0 + $windows,
    experiment => $experiment,
    scope => 'warm execution only; same test image; external contention retained',
    performance_acceptance => JSON::PP::false, reports => \@reports });
