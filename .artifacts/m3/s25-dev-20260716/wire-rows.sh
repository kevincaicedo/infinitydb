#!/usr/bin/env bash
# M3-S25 dev-tier wire rows (hand-run memtier harness — labeled, per §19
# "no silent substitution": inf-compare json lanes do not exist yet).
# Gates: JSON.GET $.path p50 <= 1.5x plain GET (200B + 1KiB shapes);
#        JSON.SET >= 70% plain SET throughput (memory-mode namespace).
set -euo pipefail

INF=/home/kcaicedo/Documents/Projects/databases/infinitydb
SCRATCH=/tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/f7c996e7-230c-45c8-80f1-979984910087/scratchpad
CORPUS=$SCRATCH/corpus
OUT=$SCRATCH/wire-rows
PORT=6400
SERVER_CPUS=0-7
LOAD_CPUS=12-23
REPS=3
SECS=10
THREADS=4
CONNS=25          # per thread => 100 connections
PIPELINE=16
KEYMAX=100000

mkdir -p "$OUT"
DOC1K=$(cat "$CORPUS/gate-1KiB.json")
DOC200=$(cat "$CORPUS/small-200B.json")

start_server() {
    taskset -c $SERVER_CPUS "$INF/target/release/infinityd" --port $PORT \
        >"$OUT/infinityd.log" 2>&1 &
    SRV=$!
    for _ in $(seq 1 100); do
        redis-cli -p $PORT ping 2>/dev/null | grep -q PONG && return
        sleep 0.2
    done
    echo "server failed to start" >&2; exit 1
}

mt() { # name, extra memtier args...
    local name=$1; shift
    for rep in $(seq 1 $REPS); do
        taskset -c $LOAD_CPUS memtier_benchmark -p $PORT --hide-histogram \
            --threads $THREADS --clients $CONNS --pipeline $PIPELINE \
            --test-time $SECS --key-maximum $KEYMAX --distinct-client-seed \
            --json-out-file "$OUT/$name-rep$rep.json" "$@" \
            >"$OUT/$name-rep$rep.txt" 2>&1
    done
}

start_server
echo "== preload string keys (1KiB + 200B under two prefixes) =="
taskset -c $LOAD_CPUS memtier_benchmark -p $PORT --hide-histogram \
    --threads $THREADS --clients $CONNS --pipeline 32 --requests=allkeys \
    --key-maximum $KEYMAX --ratio=1:0 -d 1024 --key-prefix="s1k-" \
    >"$OUT/preload-s1k.txt" 2>&1
taskset -c $LOAD_CPUS memtier_benchmark -p $PORT --hide-histogram \
    --threads $THREADS --clients $CONNS --pipeline 32 --requests=allkeys \
    --key-maximum $KEYMAX --ratio=1:0 -d 200 --key-prefix="s2h-" \
    >"$OUT/preload-s2h.txt" 2>&1
echo "== preload documents (1KiB gate shape + 200B shape, via pipe) =="
python3 - "$CORPUS" "$OUT/preload-docs.resp" $KEYMAX <<'EOF'
import sys, os
corpus, out, keymax = sys.argv[1], sys.argv[2], int(sys.argv[3])
d1k = open(os.path.join(corpus, "gate-1KiB.json"), "rb").read()
d2h = open(os.path.join(corpus, "small-200B.json"), "rb").read()
with open(out, "wb") as f:
    for prefix, doc in ((b"d1k-", d1k), (b"d2h-", d2h)):
        for i in range(keymax + 1):
            args = [b"JSON.SET", prefix + str(i).encode(), b"$", doc]
            f.write(b"*%d\r\n" % len(args))
            for a in args:
                f.write(b"$%d\r\n" % len(a)); f.write(a); f.write(b"\r\n")
EOF
redis-cli -p $PORT --pipe < "$OUT/preload-docs.resp" > "$OUT/preload-docs.txt" 2>&1

echo "== throughput rows: plain SET vs JSON.SET (1KiB shape) =="
mt set-1k   --ratio=1:0 -d 1024 --key-prefix="s1k-"
mt jset-1k  --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"

echo "== latency rows: plain GET vs JSON.GET path (1KiB + 200B) =="
mt get-1k   --ratio=0:1 -d 1024 --key-prefix="s1k-"
mt jget-1k  --command="JSON.GET __key__ \$.child.child.child.child.id" --command-key-pattern=R --key-prefix="d1k-"
mt get-2h   --ratio=0:1 -d 200 --key-prefix="s2h-"
mt jget-2h  --command="JSON.GET __key__ \$.name" --command-key-pattern=R --key-prefix="d2h-"

echo "== path-mutation mix row (NUMINCRBY multi-match on 1KiB docs) =="
mt jmut-1k  --command="JSON.NUMINCRBY __key__ \$.score 1" --command-key-pattern=R --key-prefix="d1k-"

echo "== mixed 50/50 KV+JSON row =="
for rep in $(seq 1 $REPS); do
    taskset -c $LOAD_CPUS memtier_benchmark -p $PORT --hide-histogram \
        --threads 2 --clients $CONNS --pipeline $PIPELINE --test-time $SECS \
        --key-maximum $KEYMAX --ratio=1:1 -d 1024 --key-prefix="s1k-" \
        --json-out-file "$OUT/mix-kv-rep$rep.json" >"$OUT/mix-kv-rep$rep.txt" 2>&1 &
    KV=$!
    taskset -c $LOAD_CPUS memtier_benchmark -p $PORT --hide-histogram \
        --threads 2 --clients $CONNS --pipeline $PIPELINE --test-time $SECS \
        --key-maximum $KEYMAX --key-prefix="d1k-" \
        --command="JSON.GET __key__ \$.child.child.child.child.id" --command-key-pattern=R \
        --json-out-file "$OUT/mix-json-rep$rep.json" >"$OUT/mix-json-rep$rep.txt" 2>&1 &
    JS=$!
    wait $KV $JS
done

echo "== perf profile row: JSON.GET read path (parser-symbol check) =="
taskset -c $LOAD_CPUS memtier_benchmark -p $PORT --hide-histogram \
    --threads $THREADS --clients $CONNS --pipeline $PIPELINE --test-time 12 \
    --key-maximum $KEYMAX --key-prefix="d1k-" \
    --command="JSON.GET __key__ \$.child.child.child.child.id" --command-key-pattern=R \
    >"$OUT/profile-load.txt" 2>&1 &
LOADPID=$!
sleep 1
perf record -C $SERVER_CPUS -F 1997 -g --call-graph dwarf,16384 \
    -o "$OUT/jget-read.perf" -- sleep 8 >>"$OUT/perf.log" 2>&1
wait $LOADPID
perf report -i "$OUT/jget-read.perf" --stdio --percent-limit 0.05 \
    >"$OUT/jget-read-report.txt" 2>/dev/null

kill $SRV 2>/dev/null || true
wait $SRV 2>/dev/null || true
echo DONE
