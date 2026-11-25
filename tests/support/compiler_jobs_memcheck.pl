use strict;
use warnings;
use JSON::PP;

sub bytes {
    my ($file)=@_;
    open my $fh,'<:raw',$file or die "$file: $!\n";
    local $/;
    return <$fh>;
}
sub need ($$) { my ($ok,$why)=@_; die "$why\n" unless $ok; }

my ($selection,$log)=@ARGV;
need(defined($selection) && defined($log),'selection and log required');
my @selected=(bytes($selection) =~ /^(jit::compiler_jobs::tests::[a-z_]+): test$/mg);
my %selected=map { $_=>1 } @selected;
my $expected=@selected;
need($expected>0 && keys(%selected)==$expected,'empty or duplicate test selection');
my $raw=bytes($log);
need($raw =~ /\A==([0-9]+)== Memcheck, a memory error detector\n/,'parent PID header missing');
my $pid=$1;
my $parent=join "\n",map { /^==\Q$pid\E== ?(.*)\z/ ? $1 : () } split /\n/,$raw;
need($parent =~ /^Using (Valgrind-[^\n]+) and LibVEX;/m,'tool identity missing');
my $tool=$1;
need($raw =~ /^test result: ok\. \Q$expected\E passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out; finished in [0-9.]+s$/m,'test family did not fully pass');
my @started=($raw =~ /^test (jit::compiler_jobs::tests::[a-z_]+) \.\.\./mg);
need(@started==$expected,'executed test count mismatch');
for my $name (@started) { need(delete($selected{$name}),'unknown or duplicate executed test'); }
need(!keys(%selected),'missing executed test');
my @errors=($parent =~ /^ERROR SUMMARY: (.*)$/mg);
need(@errors==1 && $errors[0] eq '0 errors from 0 contexts (suppressed: 0 from 0)','parent memory errors or suppressions');
my %memory;
for my $kind ('definitely lost','indirectly lost','possibly lost','still reachable','suppressed') {
    my @values=($parent =~ /^\s*\Q$kind\E: ([0-9,]+) bytes in ([0-9,]+) blocks$/mg);
    need(@values==2,"missing parent $kind summary");
    for (@values) { s/,//g; }
    $memory{$kind}={bytes=>0+$values[0],blocks=>0+$values[1]};
}
need($memory{'definitely lost'}{bytes}==0 && $memory{'indirectly lost'}{bytes}==0 && $memory{suppressed}{bytes}==0,'parent leaks or suppressed bytes');
need($raw !~ /unhandled.*syscall|test result: FAILED|thread .*panicked/,'unsupported syscall or failed test');
print JSON::PP->new->canonical->pretty->encode({
    tests=>$expected,parent_pid=>0+$pid,tool=>$tool,parent_memory=>\%memory,
    exec_children_traced=>JSON::PP::false,
    full_resource_acceptance=>JSON::PP::false,full_jit_acceptance=>JSON::PP::false,
});
