#!/usr/bin/env bash
# M3 §7 document-memory gate rows (blessed S25 harness — the labeled
# hand-run instrument per the plan's "or blessing" clause; inf-compare's
# json lanes are the independent cross-check beside it).
#
# Corpus v2 (ADR-0046 D3): per-index-unique documents from the pinned
# generator; the gate binds on the mixed count vector, per-shape runs are
# diagnostics. Both engines load identical bytes; DBSIZE + used_memory
# are captured before RSS sampling (row invalid on count mismatch).
#
# Usage: bench-m3-rss.sh [counts] [label] [out-dir]
#   counts  default: the frozen mixed vector
#   label   default: mixed
set -euo pipefail
COUNTS="${1:-small-200B=20000,gate-1KiB=20000,medium-2KiB=10000,large-64KiB=1000,deep-32=20000,wide-array=200}"
LABEL="${2:-mixed}"
OUT="${3:-.artifacts/m3/rss-$(date +%Y%m%d-%H%M)/$LABEL}"
PORT_INF="${PORT_INF:-6401}"
PORT_STACK="${PORT_STACK:-6402}"
SEED=0x1D0C2026
STACK_IMG=redis/redis-stack-server@sha256:798ab84d9f266936b034ab11c4d04a2b8e4b441884c5aa7d17ac951eefdf742a
WORK="${WORK:-$(mktemp -d)}"
mkdir -p "$OUT"

cargo build --release -p infinityd -p inf-bench >/dev/null

PIPE="$WORK/$LABEL.resp"
./target/release/inf-bench doc-corpus --seed $SEED \
    --pipe "$PIPE" --counts "$COUNTS" | tee "$OUT/pipe-manifest.txt"
SER=$(grep serialized_bytes_total "$OUT/pipe-manifest.txt" | awk '{print $3}')
DOCS=$(grep documents_total "$OUT/pipe-manifest.txt" | awk '{print $3}')

rss_kb() { ps -o rss= -p "$1" | tr -d ' '; }

./target/release/infinityd --port "$PORT_INF" >"$OUT/infinityd.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true; docker stop bench-m3-rss 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do redis-cli -p "$PORT_INF" ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
BI=$(rss_kb $SRV)
redis-cli -p "$PORT_INF" --pipe < "$PIPE" >"$OUT/inf-load.txt" 2>&1
sleep 2
DBI=$(redis-cli -p "$PORT_INF" dbsize)
redis-cli -p "$PORT_INF" info memory > "$OUT/inf-info-memory.txt" 2>/dev/null || true
RI=$(rss_kb $SRV)
kill $SRV; wait $SRV 2>/dev/null || true

docker run -d --rm --name bench-m3-rss -p "$PORT_STACK":6379 "$STACK_IMG" >/dev/null
for _ in $(seq 1 150); do redis-cli -p "$PORT_STACK" ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
SP=$(pgrep -f "redis-server.*6379" | while read -r p; do grep -q docker /proc/$p/cgroup 2>/dev/null && echo "$p" && break; done)
BS=$(rss_kb "$SP")
redis-cli -p "$PORT_STACK" --pipe < "$PIPE" >"$OUT/stack-load.txt" 2>&1
sleep 2
DBS=$(redis-cli -p "$PORT_STACK" dbsize)
redis-cli -p "$PORT_STACK" info memory > "$OUT/stack-info-memory.txt" 2>/dev/null || true
RS=$(rss_kb "$SP")
docker stop bench-m3-rss >/dev/null

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
    print("INVALID: document count mismatch — this row makes no claim")
    sys.exit(1)
EOF
echo "bench-m3-rss: artifacts in $OUT (scope note: write-once load — RSS excludes post-ingest growth slack by construction)"
