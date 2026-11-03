#!/usr/bin/env perl
# Stand-in for `claude` in scripts/perf_smoke.sh. The main panel is the one
# caucus launches with `--mcp-config`; it writes at CAUCUS_PERF_MAIN_HZ and
# every other panel at CAUCUS_PERF_WORKER_HZ. A rate of 0 stays silent.
# Output starts once CAUCUS_PERF_GATE exists, so every panel is up first.
use strict;
use warnings;
use Time::HiRes qw(clock_gettime sleep CLOCK_MONOTONIC);

my $is_main = grep { $_ eq '--mcp-config' } @ARGV;
my $rate = $is_main ? $ENV{CAUCUS_PERF_MAIN_HZ} : $ENV{CAUCUS_PERF_WORKER_HZ};
my $gate = $ENV{CAUCUS_PERF_GATE};
my $label = ($is_main ? 'main-' : 'worker-') . ($ENV{CAUCUS_PANEL_ID} // $$);
die "CAUCUS_PERF_MAIN_HZ, CAUCUS_PERF_WORKER_HZ and CAUCUS_PERF_GATE must be set\n"
    unless defined $rate && defined $gate && length $gate;

sleep 0.01 until -e $gate;
sleep 3600 while $rate == 0;
$| = 1;
my $period = 1 / $rate;
my $next = clock_gettime(CLOCK_MONOTONIC);
my $sequence = 0;
while (1) {
    $sequence++;
    printf "\rbench-output-%08d-%s", $sequence, $label;
    $next += $period;
    my $now = clock_gettime(CLOCK_MONOTONIC);
    $next = $now + $period if $next < $now - $period;
    my $remaining = $next - $now;
    sleep $remaining if $remaining > 0;
}
