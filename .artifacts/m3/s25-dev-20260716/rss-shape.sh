#!/usr/bin/env bash
# Per-shape RSS decomposition: one shape, both engines. Usage: rss-shape.sh <shape> <count>
set -euo pipefail
SHAPE=$1; COUNT=$2
INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
SCRATCH=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/f7c996e7-230c-45c8-80f1-979984910087/scratchpad
CORPUS=$SCRATCH/corpus
OUT=$SCRATCH/rss-ab/shape-$SHAPE
PORT_INF=6401; PORT_STACK=6402
STACK_IMG=redis/redis-stack-server@sha256:798ab84d9f266936b034ab11c4d04a2b8e4b441884c5aa7d17ac951eefdf742a
mkdir -p "$OUT"

PIPE=$OUT/load.resp
python3 - "$CORPUS/$SHAPE.json" "$PIPE" "$SHAPE" "$COUNT" <<'EOF'
import sys
doc = open(sys.argv[1], "rb").read()
n = int(sys.argv[4])
with open(sys.argv[2], "wb") as f:
    for i in range(n):
        key = f"{sys.argv[3]}:{i}".encode()
        args = [b"JSON.SET", key, b"$", doc]
        f.write(b"*%d\r\n" % len(args))
        for a in args:
            f.write(b"$%d\r\n" % len(a)); f.write(a); f.write(b"\r\n")
print(f"serialized={len(doc)*n}")
EOF
SER=$(python3 -c "import os,sys; print(os.path.getsize('$CORPUS/$SHAPE.json') * $COUNT)")

rss_kb() { ps -o rss= -p "$1" | tr -d ' '; }

"$INF/target/release/infinityd" --port $PORT_INF >"$OUT/infinityd.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do redis-cli -p $PORT_INF ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
BI=$(rss_kb $SRV)
redis-cli -p $PORT_INF --pipe < "$PIPE" >"$OUT/inf-load.txt" 2>&1
sleep 2; RI=$(rss_kb $SRV)
kill $SRV; wait $SRV 2>/dev/null || true

docker run -d --rm --name rss-shape -p $PORT_STACK:6379 "$STACK_IMG" >/dev/null
for _ in $(seq 1 150); do redis-cli -p $PORT_STACK ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
SP=$(pgrep -f "redis-server.*6379" | while read -r p; do grep -q docker /proc/$p/cgroup 2>/dev/null && echo "$p" && break; done)
BS=$(rss_kb "$SP")
redis-cli -p $PORT_STACK --pipe < "$PIPE" >"$OUT/stack-load.txt" 2>&1
sleep 2; RS=$(rss_kb "$SP")
docker stop rss-shape >/dev/null

python3 - "$SHAPE" "$SER" "$BI" "$RI" "$BS" "$RS" <<'EOF' | tee "$OUT/verdict.txt"
import sys
shape, ser, bi, ri, bs, rs = sys.argv[1], *map(int, sys.argv[2:7])
inf = (ri - bi) * 1024; stack = (rs - bs) * 1024
print(f"{shape}: serialized={ser} inf={inf} stack={stack} "
      f"inf/serialized={inf/ser:.3f} inf/stack={inf/stack:.3f}")
EOF
