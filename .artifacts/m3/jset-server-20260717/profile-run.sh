#!/usr/bin/env bash
# M3-S25 server-side json_set slice — opening profile (ledger-claimed
# 2026-07-17). Same wire methodology as adr0047 wire2: server P-cores
# 0-7, memtier 12-23, 4x25 conns, pipeline 16, 10 s legs, 3 reps for
# throughput rows; perf attribution legs sampled on the serving cores.
set -euo pipefail
INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
CORPUS=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/ac1a48dc-e61f-4ee2-a640-6595d350cc69/scratchpad/corpus
OUT=$INF/.artifacts/m3/jset-server-20260717
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

# One long leg with perf sampling the serving cores mid-run.
mt_perf() {
    local name=$1; shift
    taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram \
        --threads 4 --clients 25 --pipeline 16 --test-time 20 \
        --key-maximum 100000 --distinct-client-seed "$@" \
        >"$OUT/$name-perfleg.txt" 2>&1 &
    local MT=$!
    sleep 5
    perf record -C 0-7 -F 2000 -o "$OUT/perf-$name.data" -- sleep 10 >/dev/null 2>&1
    wait $MT
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
mt_perf set-1k  --ratio=1:0 -d 1024 --key-prefix="s1k-"
mt_perf jset-1k --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"

kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true

python3 - "$OUT" <<'EOF' | tee "$OUT/SUMMARY-baseline.txt"
import glob, statistics, sys
out = sys.argv[1]
means = {}
for name in ("set-1k", "jset-1k"):
    ops = []
    for path in sorted(glob.glob(f"{out}/{name}-rep*.txt")):
        for line in open(path):
            if line.startswith("Totals"):
                ops.append(float(line.split()[1]))
    m = statistics.mean(ops)
    rsd = statistics.stdev(ops) / m * 100 if len(ops) > 1 else 0.0
    means[name] = m
    print(f"{name:8s} ops/s mean {m:12,.0f} rsd {rsd:.1f}%  reps {len(ops)}")
print(f"JSON.SET/SET throughput = {means['jset-1k']/means['set-1k']:.4f}  (gate >= 0.70)")
EOF
