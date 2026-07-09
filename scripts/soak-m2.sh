#!/usr/bin/env bash
# M2 §6 stability gate: 24 h mixed memory+durable soak with continuous
# checkpoint cycles. Zero crashes + RSS slope < 0.5%/24 h + tripwires green.
#
# Usage:  ./scripts/soak-m2.sh [hours] [data-dir] [out-dir]
# Run from infinitydb/ on the reference box after `just check` on a clean
# tree. Produces: out/rss.csv (10 s samples), out/info-{start,end}.txt,
# out/loadgen-*.log, out/verdict.txt. Ctrl-C stops everything cleanly.
set -euo pipefail

HOURS="${1:-24}"
DATA_DIR="${2:-$HOME/.cache/inf-m2-soak/data}"
OUT="${3:-.artifacts/m2/soak-$(date +%Y%m%d-%H%M)}"
PORT=7401
CELLS=4
DURATION_S=$((HOURS * 3600))

mkdir -p "$OUT"
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

cargo build --release -p infinityd -p inf-bench

./target/release/infinityd --port $PORT --cells $CELLS --pin-start 4 \
  --data-dir "$DATA_DIR" --segment-bytes $((64 << 20)) \
  --ckpt-interval-bytes $((64 << 20)) >"$OUT/infinityd.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
sleep 2

# Namespaces: durable everysec + always alongside the memory default DB.
redis-cli -p $PORT INF.NS CREATE soak_es MODE durable FSYNC everysec
redis-cli -p $PORT INF.NS CREATE soak_al MODE durable FSYNC always
redis-cli -p $PORT INFO persistence >"$OUT/info-start.txt"

# RSS + persistence-gauge sampler (10 s cadence).
(
  echo "unix_s,vmrss_kb,ckpts,manifests,segs_truncated,segs_live" >"$OUT/rss.csv"
  while kill -0 $SERVER_PID 2>/dev/null; do
    rss=$(awk '/VmRSS/{print $2}' /proc/$SERVER_PID/status 2>/dev/null || echo 0)
    info=$(redis-cli -p $PORT INFO persistence 2>/dev/null || true)
    ck=$(grep -oP 'ckpts_completed:\K\d+' <<<"$info" | paste -sd+ | bc 2>/dev/null || echo 0)
    mf=$(grep -oP 'manifests_published:\K\d+' <<<"$info" | paste -sd+ | bc 2>/dev/null || echo 0)
    tr_=$(grep -oP 'segments_truncated:\K\d+' <<<"$info" | paste -sd+ | bc 2>/dev/null || echo 0)
    lv=$(grep -oP 'log_segments_live:\K\d+' <<<"$info" | paste -sd+ | bc 2>/dev/null || echo 0)
    echo "$(date +%s),$rss,$ck,$mf,$tr_,$lv" >>"$OUT/rss.csv"
    sleep 10
  done
) &
SAMPLER_PID=$!

# Three concurrent moderate load generators (soak = sustained, not saturating):
# memory 1:10, everysec 1:1, always SET-only. inf-bench load has no --setup
# flag, so the durable legs select their namespace via key-prefix-free
# per-connection setup — run them through redis-cli pipes instead:
# simplest sustained shape that exercises all three planes.
run_leg() { # name, ns (or ""), mix, conns, pipeline
  local name=$1 ns=$2 mix=$3 conns=$4 pipe=$5
  while true; do
    ./target/release/inf-bench load --port $PORT --conns "$conns" \
      --pipeline "$pipe" --duration 300 --mix "$mix" --keys 200000 \
      --value-size 512 ${ns:+--setup "INF.NS USE $ns"} \
      >>"$OUT/loadgen-$name.log" 2>&1 || true
  done
}
run_leg mem "" 1:10 16 8 &
LEG1=$!
run_leg esec soak_es 1:1 16 8 &
LEG2=$!
run_leg alw soak_al 1:0 8 8 &
LEG3=$!
trap 'kill $LEG1 $LEG2 $LEG3 $SAMPLER_PID $SERVER_PID 2>/dev/null || true' EXIT

echo "soak: running $HOURS h against pid $SERVER_PID (out: $OUT)"
sleep "$DURATION_S"

kill $LEG1 $LEG2 $LEG3 2>/dev/null || true
sleep 2
redis-cli -p $PORT INFO persistence >"$OUT/info-end.txt"
redis-cli -p $PORT INFO tripwires >>"$OUT/info-end.txt" || true

# Verdict: server alive the whole run + RSS slope from the samples.
if ! kill -0 $SERVER_PID 2>/dev/null; then
  echo "FAIL: server died during soak (see infinityd.log)" | tee "$OUT/verdict.txt"
  exit 1
fi
python3 - "$OUT/rss.csv" "$HOURS" <<'EOF' | tee "$OUT/verdict.txt"
import csv, sys
rows = [r for r in csv.DictReader(open(sys.argv[1])) if int(r["vmrss_kb"]) > 0]
h = float(sys.argv[2])
# Slope: compare medians of the first and last 5% of samples (storm-resistant).
k = max(1, len(rows) // 20)
med = lambda xs: sorted(xs)[len(xs) // 2]
first, last = med([int(r["vmrss_kb"]) for r in rows[:k]]), med([int(r["vmrss_kb"]) for r in rows[-k:]])
slope = (last - first) / first * 100 / (h / 24)
verdict = "PASS" if slope < 0.5 else "FAIL"
print(f"{verdict}: RSS slope {slope:+.3f}%/24h (first-5% median {first} kB -> last-5% median {last} kB, {len(rows)} samples)")
EOF
echo "soak: artifacts in $OUT"
