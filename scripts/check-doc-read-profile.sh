#!/usr/bin/env bash
# M3-S25 parser-symbol profile check (the parse-free-read proof, now a
# script): profiles the serving cores during a live JSON.GET wire row and
# fails if any JSON-text-parser or path-compiler symbol appears — text
# parsing sneaking onto the read path is the §7 read gate's named risk.
#
# Usage: check-doc-read-profile.sh [out-dir]
# Env:   SERVER_CPUS (default 0-7), LOAD_CPUS (default 12-23)
set -euo pipefail
OUT="${1:-.artifacts/m3/read-profile-$(date +%Y%m%d-%H%M)}"
PORT="${PORT:-6400}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOAD_CPUS="${LOAD_CPUS:-12-23}"
KEYMAX=100000
SEED=0x1D0C2026
WORK="${WORK:-$(mktemp -d)}"
mkdir -p "$OUT"

cargo build --release -p infinityd -p inf-bench >/dev/null
./target/release/inf-bench doc-corpus --seed $SEED --out "$WORK/corpus" >/dev/null

taskset -c "$SERVER_CPUS" ./target/release/infinityd --port "$PORT" >"$OUT/infinityd.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do redis-cli -p "$PORT" ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done

python3 - "$WORK/corpus/gate-1KiB.json" "$WORK/preload.resp" $KEYMAX <<'EOF'
import sys
doc = open(sys.argv[1], "rb").read()
with open(sys.argv[2], "wb") as f:
    for i in range(int(sys.argv[3]) + 1):
        args = [b"JSON.SET", b"d1k-" + str(i).encode(), b"$", doc]
        f.write(b"*%d\r\n" % len(args))
        for a in args:
            f.write(b"$%d\r\n" % len(a)); f.write(a); f.write(b"\r\n")
EOF
redis-cli -p "$PORT" --pipe < "$WORK/preload.resp" >"$OUT/preload.txt" 2>&1

taskset -c "$LOAD_CPUS" memtier_benchmark -p "$PORT" --hide-histogram \
    --threads 4 --clients 25 --pipeline 16 --test-time 12 --key-maximum $KEYMAX \
    --key-prefix="d1k-" --command="JSON.GET __key__ \$.child.child.child.child.id" \
    --command-key-pattern=R >"$OUT/load.txt" 2>&1 &
LOADPID=$!
sleep 1
perf record -C "$SERVER_CPUS" -F 1997 -g --call-graph dwarf,16384 \
    -o "$OUT/jget-read.perf" -- sleep 8 >>"$OUT/perf.log" 2>&1
wait $LOADPID
perf report -i "$OUT/jget-read.perf" --stdio --percent-limit 0.05 \
    >"$OUT/jget-read-report.txt" 2>/dev/null

kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true

# The banned symbol classes: the JSON text parser, its stage-1 scan, and
# the JSONPath compiler. Tape traversal (ObjIter, read_value) is expected.
BANNED='JsonParser|parse_into|parse_indexed|json_scan_structurals|path::compile|PathCompiler'
if grep -E "$BANNED" "$OUT/jget-read-report.txt" > "$OUT/banned-hits.txt"; then
    echo "FAIL: parser/compiler symbols on the read path:" | tee "$OUT/verdict.txt"
    cat "$OUT/banned-hits.txt"
    exit 1
fi
ROWS=$(grep -cE "^\s+[0-9]" "$OUT/jget-read-report.txt" || true)
echo "PASS: zero parser/compiler symbols in $ROWS report rows ($OUT/jget-read-report.txt)" | tee "$OUT/verdict.txt"
