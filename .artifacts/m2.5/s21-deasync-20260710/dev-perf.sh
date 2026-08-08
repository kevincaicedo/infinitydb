#!/usr/bin/env bash
# Perf re-attribution: does the ON arm actually bypass dispatch_one's
# async machinery? Extract symbol tables, delete perf.data (RAM lesson).
set -euo pipefail
BIN=/home/kcaicedo/Documents/Projects/databases/infinitydb/target/release
OUT=$(dirname "$0")
PORT=6402
CELLS_CPUS=4,6,8,10

perf_leg() {
  local name=$1; shift
  local server_flags=("$@")
  echo "== perf leg $name =="
  "$BIN/infinityd" --port $PORT --cells 4 --pin-start 4 "${server_flags[@]}" \
    > /dev/null 2> "$OUT/$name-perf-server.stderr" &
  local srv=$!
  sleep 1
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --fill 1000000 --duration 5 > /dev/null 2>&1
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --duration 30 \
    > "$OUT/$name-perf-load.stdout" 2>&1 &
  local load=$!
  sleep 8
  perf stat -C $CELLS_CPUS -e cycles,instructions,cache-misses -- sleep 8 \
    2> "$OUT/$name-perf-stat.txt"
  perf record -C $CELLS_CPUS -F 1997 -g --call-graph dwarf,16384 \
    -o "$OUT/$name.perf.data" -- sleep 8 2> /dev/null
  wait $load || true
  kill $srv 2>/dev/null || true
  wait $srv 2>/dev/null || true
  perf report -i "$OUT/$name.perf.data" --stdio --no-children --percent-limit 0.10 \
    > "$OUT/$name-perf-agg.txt" 2>/dev/null
  rm -f "$OUT/$name.perf.data"
  grep ops_per_sec "$OUT/$name-perf-load.stdout"
}

perf_leg P-off --no-deasync-dispatch
perf_leg P-on  --deasync-dispatch
