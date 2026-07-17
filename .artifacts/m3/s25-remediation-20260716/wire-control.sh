#!/usr/bin/env bash
# ADR-0046 no-regression wire controls on the candidate binary: the gate
# shapes are <= 4 KiB (placement unchanged by the ADR), so these rows must
# match the s25-dev-20260716 baseline within noise. Same methodology:
# server P-cores 0-7, memtier 12-23, 4x25 conns, pipeline 16, 10 s, 3 reps.
set -euo pipefail
INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
CORPUS=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/f7c996e7-230c-45c8-80f1-979984910087/scratchpad/corpus
OUT=$INF/.artifacts/m3/s25-remediation-20260716/wire-control
PORT=6400
REPS=3
mkdir -p "$OUT"
DOC1K=$(cat "$CORPUS/gate-1KiB.json")

taskset -c 0-7 "$INF/target/release/infinityd" --port $PORT >"$OUT/infinityd.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do redis-cli -p $PORT ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done

mt() {
    local name=$1; shift
    for rep in $(seq 1 $REPS); do
        taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram \
            --threads 4 --clients 25 --pipeline 16 --test-time 10 \
            --key-maximum 100000 --distinct-client-seed "$@" \
            >"$OUT/$name-rep$rep.txt" 2>&1
    done
}

taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram --threads 4 --clients 25 \
    --pipeline 32 --requests=allkeys --key-maximum 100000 --ratio=1:0 -d 1024 \
    --key-prefix="s1k-" >"$OUT/preload-s1k.txt" 2>&1
python3 - "$CORPUS/gate-1KiB.json" "$OUT/preload-docs.resp" <<'EOF'
import sys
doc = open(sys.argv[1], "rb").read()
with open(sys.argv[2], "wb") as f:
    for i in range(100001):
        args = [b"JSON.SET", b"d1k-" + str(i).encode(), b"$", doc]
        f.write(b"*%d\r\n" % len(args))
        for a in args:
            f.write(b"$%d\r\n" % len(a)); f.write(a); f.write(b"\r\n")
EOF
redis-cli -p $PORT --pipe < "$OUT/preload-docs.resp" >"$OUT/preload-docs.txt" 2>&1

mt set-1k  --ratio=1:0 -d 1024 --key-prefix="s1k-"
mt jset-1k --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"
mt get-1k  --ratio=0:1 -d 1024 --key-prefix="s1k-"
mt jget-1k --command="JSON.GET __key__ \$.child.child.child.child.id" --command-key-pattern=R --key-prefix="d1k-"

kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true

python3 - "$OUT" <<'EOF' | tee "$OUT/SUMMARY.txt"
import glob, re, statistics, sys
out = sys.argv[1]
rows = {}
for name in ("set-1k", "jset-1k", "get-1k", "jget-1k"):
    ops, p50 = [], []
    for path in sorted(glob.glob(f"{out}/{name}-rep*.txt")):
        for line in open(path):
            if line.startswith("Totals"):
                parts = line.split()
                ops.append(float(parts[1])); p50.append(float(parts[5]))
    rows[name] = (statistics.mean(ops), statistics.pstdev(ops) / statistics.mean(ops), statistics.mean(p50))
    print(f"{name:8s} ops/s mean {rows[name][0]:>12,.0f} rsd {rows[name][1]*100:.1f}%  p50 {rows[name][2]:.3f} ms")
print(f"JSON.SET/SET throughput = {rows['jset-1k'][0]/rows['set-1k'][0]:.4f}  (gate >= 0.70; baseline 0.6020)")
print(f"JSON.GET/GET p50 (1KiB) = {rows['jget-1k'][2]/rows['get-1k'][2]:.4f}  (gate <= 1.5; baseline 1.1988)")
EOF
