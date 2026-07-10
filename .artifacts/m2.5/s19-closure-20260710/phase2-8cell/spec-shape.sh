#!/usr/bin/env bash
# M2.5-S19 Phase 2 (supplementary): the 8-cell SPEC SHAPE read — cells on
# all 8 P-cores (--pin-start 0 → cpus 0,2,4,6,8,10,12,14), loadgen on
# E-cores 16-23 (measured generator ceiling 7.06-7.07M ops/s, phase0 —
# any row at/near it is a GENERATOR FLOOR, not an engine number).
# Includes the de-async A/B arm at 8 cells (ADR-0034 D2), ABAB order.
# The harness-shape (unpinned) citable rows come from gate-run --cells 8.
set -euo pipefail
BIN=/home/kcaicedo/Documents/Projects/databases/infinitydb/target/release
OUT=$(dirname "$0")
PORT=6420

leg() {
  local name=$1; shift
  local route_flags=()
  local extra=("$@")
  echo "== leg $name: ${extra[*]:-defaults} =="
  "$BIN/infinityd" --port $PORT --cells 8 --pin-start 0 "${extra[@]}" \
    > "$OUT/$name-server.stdout" 2> "$OUT/$name-server.stderr" &
  local srv=$!
  sleep 2
  taskset -c 16-23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --fill 1000000 --duration 5 > "$OUT/$name-fill.stdout" 2>&1
  redis-cli -p $PORT info > "$OUT/$name-info-before.txt" 2>/dev/null || true
  taskset -c 16-23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --duration 20 > "$OUT/$name-load.stdout" 2>&1
  redis-cli -p $PORT info > "$OUT/$name-info-after.txt" 2>/dev/null || true
  kill $srv 2>/dev/null || true
  wait $srv 2>/dev/null || true
  grep ops_per_sec "$OUT/$name-load.stdout"
  sleep 1
}

# De-async ABAB at 8 cells, natural routing (n=3 per arm)
leg N1-off --no-deasync-dispatch
leg N1-on  --deasync-dispatch
leg N2-off --no-deasync-dispatch
leg N2-on  --deasync-dispatch
leg N3-off --no-deasync-dispatch
leg N3-on  --deasync-dispatch
# All-local rows (n=3; no de-async arm — the pump is off this path)
leg L1 --route-local-only
leg L2 --route-local-only
leg L3 --route-local-only
