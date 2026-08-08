#!/usr/bin/env bash
# L1 in-place blob overwrite — wire A/B. ABAB across binaries (base =
# HEAD, l1 = lever), fresh server per arm, per-lane discarded warm-up
# leg before the 3 reps (the PROFILE.md warm-up finding folded into the
# protocol). Same wire methodology otherwise: server cores 0-7, memtier
# 12-23, 4t/25c, pipeline 16, 10 s legs.
set -euo pipefail
INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
SCRATCH=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/ac1a48dc-e61f-4ee2-a640-6595d350cc69/scratchpad
CORPUS=$SCRATCH/corpus
OUT=$INF/.artifacts/m3/jset-server-20260717
PORT=6400
DOC1K=$(cat "$CORPUS/gate-1KiB.json")

mtleg() {
    local file=$1; shift
    taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram \
        --threads 4 --clients 25 --pipeline 16 --test-time 10 \
        --key-maximum 100000 --distinct-client-seed "$@" >"$file" 2>&1
}

arm() {
    local bin=$1 tag=$2
    taskset -c 0-7 "$SCRATCH/$bin" --port $PORT >"$OUT/infinityd-$tag.log" 2>&1 &
    local SRV=$!
    for _ in $(seq 1 100); do redis-cli -p $PORT ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done
    taskset -c 12-23 memtier_benchmark -p $PORT --hide-histogram --threads 4 --clients 25 \
        --pipeline 32 --requests=allkeys --key-maximum 100000 --ratio=1:0 -d 1024 \
        --key-prefix="s1k-" >/dev/null 2>&1
    redis-cli -p $PORT --pipe < "$OUT/preload-docs.resp" >/dev/null 2>&1
    # Discarded warm-up legs, one per lane.
    mtleg "$OUT/ab-$tag-set-warm.txt"  --ratio=1:0 -d 1024 --key-prefix="s1k-"
    mtleg "$OUT/ab-$tag-jset-warm.txt" --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"
    for rep in 1 2 3; do
        mtleg "$OUT/ab-$tag-set-rep$rep.txt"  --ratio=1:0 -d 1024 --key-prefix="s1k-"
        mtleg "$OUT/ab-$tag-jset-rep$rep.txt" --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"
    done
    kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true
}

arm infinityd-base base-r1
arm infinityd-l1   l1-r1
arm infinityd-base base-r2
arm infinityd-l1   l1-r2

python3 - "$OUT" <<'EOF' | tee "$OUT/SUMMARY-ab.txt"
import glob, statistics, sys
out = sys.argv[1]
def ops(pattern):
    vals = []
    for path in sorted(glob.glob(f"{out}/{pattern}")):
        for line in open(path):
            if line.startswith("Totals"):
                vals.append(float(line.split()[1]))
    return vals
for armtag in ("base", "l1"):
    sets  = ops(f"ab-{armtag}-*-set-rep*.txt")
    jsets = ops(f"ab-{armtag}-*-jset-rep*.txt")
    ms, mj = statistics.mean(sets), statistics.mean(jsets)
    rs = statistics.stdev(sets)/ms*100
    rj = statistics.stdev(jsets)/mj*100
    print(f"{armtag:5s} set {ms:12,.0f} rsd {rs:.1f}%   jset {mj:12,.0f} rsd {rj:.1f}%   ratio {mj/ms:.4f}")
    print(f"      set reps  {sets}")
    print(f"      jset reps {jsets}")
EOF
