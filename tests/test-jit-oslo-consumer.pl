use strict;
use warnings;
use JSON::PP;
use File::Temp qw(tempdir);
use File::Path qw(make_path);
use File::Spec;
use Digest::SHA qw(sha256_hex);
my $root=tempdir(CLEANUP=>1);
my $audit=File::Spec->rel2abs('examples/jit_support/oslo_consumer_audit.pl');
my $json=JSON::PP->new->canonical;
sub write_file {open my $f,'>:raw',"$root/$_[0]" or die $!;print {$f} $_[1];close $f or die $!}
sub bytes {open my $f,'<:raw',"$root/$_[0]" or die $!;local $/;return <$f>}
make_path("$root/images/3","$root/images/s","$root/work","$root/check");
write_file('configuration',"protocol=oslo-luna-consumer-v1\nprofiles=3,s\nrows=10000\nsamples=11\nwarmups=2\nrounds=6\ncpus=0,16\nthreshold=unscored\n");
my $manifest='';
for my $opt (3,'s') {for my $variant ('no-jit','jit') {
    my $path="images/$opt/$variant";
    write_file($path,"fixture-$opt-$variant");
    $manifest.=sha256_hex(bytes($path))."  $path\n";
}}
write_file('images.sha256',$manifest);
my %rows;
$rows{rows}=[map {{id=>$_,free=>($_%2 ? 1500000000 : 500000000),mount=>'/test',used=>123456789+$_}} 0..9999];
$rows{boundary}=[map {{id=>$_,free=>(999999999,1000000000,1000000001)[$_%3],mount=>'/test',used=>123456789+$_}} 0..9999];
$manifest='';
for my $name (qw(rows boundary)) {
    write_file("work/$name.json",$json->encode($rows{$name}));
    $manifest.=sha256_hex(bytes("work/$name.json"))."  work/$name.json\n";
}
write_file('fixture.sha256',$manifest);
my ($sequence,$records)=(0,'');
for my $profile (3,'s') {for my $case (qw(primary row_fields boundary)) {for my $label (qw(a off auto)) {
    my $mode=$label eq 'auto' ? 'auto' : 'off';
    my $variant=$label eq 'a' ? 'no-jit' : 'jit';
    my $input=$case eq 'boundary' ? 'boundary' : 'rows';
    my $expression=$case eq 'row_fields' ? 'row.free < 1e9 and row.id % 4 == 0' : 'free < 1e9';
    my $image="$root/images/$profile/$variant";
    my $prefix=sprintf('check/%05d',$sequence);
    my @kept=grep {$_->{free}<1000000000 && ($case ne 'row_fields' || $_->{id}%4==0)} @{$rows{$input}};
    write_file("$prefix.stdout",$json->encode(\@kept));
    my ($entries,$instructions,$requests,$code)=$mode eq 'auto' ? (10,30,1,4096) : (0,0,0,0);
    my $compiled=$variant eq 'jit' ? 1 : 0;
    my $metadata=$compiled ? 432 : 0;
    write_file("$prefix.stderr","luna_oslo_consumer_protocol=1 jit_compiled=$compiled rows=10000 kept=".scalar(@kept)." native_entries=$entries native_instructions=$instructions compilation_requests=$requests code_bytes=$code metadata_bytes=$metadata\n");
    $records.=$json->encode({sequence=>$sequence++,profile=>"$profile",case=>$case,cpu=>0,round=>0,sample=>0,label=>$label,mode=>$mode,image=>$image,
        argv=>['taskset','-c','0',$image,'-c',"cat $input.json | from json | where \"$expression\" | to json"],
        stdout=>"$prefix.stdout",stderr=>"$prefix.stderr",duration_ns=>100000,status=>0,warmup=>0})."\n";
}}}
write_file('check/records.jsonl',$records);
write_file('check/completed',"complete\n");
sub run {
    my $pid=fork();defined $pid or die $!;
    if (!$pid) {
        open STDOUT,'>',"$root/out.json" or die $!;
        open STDERR,'>',"$root/error" or die $!;
        exec $^X,$audit,$root,'check';die $!;
    }
    waitpid($pid,0);return $?;
}
run()==0 or die 'valid consumer fixture rejected: '.bytes('error');
my @mutations=(
    ['check/completed',sub {s/complete/unfinished/}],
    ['configuration',sub {s/threshold=unscored/threshold=passed/}],
    ['images/3/jit',sub {$_ .= 'changed';1}],
    ['work/rows.json',sub {s/123456789/123456788/}],
    ['check/00000.stdout',sub {s/123456789/123456788/}],
    ['check/00000.stdout',sub {s/"id":0/"id":2/}],
    ['check/00002.stderr',sub {s/native_instructions=30/native_instructions=0/}],
    ['check/00001.stderr',sub {s/native_entries=0/native_entries=1/}],
    ['check/00000.stderr',sub {s/jit_compiled=0/jit_compiled=1/}],
    ['check/00000.stderr',sub {s/kept=5000/kept=4999/}],
    ['check/00000.stderr',sub {s/metadata_bytes=0/metadata_bytes=1/}],
    ['check/records.jsonl',sub {s/^.*\n//}],
    ['check/records.jsonl',sub {s/"duration_ns":100000/"duration_ns":0/}],
    ['check/records.jsonl',sub {s/"duration_ns":100000/"duration_ns":9007199254740992/}],
    ['check/records.jsonl',sub {s/"cpu":0/"cpu":"0junk"/}],
    ['check/records.jsonl',sub {s/"status":0/"status":1/}],
    ['check/records.jsonl',sub {s/"sequence":0/"sequence":1/}],
    ['check/records.jsonl',sub {s/where/where_wrong/}],
);
for my $mutation (@mutations) {
    my ($path,$mutate)=@$mutation;
    my $original=bytes($path);
    local $_=$original;$mutate->() or die "mutation did not apply: $path\n";
    write_file($path,$_);
    run()!=0 or die "accepted inconsistent consumer data: $path\n";
    write_file($path,$original);
}
run()==0 or die 'restored consumer fixture rejected: '.bytes('error');
my $result=decode_json(bytes('out.json'));
$result->{commands}==18 && !$result->{full_jit_acceptance} && $result->{numerical_gate} eq 'unscored' or die 'scope';
printf "Oslo consumer audit: 18 CLI-shaped records, valid/restored and %d refusal cases passed\n",scalar(@mutations);
