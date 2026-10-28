use strict;
use warnings;
use File::Temp qw(tempdir);
use JSON::PP;

my $reader=shift // die "reader path required\n";
my $directory=tempdir(CLEANUP=>1);
my $selection="jit::compiler_jobs::tests::budget_example: test\njit::compiler_jobs::tests::native_example: test\n";
my $log=<<'LOG';
==100== Memcheck, a memory error detector
==100== Using Valgrind-3.28.0 and LibVEX; rerun with -h for copyright info
==100== Command: test-executable jit::compiler_jobs::tests --test-threads=1
running 2 tests
test jit::compiler_jobs::tests::budget_example ... ok
test jit::compiler_jobs::tests::native_example ... ==200== Warning: child pre-exec diagnostic
==200==     possibly lost: 8,888 bytes in 30 blocks
==200==     still reachable: 999,999 bytes in 40 blocks
==200== ERROR SUMMARY: 0 errors from 0 contexts (suppressed: 0 from 0)
ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.84s
==100== LEAK SUMMARY:
==100==     definitely lost: 0 bytes in 0 blocks
==100==     indirectly lost: 0 bytes in 0 blocks
==100==       possibly lost: 48 bytes in 1 blocks
==100==     still reachable: 544 bytes in 1 blocks
==100==          suppressed: 0 bytes in 0 blocks
==100== ERROR SUMMARY: 0 errors from 0 contexts (suppressed: 0 from 0)
LOG

sub write_file {
    my ($path,$content)=@_;
    open my $fh,'>:raw',$path or die $!;
    print {$fh} $content;
    close $fh or die $!;
}
sub read_file { open my $fh,'<:raw',shift or die $!; local $/; return <$fh>; }
my @mutations=(
    sub { my $s=shift; $s =~ s/2 passed; 0 failed/1 passed; 1 failed/; $s },
    sub { my $s=shift; $s =~ s/0 ignored/1 ignored/; $s },
    sub { my $s=shift; $s =~ s/==100== ERROR SUMMARY: 0/==100== ERROR SUMMARY: 1/; $s },
    sub { my $s=shift; $s =~ s/definitely lost: 0/definitely lost: 1/; $s },
    sub { my $s=shift; $s =~ s/indirectly lost: 0/indirectly lost: 1/; $s },
    sub { my $s=shift; $s =~ s/suppressed: 0 from 0/suppressed: 1 from 1/g; $s },
    sub { my $s=shift; $s =~ s/test jit::compiler_jobs::tests::native_example/test jit::compiler_jobs::tests::budget_example/; $s },
    sub { my $s=shift; $s =~ s/^test jit::compiler_jobs::tests::budget_example[^\n]+\n//m; $s },
    sub { shift."\nunhandled amd64-linux syscall: 424\n" },
    sub { my $s=shift; $s =~ s/^==100== ERROR SUMMARY:[^\n]+\n//m; $s },
);
write_file("$directory/selection",$selection);
my $valid=0;
my $index=0;
for my $mutation (undef,@mutations,undef) {
    ++$index;
    my $candidate=$mutation ? $mutation->($log) : $log;
    die "mutation $index changed no bytes\n" if $mutation && $candidate eq $log;
    write_file("$directory/log",$candidate);
    my $pid=fork();
    die $! unless defined $pid;
    if (!$pid) {
        open STDOUT,'>',"$directory/output" or die $!;
        open STDERR,'>',"$directory/error" or die $!;
        exec $^X,$reader,"$directory/selection","$directory/log";
        die $!;
    }
    waitpid($pid,0);
    my $status=$?;
    if ($mutation) { die "corrupt memory evidence $index accepted\n" unless $status; }
    else {
        die read_file("$directory/error") if $status;
        my $report=decode_json(read_file("$directory/output"));
        die 'parent/child partition mismatch' unless $report->{tests}==2 && $report->{parent_pid}==100 && $report->{parent_memory}{'possibly lost'}{bytes}==48 && $report->{parent_memory}{'still reachable'}{bytes}==544 && !$report->{full_jit_acceptance};
        ++$valid;
    }
}
print "valid=$valid rejected=".scalar(@mutations)." parent_partition=1\n";
