#!/usr/bin/env bash
# M2.5-S19 Phase 6: flamegraph + attribution per cited row class (the
# S22-M2 recorded gap). Attribution instrument: 1 leg per class (the
# s09-s21 cycle-split precedent); the throughput rows the legs cross-check
# carry the replicate burden. perf.data deleted after extraction (RAM).
set -euo pipefail
BIN=/home/kcaicedo/Documents/Projects/databases/infinitydb/target/release
OUT=$(dirname "$0")
PORT=6430
CELLS_CPUS=4,6,8,10

profile_leg() {
  local name=$1; shift
  local load_extra=$1; shift
  local server_flags=("$@")
  echo "== perf leg $name =="
  "$BIN/infinityd" --port $PORT --cells 4 --pin-start 4 "${server_flags[@]}" \
    > /dev/null 2> "$OUT/$name-server.stderr" &
  local srv=$!
  sleep 2
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --fill 1000000 --duration 5 > /dev/null 2>&1
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 $load_extra --duration 30 \
    > "$OUT/$name-load.stdout" 2>&1 &
  local load=$!
  sleep 8
  perf record -C $CELLS_CPUS -F 1997 -g --call-graph dwarf,16384 \
    -o "$OUT/$name.perf.data" -- sleep 8 2> /dev/null
  wait $load || true
  kill $srv 2>/dev/null || true
  wait $srv 2>/dev/null || true
  perf report -i "$OUT/$name.perf.data" --stdio --no-children --percent-limit 0.3 \
    > "$OUT/$name-flame.txt" 2>/dev/null
  perf report -i "$OUT/$name.perf.data" --stdio --no-children --percent-limit 0.3 --sort dso \
    > "$OUT/$name-dso.txt" 2>/dev/null
  rm -f "$OUT/$name.perf.data"
  grep ops_per_sec "$OUT/$name-load.stdout" || true
}

profile_leg m0-natural ""
profile_leg m0-all-local "" --route-local-only
profile_leg m1-mixed "--mix 1:1"
