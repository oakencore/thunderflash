#!/usr/bin/env bash
# Localhost benchmark driver for thunderflash. Measures software overhead,
# not the Thunderbolt link — see docs/benchmarks.md before quoting a number.
#
#   TF_BENCH_DIR=/tmp/tf-bench TF_LARGE_GIB=16 bash scripts/bench.sh
#
# Everything except the cargo build output is written under TF_BENCH_DIR.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BENCH_DIR="${TF_BENCH_DIR:-/tmp/tf-bench}"
LARGE_GIB="${TF_LARGE_GIB:-16}"
SMALL_COUNT="${TF_SMALL_COUNT:-10000}"
MEDIUM_COUNT="${TF_MEDIUM_COUNT:-10000}"

# Both ends share one machine here, but blake3's rayon pool defaults to
# wanting the whole machine's cores each, and the two pools thrash. Cap each
# at half. The two-Mac procedure needs no cap; each side has its own cores.
export RAYON_NUM_THREADS="${RAYON_NUM_THREADS:-$(( $(sysctl -n hw.ncpu) / 2 ))}"

FIXTURES="$BENCH_DIR/fixtures"
DEST="$BENCH_DIR/dest"
LOGS="$BENCH_DIR/logs"
BENCH="$REPO/target/release/examples/bench"

# A failed run must not leave a receiver blocked in accept forever.
cleanup() {
  local pids
  pids="$(jobs -p)"
  [ -n "$pids" ] && kill $pids 2>/dev/null
  return 0
}
trap cleanup EXIT

cargo build --release --example bench --manifest-path "$REPO/Cargo.toml"
mkdir -p "$FIXTURES" "$LOGS"

echo "== fixtures =="
"$BENCH" fix "large=$LARGE_GIB" "$FIXTURES/large"
"$BENCH" fix "small=$SMALL_COUNT" "$FIXTURES/small"
"$BENCH" fix "medium=$MEDIUM_COUNT" "$FIXTURES/medium"
"$BENCH" fix mixed "$FIXTURES/mixed"
"$BENCH" fix edge "$FIXTURES/edge"

# `key=value` out of a SEND/RECV line.
kv() {
  awk -v key="$1" '{
    for (i = 1; i <= NF; i++) {
      n = index($i, "=")
      if (n && substr($i, 1, n - 1) == key) { print substr($i, n + 1); exit }
    }
  }' "$2"
}

# "1.23 real 0.45 user 0.67 sys" out of /usr/bin/time -l.
cpu_seconds() {
  awk '{
    for (i = 2; i <= NF; i++) {
      if ($i == "user") u = $(i - 1)
      if ($i == "sys") s = $(i - 1)
    }
  } END { printf "%.2f", u + s }' "$1"
}

peak_rss_mb() {
  awk '/maximum resident set size/ { printf "%.1f", $1 / 1048576; exit }' "$1"
}

ROWS=()

run() {
  local label="$1" fixture="$2"
  local recv_log="$LOGS/$label.recv.log" send_log="$LOGS/$label.send.log"

  rm -rf "$DEST"
  mkdir -p "$DEST"
  : >"$recv_log"

  /usr/bin/time -l "$BENCH" recv "$DEST" >"$recv_log" 2>&1 &
  local recv_pid=$!

  local port=""
  for _ in $(seq 1 400); do
    port="$(awk '/^PORT /{print $2; exit}' "$recv_log")"
    if [ -n "$port" ]; then break; fi
    sleep 0.05
  done
  if [ -z "$port" ]; then
    echo "receiver never printed PORT; see $recv_log" >&2
    exit 1
  fi

  /usr/bin/time -l "$BENCH" send "$port" "$fixture" >"$send_log" 2>&1
  wait "$recv_pid"

  local bytes files elapsed walk
  bytes="$(kv bytes "$send_log")"
  files="$(kv files "$send_log")"
  elapsed="$(kv elapsed_s "$send_log")"
  walk="$(kv walk_s "$send_log")"

  local rate
  rate="$(awk -v b="$bytes" -v e="$elapsed" 'BEGIN { printf "%.2f", (e > 0 ? b / e / 1e9 : 0) }')"

  ROWS+=("$(printf '%-13s %14s %7s %9s %7s %9s %9s %11s %11s %8s' \
    "$label" "$bytes" "$files" "$elapsed" "$rate" \
    "$(cpu_seconds "$send_log")" "$(cpu_seconds "$recv_log")" \
    "$(peak_rss_mb "$send_log")" "$(peak_rss_mb "$recv_log")" "$walk")")

  echo "  $label: $bytes bytes, $files files, ${elapsed}s, $rate GB/s"
}

echo "== runs =="
run large-cold "$FIXTURES/large"
# Immediately again: same bytes, page cache as warm as it is going to get.
run large-warm "$FIXTURES/large"
run small-4KiB "$FIXTURES/small"
run medium-1MiB "$FIXTURES/medium"
run mixed "$FIXTURES/mixed"
run edge "$FIXTURES/edge"

rm -rf "$DEST"

echo
printf '%-13s %14s %7s %9s %7s %9s %9s %11s %11s %8s\n' \
  workload bytes files elapsed_s GB/s send_cpu recv_cpu send_rss_MB recv_rss_MB walk_s
for row in "${ROWS[@]}"; do echo "$row"; done
echo
echo "per-run logs: $LOGS"
