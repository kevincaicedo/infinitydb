#!/usr/bin/env bash
# M3 §7 wire gate rows (blessed S25 harness — labeled memtier instrument;
# inf-compare's json lanes are the independent-generator cross-check).
# Rows: JSON.SET vs SET (1 KiB, the write gate), JSON.GET vs GET p50
# (1 KiB + 200 B, the read gate), and the path-mutation row.
#
# Usage: bench-m3-wire.sh [out-dir]
# Env:   SERVER_CPUS (default 0-7), LOAD_CPUS (default 12-23), REPS (3)
set -euo pipefail
OUT="${1:-.artifacts/m3/wire-$(date +%Y%m%d-%H%M)}"
PORT="${PORT:-6400}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOAD_CPUS="${LOAD_CPUS:-12-23}"
REPS="${REPS:-3}"
SECS="${SECS:-10}"
KEYMAX=100000
SEED=0x1D0C2026
WORK="${WORK:-$(mktemp -d)}"
mkdir -p "$OUT"

cargo build --release -p infinityd -p inf-bench >/dev/null
./target/release/inf-bench doc-corpus --seed $SEED --out "$WORK/corpus" >/dev/null
DOC1K=$(cat "$WORK/corpus/gate-1KiB.json")

taskset -c "$SERVER_CPUS" ./target/release/infinityd --port "$PORT" >"$OUT/infinityd.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do redis-cli -p "$PORT" ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done

mt() {
    local name=$1; shift
    for rep in $(seq 1 "$REPS"); do
        taskset -c "$LOAD_CPUS" memtier_benchmark -p "$PORT" --hide-histogram \
            --threads 4 --clients 25 --pipeline 16 --test-time "$SECS" \
            --key-maximum $KEYMAX --distinct-client-seed "$@" \
            >"$OUT/$name-rep$rep.txt" 2>&1
    done
}

taskset -c "$LOAD_CPUS" memtier_benchmark -p "$PORT" --hide-histogram --threads 4 --clients 25 \
    --pipeline 32 --requests=allkeys --key-maximum $KEYMAX --ratio=1:0 -d 1024 \
    --key-prefix="s1k-" >"$OUT/preload-s1k.txt" 2>&1
taskset -c "$LOAD_CPUS" memtier_benchmark -p "$PORT" --hide-histogram --threads 4 --clients 25 \
    --pipeline 32 --requests=allkeys --key-maximum $KEYMAX --ratio=1:0 -d 200 \
    --key-prefix="s2h-" >"$OUT/preload-s2h.txt" 2>&1
python3 - "$WORK/corpus" "$WORK/preload-docs.resp" $KEYMAX <<'EOF'
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
redis-cli -p "$PORT" --pipe < "$WORK/preload-docs.resp" >"$OUT/preload-docs.txt" 2>&1

mt set-1k  --ratio=1:0 -d 1024 --key-prefix="s1k-"
mt jset-1k --command="JSON.SET __key__ \$ '$DOC1K'" --command-key-pattern=R --key-prefix="d1k-"
mt get-1k  --ratio=0:1 -d 1024 --key-prefix="s1k-"
mt jget-1k --command="JSON.GET __key__ \$.child.child.child.child.id" --command-key-pattern=R --key-prefix="d1k-"
mt get-2h  --ratio=0:1 -d 200 --key-prefix="s2h-"
mt jget-2h --command="JSON.GET __key__ \$.name" --command-key-pattern=R --key-prefix="d2h-"
mt jmut-1k --command="JSON.NUMINCRBY __key__ \$.score 1" --command-key-pattern=R --key-prefix="d1k-"

kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true

python3 - "$OUT" <<'EOF' | tee "$OUT/SUMMARY.txt"
import glob, statistics, sys
out = sys.argv[1]
rows = {}
for name in ("set-1k", "jset-1k", "get-1k", "jget-1k", "get-2h", "jget-2h", "jmut-1k"):
    ops, p50 = [], []
    for path in sorted(glob.glob(f"{out}/{name}-rep*.txt")):
        for line in open(path):
            if line.startswith("Totals"):
                parts = line.split()
                ops.append(float(parts[1])); p50.append(float(parts[5]))
    if not ops:
        continue
    rows[name] = (statistics.mean(ops), statistics.pstdev(ops) / statistics.mean(ops), statistics.mean(p50))
    print(f"{name:8s} ops/s mean {rows[name][0]:>12,.0f} rsd {rows[name][1]*100:.1f}%  p50 {rows[name][2]:.3f} ms")
print(f"GATE doc_write_throughput_ratio = {rows['jset-1k'][0]/rows['set-1k'][0]:.4f}  (>= 0.70)")
print(f"GATE doc_read_p50_ratio_1k      = {rows['jget-1k'][2]/rows['get-1k'][2]:.4f}  (<= 1.5)")
print(f"GATE doc_read_p50_ratio_200b    = {rows['jget-2h'][2]/rows['get-2h'][2]:.4f}  (<= 1.5)")
EOF
echo "bench-m3-wire: artifacts in $OUT"
