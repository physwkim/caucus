#!/usr/bin/env bash
# Release CPU smoke: caucus's own CPU and RSS, baseline against candidate,
# under synthetic agent output (scripts/perf_fake_agent.pl stands in for
# `claude`). visible30 is one on-screen panel writing at 30 Hz: ingest and
# render. hidden50 is 49 workers writing at 60 Hz behind a zoomed, silent
# main panel: ingest and the per-tick passes with no worker render. Two
# rounds in alternating order; fails when the candidate's mean CPU exceeds
# the baseline's by more than 25% and 0.5 CPU points.
# CAUCUS_PERF_SAMPLE_SECONDS (10) and CAUCUS_PERF_WARMUP_SECONDS (3) set
# the per-case pidstat window and the settle time before it.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <baseline-binary> <candidate-binary>" >&2
  exit 2
fi
abs() { echo "$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; }
baseline=$(abs "$1")
candidate=$(abs "$2")
for bin in "$baseline" "$candidate"; do
  [[ -x $bin ]] || { echo "binary is not executable: $bin" >&2; exit 1; }
done
for command in git perl pidstat tmux; do
  command -v "$command" >/dev/null || { echo "required command not found: $command" >&2; exit 1; }
done

script_dir=$(cd "$(dirname "$0")" && pwd)
seconds=${CAUCUS_PERF_SAMPLE_SECONDS:-10}
warmup=${CAUCUS_PERF_WARMUP_SECONDS:-3}
cols=160
rows=48
root=$(mktemp -d "${TMPDIR:-/tmp}/caucus-perf-smoke.XXXXXX")
# caucus binds $TMPDIR/caucus-<uuid>.sock; a short TMPDIR keeps that under
# the 108-byte sun_path limit wherever $root lives.
sock_dir=$(mktemp -d /tmp/caucus-perf.XXXXXX)
tmux_() { tmux -S "$sock_dir/tmux" -f /dev/null "$@"; }
live_name='' live_pid=''
# Close the live case and wait until caucus and its agents are gone, so no
# sample and no `rm -rf` overlaps a caucus still saving its session.
stop_case() {
  [[ -n $live_pid ]] || return 0
  tmux_ kill-session -t "$live_name" >/dev/null 2>&1 || true
  for _ in $(seq 1 100); do
    if ! kill -0 "$live_pid" 2>/dev/null && ! pgrep -f "$root/bin/claude" >/dev/null; then
      live_pid=''
      return 0
    fi
    sleep 0.1
  done
  echo "$live_name: caucus or its agents still running 10s after the session closed" >&2
  return 1
}
cleanup() {
  stop_case || true
  tmux_ kill-server >/dev/null 2>&1 || true
  pkill -f "$root/bin/claude" >/dev/null 2>&1 || true
  rm -rf "$root" "$sock_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$root/bin" "$root/results"
ln -s "$script_dir/perf_fake_agent.pl" "$root/bin/claude"

# Print "<mean %CPU> <mean RSS KiB> <samples>" for <pid> from `pidstat -h -u -r`.
pidstat_means() {
  awk -v pid="$2" '
    /^#/ { for (i = 2; i <= NF; i++) col[$i] = i - 1; next }
    ("PID" in col) && $col["PID"] == pid { cpu += $col["%CPU"]; rss += $col["RSS"]; n++ }
    END { if (!n) exit 1; printf "%.3f %.0f %d\n", cpu / n, rss / n, n }
  ' "$1"
}

# run_case <binary> <variant> <scenario> <round>: one launch, one sample,
# appended as "<cpu> <rss>" to results/<variant>-<scenario>.
run_case() {
  local bin=$1 variant=$2 scenario=$3 round=$4
  local main_hz worker_hz panels roles=()
  case $scenario in
    visible30) main_hz=30 worker_hz=0 panels=1 ;;
    hidden50)
      main_hz=0 worker_hz=60 panels=50
      roles=(--roles "$(printf 'worker%.0s,' $(seq 2 "$panels") | sed 's/,$//')")
      ;;
    *) echo "unknown scenario: $scenario" >&2; exit 2 ;;
  esac
  local name="$variant-$scenario-r$round"
  local state="$root/$name"
  local gate="$state/gate"
  mkdir -p "$state/home" "$state/repo"
  git -C "$state/repo" init -q

  local launch
  printf -v launch '%q ' env -u CAUCUS_SOCK -u CAUCUS_PANEL_ID -u CAUCUS_SESSION_ID \
    -u CAUCUS_SESSION_DIR -u CAUCUS_HOOK_REPLY HOME="$state/home" TMPDIR="$sock_dir" \
    PATH="$root/bin:$PATH" CAUCUS_PREFIX=a CAUCUS_PERF_MAIN_HZ="$main_hz" CAUCUS_PERF_WORKER_HZ="$worker_hz" \
    CAUCUS_PERF_GATE="$gate" "$bin" "${roles[@]}"
  tmux_ new-session -d -s "$name" -x "$cols" -y "$rows" -c "$state/repo" "exec $launch 2>$(printf %q "$state/stderr")"
  local pid
  pid=$(tmux_ list-panes -t "$name" -F '#{pane_pid}')
  live_name=$name live_pid=$pid

  local up=0
  for _ in $(seq 1 150); do
    kill -0 "$pid" 2>/dev/null ||
      { echo "$name: caucus exited during startup" >&2; cat "$state/stderr" >&2; exit 1; }
    up=$(pgrep -c -P "$pid" || true)
    [[ $up -ge $panels ]] && break
    sleep 0.1
  done
  [[ $up -ge $panels ]] || { echo "$name: $up of $panels panels came up" >&2; exit 1; }
  touch "$gate"
  sleep 1

  local writers
  writers=$(tmux_ capture-pane -p -t "$name" | grep -o 'bench-output-' | wc -l)
  [[ $writers -ge $((panels == 1 ? 1 : panels - 1)) ]] ||
    { echo "$name: only $writers panels show writer output" >&2; exit 1; }
  if [[ $scenario == hidden50 ]]; then
    tmux_ send-keys -t "$name" C-a z
    sleep 0.5
    # main is silent here, so any writer output left on screen is a worker.
    ! tmux_ capture-pane -p -t "$name" | grep -q 'bench-output-' ||
      { echo "$name: zoom did not hide the workers" >&2; exit 1; }
  fi

  sleep "$warmup"
  local raw="$state/pidstat.txt"
  LC_ALL=C pidstat -h -u -r -p "$pid" 1 "$seconds" >"$raw"
  local cpu rss samples
  read -r cpu rss samples < <(pidstat_means "$raw" "$pid") ||
    { echo "$name: pidstat recorded nothing for pid $pid" >&2; exit 1; }
  [[ $samples -eq $seconds ]] || { echo "$name: expected $seconds samples, got $samples" >&2; exit 1; }
  echo "$cpu $rss" >>"$root/results/$variant-$scenario"
  printf '%-30s cpu %7s%%  rss %8s KiB\n' "$name" "$cpu" "$rss"
  stop_case || exit 1
}

scenarios="visible30 hidden50"
for round in 1 2; do
  if [[ $round -eq 1 ]]; then order="baseline candidate"; else order="candidate baseline"; fi
  for scenario in $scenarios; do
    for variant in $order; do
      if [[ $variant == baseline ]]; then bin=$baseline; else bin=$candidate; fi
      run_case "$bin" "$variant" "$scenario" "$round"
    done
  done
done

mean() { awk -v f="$2" '{ s += $f; n++ } END { printf "%.3f", s / n }' "$root/results/$1"; }
failed=0
printf '\ncaucus perf smoke (%s rounds of %ss samples)\n' 2 "$seconds"
printf '%-10s %10s %10s %8s %12s %12s\n' scenario base_cpu cand_cpu change base_rss_mib cand_rss_mib
for scenario in $scenarios; do
  base_cpu=$(mean "baseline-$scenario" 1)
  cand_cpu=$(mean "candidate-$scenario" 1)
  base_rss=$(awk -v k="$(mean "baseline-$scenario" 2)" 'BEGIN { printf "%.1f", k / 1024 }')
  cand_rss=$(awk -v k="$(mean "candidate-$scenario" 2)" 'BEGIN { printf "%.1f", k / 1024 }')
  change=$(awk -v b="$base_cpu" -v c="$cand_cpu" 'BEGIN { if (b == 0) print "n/a"; else printf "%+.1f%%", (c - b) / b * 100 }')
  printf '%-10s %10s %10s %8s %12s %12s\n' "$scenario" "$base_cpu" "$cand_cpu" "$change" "$base_rss" "$cand_rss"
  if awk -v b="$base_cpu" -v c="$cand_cpu" 'BEGIN { exit !(b <= 0 || c <= 0) }'; then
    echo "error: $scenario measured no CPU; the run did not exercise caucus" >&2
    failed=1
  fi
  if awk -v b="$base_cpu" -v c="$cand_cpu" 'BEGIN { exit !(c > b * 1.25 && c - b > 0.5) }'; then
    echo "error: $scenario candidate CPU exceeds baseline by more than 25% and 0.5 CPU points" >&2
    failed=1
  fi
done
[[ $failed -eq 0 ]] || exit 1
echo "caucus perf smoke passed"
