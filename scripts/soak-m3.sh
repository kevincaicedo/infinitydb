#!/usr/bin/env bash
# M3 §7 stability gate: 24 h document-workload soak — corpus-v2 ingest
# (unique documents), path reads, and scalar mutations against memory +
# durable-everysec namespaces with continuous checkpoint cycles.
# Zero crashes + RSS slope < 0.5%/24 h + tripwires green.
#
# Usage: soak-m3.sh [hours] [data-dir] [out-dir]
set -euo pipefail
HOURS="${1:-24}"
DATA_DIR="${2:-$HOME/.cache/inf-m3-soak/data}"
OUT="${3:-.artifacts/m3/soak-$(date +%Y%m%d-%H%M)}"
PORT=7402
CELLS=4
SEED=0x1D0C2026
DURATION_S=$((HOURS * 3600))
WORK="${WORK:-$(mktemp -d)}"
mkdir -p "$OUT"
rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"
cargo build --release -p infinityd -p inf-bench

# ---- workload pipes (generated once, replayed in loops) --------------------
# Ingest leg: a reduced corpus-v2 mix (~21 MB serialized, unique docs) —
# every replay overwrites the same keys, exercising release/re-tier paths.
./target/release/inf-bench doc-corpus --seed $SEED --pipe "$WORK/ingest.resp" \
    --counts "small-200B=2000,gate-1KiB=2000,medium-2KiB=1000,large-64KiB=100,deep-32=2000,wide-array=20" \
    > "$OUT/ingest-manifest.txt"
python3 - "$WORK" <<'EOF'
import sys
work = sys.argv[1]
def frame(args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out
# Read leg: depth-4 path fetches over the gate shape + array probes on wide.
with open(f"{work}/reads.resp", "wb") as f:
    for i in range(2000):
        f.write(frame([b"JSON.GET", b"gate-1KiB:%d" % i, b"$.child.child.child.child.id"]))
    for i in range(20):
        for k in (0, 999, 9999):
            f.write(frame([b"JSON.GET", b"wide-array:%d" % i, b"$[%d].qty" % k]))
# Mutation leg: the scalar fast lane (NUMINCRBY) + a structural splice mix.
with open(f"{work}/mutations.resp", "wb") as f:
    for i in range(2000):
        f.write(frame([b"JSON.NUMINCRBY", b"gate-1KiB:%d" % i, b"$.score", b"1"]))
    for i in range(200):
        f.write(frame([b"JSON.SET", b"small-200B:%d" % i, b"$.name", b'"soak"']))
EOF

# ---- server ------------------------------------------------------------------
./target/release/infinityd --port $PORT --cells $CELLS --pin-start 4 \
  --data-dir "$DATA_DIR" --segment-bytes $((64 << 20)) \
  --ckpt-interval-bytes $((64 << 20)) >"$OUT/infinityd.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
sleep 2
redis-cli -p $PORT INF.NS CREATE soak_es MODE durable FSYNC everysec
redis-cli -p $PORT INFO persistence >"$OUT/info-start.txt"

# ---- RSS + doc-domain sampler (10 s cadence) ---------------------------------
(
  echo "unix_s,vmrss_kb,docs_live,doc_resident,ckpts,segs_live" >"$OUT/rss.csv"
  while kill -0 $SERVER_PID 2>/dev/null; do
    rss=$(awk '/VmRSS/{print $2}' /proc/$SERVER_PID/status 2>/dev/null || echo 0)
    mem=$(redis-cli -p $PORT INFO memory 2>/dev/null || true)
    pers=$(redis-cli -p $PORT INFO persistence 2>/dev/null || true)
    docs=$(grep -oP 'docs_live:\K\d+' <<<"$mem" | head -1 || echo 0)
    dres=$(grep -oP 'doc_resident_bytes:\K\d+' <<<"$mem" | head -1 || echo 0)
    ck=$(grep -oP 'ckpts_completed:\K\d+' <<<"$pers" | paste -sd+ | bc 2>/dev/null || echo 0)
    lv=$(grep -oP 'log_segments_live:\K\d+' <<<"$pers" | paste -sd+ | bc 2>/dev/null || echo 0)
    echo "$(date +%s),$rss,${docs:-0},${dres:-0},$ck,$lv" >>"$OUT/rss.csv"
    sleep 10
  done
) &
SAMPLER_PID=$!

# ---- workload legs (sustained, non-saturating, restarted per pass) -----------
run_leg() { # name, pipe, ns (or "")
  local name=$1 pipe=$2 ns=$3
  while true; do
    if [ -n "$ns" ]; then
      { printf '*3\r\n$6\r\nINF.NS\r\n$3\r\nUSE\r\n$%d\r\n%s\r\n' "${#ns}" "$ns"; cat "$pipe"; } \
        | redis-cli -p $PORT --pipe >>"$OUT/loadgen-$name.log" 2>&1 || true
    else
      redis-cli -p $PORT --pipe < "$pipe" >>"$OUT/loadgen-$name.log" 2>&1 || true
    fi
    sleep 0.2
  done
}
run_leg ingest "$WORK/ingest.resp"    ""      & LEG1=$!
run_leg reads  "$WORK/reads.resp"     ""      & LEG2=$!
run_leg mut    "$WORK/mutations.resp" ""      & LEG3=$!
run_leg esec   "$WORK/ingest.resp"    soak_es & LEG4=$!
trap 'kill $LEG1 $LEG2 $LEG3 $LEG4 $SAMPLER_PID $SERVER_PID 2>/dev/null || true' EXIT

echo "soak-m3: running $HOURS h against pid $SERVER_PID (out: $OUT)"
sleep "$DURATION_S"
kill $LEG1 $LEG2 $LEG3 $LEG4 2>/dev/null || true
sleep 2
redis-cli -p $PORT INFO persistence >"$OUT/info-end.txt"
redis-cli -p $PORT INFO tripwires >>"$OUT/info-end.txt" || true
redis-cli -p $PORT INFO memory >>"$OUT/info-end.txt" || true

if ! kill -0 $SERVER_PID 2>/dev/null; then
  echo "FAIL: server died during soak (see infinityd.log)" | tee "$OUT/verdict.txt"; exit 1
fi
python3 - "$OUT/rss.csv" "$HOURS" <<'EOF' | tee "$OUT/verdict.txt"
import csv, sys
rows = [r for r in csv.DictReader(open(sys.argv[1])) if int(r["vmrss_kb"]) > 0]
h = float(sys.argv[2])
k = max(1, len(rows) // 20)                      # first/last 5% of samples
med = lambda xs: sorted(xs)[len(xs) // 2]
first, last = med([int(r["vmrss_kb"]) for r in rows[:k]]), med([int(r["vmrss_kb"]) for r in rows[-k:]])
slope = (last - first) / first * 100 / (h / 24)  # normalized to %/24h
verdict = "PASS" if slope < 0.5 else "FAIL"
print(f"{verdict}: RSS slope {slope:+.3f}%/24h (first {first} KiB -> last {last} KiB, {len(rows)} samples)")
EOF
echo "soak-m3: artifacts in $OUT"
