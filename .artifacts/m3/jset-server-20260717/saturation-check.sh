#!/usr/bin/env bash
# M3-S25 json_set server-side slice — generator-saturation evidence +
# honest 3-replicate rows. Finding under test: the blessed 4-thread
# memtier config saturates on the JSON.SET --command lane (per-request
# 1 KiB template substitution), clipping the jset row at ~1.24M ops/s
# while the server keeps headroom — a §19 "load-gen saturated" validity
# violation for the doc-write gate row. Fix under test: 8 generator
# threads (13 clients each; outstanding ops ~equal: 104x16 vs 100x16).
set -euo pipefail
INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
CORPUS=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/ac1a48dc-e61f-4ee2-a640-6595d350cc69/scratchpad/corpus
OUT=$INF/.artifacts/m3/jset-server-20260717
PORT=6400
REPS=3
DOC1K=$(cat "$CORPUS/gate-1KiB.json")

taskset -c 0-7 "$INF/target/release/infinityd" --port $PORT >"$OUT/infinityd-sat.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do redis-cli -p $PORT ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done

taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram --threads 4 --clients 25 \
    --pipeline 32 --requests=allkeys --key-maximum 100000 --ratio=1:0 -d 1024 \
    --key-prefix="s1k-" >"$OUT/sat-preload-s1k.txt" 2>&1
redis-cli -p $PORT --pipe < "$OUT/preload-docs.resp" >"$OUT/sat-preload-docs.txt" 2>&1

# Utilization evidence legs (one each, 20 s, sampled mid-run).
utilization_leg() {
    local name=$1; shift
    taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram \
        --pipeline 16 --test-time 20 --key-maximum 100000 --distinct-client-seed "$@" \
        >"$OUT/sat-$name.txt" 2>&1 &
    local MT=$!
    sleep 6
    mpstat -P 0-7 5 1 > "$OUT/sat-$name-servercpu.txt" 2>&1 &
    pidstat -t -p "$(pgrep -x memtier_benchmark)" 5 1 > "$OUT/sat-$name-memtiercpu.txt" 2>&1
    wait $MT
}
utilization_leg jset-4t --threads 4 --clients 25 --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"
utilization_leg set-4t  --threads 4 --clients 25 --ratio=1:0 -d 1024 --key-prefix="s1k-"
utilization_leg jset-8t --threads 8 --clients 13 --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"

# Honest 3-replicate rows, both lanes, same 8-thread generator config.
mt8() {
    local name=$1; shift
    for rep in $(seq 1 $REPS); do
        taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram \
            --threads 8 --clients 13 --pipeline 16 --test-time 10 \
            --key-maximum 100000 --distinct-client-seed "$@" \
            >"$OUT/hon-$name-rep$rep.txt" 2>&1
    done
}
mt8 set-1k  --ratio=1:0 -d 1024 --key-prefix="s1k-"
mt8 jset-1k --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"

kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true

python3 - "$OUT" <<'EOF' | tee "$OUT/SUMMARY-honest.txt"
import glob, statistics, sys
out = sys.argv[1]
means = {}
for name in ("set-1k", "jset-1k"):
    ops = []
    for path in sorted(glob.glob(f"{out}/hon-{name}-rep*.txt")):
        for line in open(path):
            if line.startswith("Totals"):
                ops.append(float(line.split()[1]))
    m = statistics.mean(ops)
    rsd = statistics.stdev(ops) / m * 100 if len(ops) > 1 else 0.0
    means[name] = m
    print(f"{name:8s} ops/s mean {m:12,.0f} rsd {rsd:.1f}%  reps {ops}")
print(f"honest JSON.SET/SET (8t generator) = {means['jset-1k']/means['set-1k']:.4f}  (gate >= 0.70)")
EOF
