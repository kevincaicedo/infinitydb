#!/usr/bin/env bash
# M4.5-S35 dev-tier preview (ADR-0087 D8). NOT §19-valid: no competitors
# in-run, governor/thermal state as found (disclosed in the output header),
# no gate-run. Same binary every arm; the arm flags are the only difference.
#
# Shape: the S34 F2 discriminator — 200k × 1 KB load at 64 conns, then
# 128k ops at 32 conns, zipfian, 100 % write, FSYNC always, tri-bench kv.
# Cells pinned 0,2,4,6 (--pin-start 0), generator pinned 8,10,12,14; runs
# spaced 20 s (the S34 drive-state rule; fstrim needs sudo).
#
# Arms (all --barrier-class fua):
#   k1   : --frames-in-flight 1 --log-staging-mib 4   (S34's arm, the baseline)
#   k3s2 : --frames-in-flight 3 --log-staging-mib 2   (the L5-neutral reference arm, 4 × 2 MiB)
#   k4   : --frames-in-flight 4 --log-staging-mib 4   (depth trend; +8 MiB/cell attributed)
#
# usage: preview.sh <out-file> [reps=3] [cells-list="4 1"]
set -euo pipefail
OUT=${1:?out file}; REPS=${2:-3}; CELLS_LIST=${3:-"4 1"}
BIN=/home/kcaicedo/Documents/Projects/databases/infinitydb/target/release/infinityd
TRI=/home/kcaicedo/bench-harness/target/release/tri-bench
DATA=/home/kcaicedo/bench-data/s35-ab
mkdir -p "$DATA"
PORT=7393
{
  echo "# $(date -Is) host=$(hostname) kernel=$(uname -r)"
  echo "# governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown)"
  echo "# binary=$($BIN --version)"
  echo "# git=$(git -C /home/kcaicedo/Documents/Projects/databases/infinitydb rev-parse --short HEAD) dirty=$(git -C /home/kcaicedo/Documents/Projects/databases/infinitydb status --porcelain | wc -l)"
} | tee "$OUT"

scrape() {
  # cell-scope fields summed/maxed over cells via INFO on one connection
  # (persistence rows are cell-scope; the generator hits every cell).
  redis-cli -p $PORT INFO persistence 2>/dev/null | tr -d '\r' | awk -F: '
    /^(fsync_latency_p50_us|fsync_latency_p99_us|fsyncs_linked|fsyncs_fua|fua_latency_p50_us|fua_latency_p99_us|log_write_stall_p99_us|frames_in_flight|frames_in_flight_max|frame_waits_barrier|frame_waits_rotation|log_admission_parked_total|zero_fill_bytes|log_staging_bytes):/ {printf "%s:%s  ", $1, $2}
    END {print ""}'
}

run_arm() {
  local tag=$1 cells=$2 k=$3 mib=$4 rep=$5
  local dir="$DATA/d-$tag-${cells}c-r$rep"
  rm -rf "$dir"; mkdir -p "$dir"
  echo "=== $tag (cells=$cells frames-in-flight=$k staging-mib=$mib rep=$rep, 40s idle before) ===" | tee -a "$OUT"
  sleep 40
  taskset -c 0,2,4,6 "$BIN" --cells "$cells" --port $PORT --data-dir "$dir" --pin-start 0 \
    --barrier-class fua --frames-in-flight "$k" --log-staging-mib "$mib" \
    > "$DATA/$tag-${cells}c-r$rep.log" 2>&1 &
  local spid=$!
  for _ in $(seq 1 100); do redis-cli -p $PORT PING 2>/dev/null | grep -q PONG && break; sleep 0.1; done
  redis-cli -p $PORT INF.NS CREATE syncflat MODE durable FSYNC always >/dev/null
  taskset -c 8,10,12,14 "$TRI" --engine infinity --phase load --records 200000 --conns 64 \
    --port $PORT --ns syncflat --shape kv > /dev/null 2>&1
  sleep 2
  taskset -c 8,10,12,14 "$TRI" --engine infinity --phase run --records 200000 --ops 128000 \
    --conns 32 --read-pct 0 --dist zipfian --port $PORT --ns syncflat --shape kv --warmup 2 2>&1 \
    | grep -E "throughput|p50_ms|p99_ms|max_ms|errors" | tr '\n' ' ' | tee -a "$OUT"
  echo | tee -a "$OUT"
  scrape | tee -a "$OUT"
  kill "$spid"; wait "$spid" 2>/dev/null || true
  sleep 1
}

for cells in $CELLS_LIST; do
  for rep in $(seq 1 "$REPS"); do
    # Rotated order (k4 → k1 → k3s2), 40 s spacing: separates the drive-
    # state bad mode (position) from the arm.
    run_arm k4   "$cells" 4 4 "$rep"
    run_arm k1   "$cells" 1 4 "$rep"
    run_arm k3s2 "$cells" 3 2 "$rep"
  done
done
echo "# done $(date -Is)" | tee -a "$OUT"
