#!/usr/bin/env bash
# M3-S25 dev-tier document-memory RSS A/B: the reference corpus loaded
# into InfinityDB and the pinned redis-stack (RedisJSON) on the same box.
# Gates: RSS <= 1.5x serialized JSON bytes AND <= 0.7x RedisJSON RSS.
# Methodology note: write-once load (no post-ingest mutation slack) — the
# S25 plan records this scope. Hand-run harness, labeled (no inf-compare
# json lane exists yet).
set -euo pipefail

INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
SCRATCH=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/f7c996e7-230c-45c8-80f1-979984910087/scratchpad
CORPUS=$SCRATCH/corpus
OUT=$SCRATCH/rss-ab
PORT_INF=6401
PORT_STACK=6402
STACK_IMG=redis/redis-stack-server@sha256:798ab84d9f266936b034ab11c4d04a2b8e4b441884c5aa7d17ac951eefdf742a

# Per-shape document counts (serialized total ~207 MB, wide/large bounded).
declare -A COUNTS=( [small-200B]=20000 [gate-1KiB]=20000 [medium-2KiB]=10000 [large-64KiB]=1000 [deep-32]=20000 [wide-array]=200 )

mkdir -p "$OUT"

# ---- build the RESP pipe file once (same bytes into both engines) ----
PIPE=$OUT/load.resp
if [ ! -f "$PIPE" ]; then
    python3 - "$CORPUS" "$PIPE" <<'EOF'
import sys, os
corpus, out = sys.argv[1], sys.argv[2]
counts = {"small-200B":20000,"gate-1KiB":20000,"medium-2KiB":10000,
          "large-64KiB":1000,"deep-32":20000,"wide-array":200}
total = 0
with open(out, "wb") as f:
    for shape, n in counts.items():
        doc = open(os.path.join(corpus, shape + ".json"), "rb").read()
        total += len(doc) * n
        for i in range(n):
            key = f"{shape}:{i}".encode()
            args = [b"JSON.SET", key, b"$", doc]
            f.write(b"*%d\r\n" % len(args))
            for a in args:
                f.write(b"$%d\r\n" % len(a)); f.write(a); f.write(b"\r\n")
print("serialized_json_bytes:", total)
print("documents:", sum(counts.values()))
EOF
fi
python3 - "$CORPUS" <<'EOF' > "$OUT/serialized-bytes.txt"
import sys, os
corpus = sys.argv[1]
counts = {"small-200B":20000,"gate-1KiB":20000,"medium-2KiB":10000,
          "large-64KiB":1000,"deep-32":20000,"wide-array":200}
total = sum(len(open(os.path.join(corpus, s + ".json"), "rb").read()) * n
            for s, n in counts.items())
print(f"serialized_json_bytes={total}")
print(f"documents={sum(counts.values())}")
EOF
cat "$OUT/serialized-bytes.txt"

rss_kb() { ps -o rss= -p "$1" | tr -d ' '; }

# ---- InfinityDB ----
"$INF/target/release/infinityd" --port $PORT_INF >"$OUT/infinityd.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do redis-cli -p $PORT_INF ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
BASE_INF=$(rss_kb $SRV)
redis-cli -p $PORT_INF --pipe < "$PIPE" > "$OUT/inf-load.txt" 2>&1
sleep 3
RSS_INF=$(rss_kb $SRV)
redis-cli -p $PORT_INF info memory > "$OUT/inf-info-memory.txt" 2>&1 || true
redis-cli -p $PORT_INF dbsize >> "$OUT/inf-load.txt" 2>&1
kill $SRV; wait $SRV 2>/dev/null || true

# ---- redis-stack (RedisJSON) ----
docker run -d --rm --name rss-ab-stack -p $PORT_STACK:6379 "$STACK_IMG" >"$OUT/stack-id.txt"
for _ in $(seq 1 150); do redis-cli -p $PORT_STACK ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
STACK_PID=$(pgrep -f "redis-server.*6379" | while read -r p; do
    grep -q docker /proc/$p/cgroup 2>/dev/null && echo "$p" && break; done)
BASE_STACK=$(rss_kb "$STACK_PID")
redis-cli -p $PORT_STACK --pipe < "$PIPE" > "$OUT/stack-load.txt" 2>&1
sleep 3
RSS_STACK=$(rss_kb "$STACK_PID")
redis-cli -p $PORT_STACK info memory > "$OUT/stack-info-memory.txt" 2>&1 || true
redis-cli -p $PORT_STACK dbsize >> "$OUT/stack-load.txt" 2>&1
docker stop rss-ab-stack >/dev/null

SER=$(grep serialized_json_bytes "$OUT/serialized-bytes.txt" | cut -d= -f2)
{
    echo "serialized_json_bytes=$SER"
    echo "infinitydb_rss_kb_base=$BASE_INF rss_kb_loaded=$RSS_INF"
    echo "redis_stack_rss_kb_base=$BASE_STACK rss_kb_loaded=$RSS_STACK"
    python3 - "$SER" "$BASE_INF" "$RSS_INF" "$BASE_STACK" "$RSS_STACK" <<'EOF'
import sys
ser, bi, ri, bs, rs = map(int, sys.argv[1:6])
inf = (ri - bi) * 1024
stack = (rs - bs) * 1024
print(f"infinitydb_incremental_bytes={inf}")
print(f"redis_stack_incremental_bytes={stack}")
print(f"rss_over_serialized={inf/ser:.4f}  (gate <= 1.5)")
print(f"rss_vs_redis_stack={inf/stack:.4f}  (gate <= 0.7)")
EOF
} | tee "$OUT/verdict.txt"
echo DONE
