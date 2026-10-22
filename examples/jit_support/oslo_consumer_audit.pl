use strict;
use warnings;
use JSON::PP;
use Digest::SHA qw(sha256_hex);

sub bytes {open my $f,'<:raw',$_[0] or die "$!: $_[0]";local $/;return <$f>}
sub median {my @v=sort {$a<=>$b} @_;return ($v[int($#v/2)]+$v[int(@v/2)])/2}
my ($root,$stage)=@ARGV;
defined $stage && $stage=~/^(check|capture)$/ or die "root and stage required\n";
bytes("$root/$stage/completed") eq "complete\n" or die "incomplete consumer collection\n";
bytes("$root/configuration") eq "protocol=oslo-luna-consumer-v1\nprofiles=3,s\nrows=10000\nsamples=11\nwarmups=2\nrounds=6\ncpus=0,16\nthreshold=unscored\n" or die "configuration\n";
my $json=JSON::PP->new->canonical;
my %images;
for my $line (split /\n/,bytes("$root/images.sha256")) {
    $line=~/^([a-f0-9]{64})  images\/(3|s)\/(no-jit|jit)$/ or die "image manifest\n";
    !exists $images{"$2/$3"} && sha256_hex(bytes("$root/images/$2/$3")) eq $1 or die "image identity\n";
    $images{"$2/$3"}=$1;
}
keys(%images)==4 or die "image coverage\n";
my %fixtures;
for my $line (split /\n/,bytes("$root/fixture.sha256")) {
    $line=~/^([a-f0-9]{64})  work\/(rows|boundary)\.json$/ or die "fixture manifest\n";
    !exists $fixtures{$2} && sha256_hex(bytes("$root/work/$2.json")) eq $1 or die "fixture identity\n";
    $fixtures{$2}=$1;
}
keys(%fixtures)==2 or die "fixture coverage\n";
my (%input,%expected);
for my $case (qw(primary row_fields boundary)) {
    my $file=$case eq 'boundary' ? 'boundary' : 'rows';
    $input{$case}=decode_json(bytes("$root/work/$file.json"));
    @{$input{$case}}==10000 or die "input rows\n";
    $expected{$case}=$json->encode([grep {$_->{free}<1000000000 && ($case ne 'row_fields' || $_->{id}%4==0)} @{$input{$case}}]);
}
my @orders=([qw(a off auto b)],[qw(off b a auto)],[qw(auto a b off)],
    [qw(b auto off a)],[qw(a auto off b)],[qw(b off auto a)]);
my @schedule;
for my $profile (3,'s') {
    if ($stage eq 'check') {
        for my $case (qw(primary row_fields boundary)) {push @schedule,[$profile,$case,0,0,0,$_] for qw(a off auto)}
    } else {
        for my $round (0..5) {for my $cpu ($round%2 ? (16,0) : (0,16)) {for my $sample (0..12) {
            push @schedule,[$profile,'primary',$cpu,$round,$sample,$_] for @{$orders[($round+$cpu/16+$sample)%6]};
        }}}
    }
}
my @lines=split /\n/,bytes("$root/$stage/records.jsonl");
@lines==@schedule or die "command coverage\n";
my (%times,%native,$measured,$warmups);
for my $index (0..$#lines) {
    my $record=decode_json($lines[$index]);
    my ($profile,$case,$cpu,$round,$sample,$label)=@{$schedule[$index]};
    my $mode=$label eq 'auto' ? 'auto' : 'off';
    my $variant=$label=~/^(a|b)$/ ? 'no-jit' : 'jit';
    my $image="$root/images/$profile/$variant";
    my $input=$case eq 'boundary' ? 'boundary.json' : 'rows.json';
    my $expression=$case eq 'row_fields' ? 'row.free < 1e9 and row.id % 4 == 0' : 'free < 1e9';
    my $command="cat $input | from json | where \"$expression\" | to json";
    my $prefix=sprintf('%s/%05d',$stage,$index);
    my $warmup=$stage eq 'capture' && $sample<2 ? 1 : 0;
    join(',',sort keys %$record) eq join(',',sort qw(sequence profile case cpu round sample label mode image argv stdout stderr duration_ns status warmup)) or die "record fields\n";
    for my $key (qw(sequence cpu round sample status warmup)) {
        $record->{$key}=~/^(0|[1-9][0-9]*)$/ or die "numeric record\n";
    }
    $record->{sequence}==$index && $record->{profile} eq "$profile" && $record->{case} eq $case
        && $record->{cpu}==$cpu && $record->{round}==$round && $record->{sample}==$sample
        && $record->{label} eq $label && $record->{mode} eq $mode && $record->{image} eq $image
        && $record->{stdout} eq "$prefix.stdout" && $record->{stderr} eq "$prefix.stderr"
        && $record->{status}==0 && $record->{warmup}==$warmup or die "command identity\n";
    $json->encode($record->{argv}) eq $json->encode(['taskset','-c',"$cpu",$image,'-c',$command]) or die "actual argv\n";
    $record->{duration_ns}=~/^[1-9][0-9]*$/ && $record->{duration_ns}<=9007199254740991 or die "duration\n";
    my $actual=decode_json(bytes("$root/$record->{stdout}"));
    $json->encode($actual) eq $expected{$case} or die "selected row values or order\n";
    my $err=bytes("$root/$record->{stderr}");
    $err=~/\Aluna_oslo_consumer_protocol=1 jit_compiled=([01]) rows=10000 kept=([0-9]+) native_entries=([0-9]+) native_instructions=([0-9]+) compilation_requests=([0-9]+) code_bytes=([0-9]+) metadata_bytes=([0-9]+)\n\z/ or die "consumer evidence\n";
    my ($compiled,$kept,$entries,$instructions,$requests,$code,$metadata)=($1,$2,$3,$4,$5,$6,$7);
    $compiled==($variant eq 'jit') && $kept==@$actual or die "consumer identity\n";
    $variant ne 'no-jit' || !$metadata or die "no-JIT metadata\n";
    if ($mode eq 'auto') {$entries>0 && $instructions>0 && $requests>0 && $code>0 or die "missing actual native work\n"}
    else {!$entries && !$instructions && !$requests && !$code or die "unexpected native work\n"}
    $native{$profile}{$label}{instructions}+=$instructions;
    $native{$profile}{$label}{entries}+=$entries;
    if ($warmup) {$warmups++}
    else {$measured++;push @{$times{$profile}{$cpu}{$round}{$label}},$record->{duration_ns}}
}
my @results;
if ($stage eq 'capture') {
    $measured==1056 && $warmups==192 or die "sample coverage\n";
    for my $profile (3,'s') {for my $cpu (0,16) {
        my (@off,@auto,@tier,@placebo);
        for my $round (0..5) {
            my %t;
            for my $label (qw(a off auto b)) {
                @{$times{$profile}{$cpu}{$round}{$label}}==11 or die "median coverage\n";
                $t{$label}=median(@{$times{$profile}{$cpu}{$round}{$label}});
            }
            my $control=sqrt($t{a}*$t{b});
            push @off,$control/$t{off};push @auto,$control/$t{auto};
            push @tier,$t{off}/$t{auto};push @placebo,$t{a}/$t{b};
        }
        push @results,{profile=>"$profile",cpu=>$cpu,control_over_off=>\@off,control_over_auto=>\@auto,
            off_over_auto=>\@tier,placebos=>\@placebo,median_control_over_off=>median(@off),
            median_control_over_auto=>median(@auto),median_off_over_auto=>median(@tier),median_placebo=>median(@placebo)};
    }}
}
print $json->pretty->encode({scope=>'actual isolated Oslo CLI pipeline; not an adopted migration',stage=>$stage,
    commands=>scalar(@lines),measured_commands=>$measured//0,excluded_warmups=>$warmups//0,
    rows_per_process=>10000,raw_process_durations_checked=>JSON::PP::true,process_startup_timed=>JSON::PP::true,
    native_work=>\%native,results=>\@results,numerical_gate=>'unscored',full_jit_acceptance=>JSON::PP::false});
