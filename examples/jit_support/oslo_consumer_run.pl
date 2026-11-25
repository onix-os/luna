use strict;
use warnings;
use JSON::PP;
use Time::HiRes qw(clock_gettime CLOCK_MONOTONIC);
use POSIX qw(WIFEXITED WEXITSTATUS);

my ($root,$stage)=@ARGV;
defined $stage && $stage=~/^(check|capture)$/ or die "root and stage required\n";
my $json=JSON::PP->new->canonical;
open my $records,'>',"$root/$stage/records.jsonl" or die $!;
my @orders=([qw(a off auto b)],[qw(off b a auto)],[qw(auto a b off)],
    [qw(b auto off a)],[qw(a auto off b)],[qw(b off auto a)]);
my $sequence=0;

sub run {
    my ($profile,$case,$cpu,$round,$sample,$label)=@_;
    my $mode=$label eq 'auto' ? 'auto' : 'off';
    my $variant=$label=~/^(a|b)$/ ? 'no-jit' : 'jit';
    my $image="$root/images/$profile/$variant";
    my $input=$case eq 'boundary' ? 'boundary.json' : 'rows.json';
    my $expression=$case eq 'row_fields' ? 'row.free < 1e9 and row.id % 4 == 0' : 'free < 1e9';
    my $command="cat $input | from json | where \"$expression\" | to json";
    my $name=sprintf('%05d',$sequence++);
    my $stdout="$stage/$name.stdout";
    my $stderr="$stage/$name.stderr";
    my $started=clock_gettime(CLOCK_MONOTONIC);
    my $pid=fork();
    defined $pid or die $!;
    if (!$pid) {
        %ENV=(PATH=>$ENV{PATH},HOME=>"$root/home",XDG_CONFIG_HOME=>"$root/home/config",
            XDG_CACHE_HOME=>"$root/home/cache",XDG_DATA_HOME=>"$root/home/data",
            XDG_RUNTIME_DIR=>"$root/home/runtime",TERM=>'dumb',LC_ALL=>'C',
            LUNA_OSLO_JIT_MODE=>$mode,LUNA_OSLO_EVIDENCE=>1);
        chdir "$root/work" or die $!;
        open STDIN,'<','/dev/null' or die $!;
        open STDOUT,'>',"$root/$stdout" or die $!;
        open STDERR,'>',"$root/$stderr" or die $!;
        exec 'taskset','-c',"$cpu",$image,'-c',$command;
        die $!;
    }
    waitpid($pid,0)==$pid or die $!;
    my $status=$?;
    my $elapsed=int((clock_gettime(CLOCK_MONOTONIC)-$started)*1e9);
    my $exit=WIFEXITED($status) ? WEXITSTATUS($status) : -1;
    print {$records} $json->encode({sequence=>$sequence-1,profile=>"$profile",case=>$case,
        cpu=>$cpu,round=>$round,sample=>$sample,label=>$label,mode=>$mode,image=>$image,
        argv=>['taskset','-c',"$cpu",$image,'-c',$command],stdout=>$stdout,stderr=>$stderr,
        duration_ns=>$elapsed,status=>$exit,warmup=>($stage eq 'capture' && $sample<2 ? 1 : 0)})."\n";
    $exit==0 or die "consumer failed: $stderr\n";
}

for my $profile (3,'s') {
    if ($stage eq 'check') {
        for my $case (qw(primary row_fields boundary)) {
            run($profile,$case,0,0,0,$_) for qw(a off auto);
        }
    } else {
        for my $round (0..5) {
            for my $cpu ($round%2 ? (16,0) : (0,16)) {
                for my $sample (0..12) {
                    run($profile,'primary',$cpu,$round,$sample,$_) for @{$orders[($round+$cpu/16+$sample)%6]};
                }
            }
        }
    }
}
close $records or die $!;
open my $complete,'>',"$root/$stage/completed" or die $!;
print {$complete} "complete\n";
close $complete or die $!;
