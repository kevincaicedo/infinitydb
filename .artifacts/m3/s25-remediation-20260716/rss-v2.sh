#!/usr/bin/env bash
# ADR-0046 corpus-v2 RSS A/B: per-index-unique documents from the pinned
# generator (`inf-bench doc-corpus --pipe`), identical bytes into both
# engines, incremental RSS + count/used_memory verification.
# Usage: rss-v2.sh "<shape>=<n>[,<shape>=<n>...]" <label>
set -euo pipefail
COUNTS=$1; LABEL=$2
INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
SCRATCH=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/52b965c4-6344-45c7-893c-ad3a26b372e7/scratchpad
OUT=$INF/.artifacts/m3/s25-remediation-20260716/v2-$LABEL
PORT_INF=6401; PORT_STACK=6402
STACK_IMG=redis/redis-stack-server@sha256:798ab84d9f266936b034ab11c4d04a2b8e4b441884c5aa7d17ac951eefdf742a
mkdir -p "$OUT" "$SCRATCH/pipes"

PIPE=$SCRATCH/pipes/$LABEL.resp
"$INF/target/release/inf-bench" doc-corpus --seed 0x1D0C2026 \
    --pipe "$PIPE" --counts "$COUNTS" | tee "$OUT/pipe-manifest.txt"
SER=$(grep serialized_bytes_total "$OUT/pipe-manifest.txt" | awk '{print $3}')
DOCS=$(grep documents_total "$OUT/pipe-manifest.txt" | awk '{print $3}')

rss_kb() { ps -o rss= -p "$1" | tr -d ' '; }

"$INF/target/release/infinityd" --port $PORT_INF >"$OUT/infinityd.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do redis-cli -p $PORT_INF ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
BI=$(rss_kb $SRV)
redis-cli -p $PORT_INF --pipe < "$PIPE" >"$OUT/inf-load.txt" 2>&1
sleep 2
DBI=$(redis-cli -p $PORT_INF dbsize)
redis-cli -p $PORT_INF info memory > "$OUT/inf-info-memory.txt" 2>/dev/null || true
RI=$(rss_kb $SRV)
kill $SRV; wait $SRV 2>/dev/null || true

docker run -d --rm --name rss-v2 -p $PORT_STACK:6379 "$STACK_IMG" >/dev/null
for _ in $(seq 1 150); do redis-cli -p $PORT_STACK ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
SP=$(pgrep -f "redis-server.*6379" | while read -r p; do grep -q docker /proc/$p/cgroup 2>/dev/null && echo "$p" && break; done)
BS=$(rss_kb "$SP")
redis-cli -p $PORT_STACK --pipe < "$PIPE" >"$OUT/stack-load.txt" 2>&1
sleep 2
DBS=$(redis-cli -p $PORT_STACK dbsize)
redis-cli -p $PORT_STACK info memory > "$OUT/stack-info-memory.txt" 2>/dev/null || true
RS=$(rss_kb "$SP")
docker stop rss-v2 >/dev/null

python3 - "$LABEL" "$SER" "$BI" "$RI" "$BS" "$RS" "$DOCS" "$DBI" "$DBS" <<'EOF' | tee "$OUT/verdict.txt"
import sys
label = sys.argv[1]
ser, bi, ri, bs, rs, docs, dbi, dbs = map(int, sys.argv[2:10])
inf = (ri - bi) * 1024; stack = (rs - bs) * 1024
print(f"{label}: serialized={ser} inf={inf} stack={stack}")
print(f"  inf/serialized = {inf/ser:.3f}  (gate <= 1.5)")
print(f"  inf/stack      = {inf/stack:.3f}  (gate <= 0.7)")
print(f"verification: expected_docs={docs} inf_dbsize={dbi} stack_dbsize={dbs}")
if dbi != docs or dbs != docs:
    print("WARNING: document count mismatch — row invalid for claims")
EOF
