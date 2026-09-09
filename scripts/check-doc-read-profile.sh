#!/usr/bin/env bash
# M3-S25 parser-symbol profile check (the parse-free-read proof, now a
# script): profiles the serving cores during a live JSON.GET wire row and
# fails if any JSON-text-parser or path-compiler symbol appears — text
# parsing sneaking onto the read path is the §7 read gate's named risk.
#
# Review 2026-08-30, F-L20-08 (ADR-0106 fourth amendment, D13): the
# verdict is a measurement only when the profile proves the load reached
# the serving cores. An empty report, a report with too few symbol rows or
# samples, or one without the expected tape-traversal / JSON.GET symbols
# is FAIL — never "zero parser symbols in 0 rows". The banned-symbol scan
# reads a flat report at `--percent-limit 0`, so a parser at 0.01% is
# seen, not filtered out by perf before the grep. This is a manual
# reference-box step (perf, memtier_benchmark, pinned cores); no workflow
# runs it — the claim ledger (C25) says so.
#
# Usage: check-doc-read-profile.sh [out-dir]
# Env:   SERVER_CPUS (default 0-7), LOAD_CPUS (default 12-23)
#        MIN_ROWS (default 200), MIN_SAMPLES (default 10000)
#        INF_PROFILE_REPORT=<flat perf report>: verdict only, no profiling
#        (the self-test hook — `check-scripts-selftest.sh` plants reports)
set -euo pipefail
OUT="${1:-.artifacts/m3/read-profile-$(date +%Y%m%d-%H%M)}"
PORT="${PORT:-6400}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOAD_CPUS="${LOAD_CPUS:-12-23}"
KEYMAX=100000
SEED=0x1D0C2026
MIN_ROWS="${MIN_ROWS:-200}"
MIN_SAMPLES="${MIN_SAMPLES:-10000}"

# The banned symbol classes: the JSON text parser, its stage-1 scan, and
# the JSONPath compiler.
BANNED='JsonParser|parse_into|parse_indexed|json_scan_structurals|path::compile|PathCompiler'
# The positive control: a live JSON.GET load must put the tape traversal
# and the JSON.GET handler on the serving cores. Every pattern must match.
EXPECTED='inf_doc::tape::ObjIter
inf_doc::tape::read_value
json_get'

# verdict <flat report> <out dir>: PASS/FAIL to stdout and verdict.txt.
verdict() {
    local report=$1 out=$2
    if [ ! -s "$report" ]; then
        echo "FAIL: empty profile report ($report) — nothing was measured" | tee "$out/verdict.txt"
        return 1
    fi
    local samples rows
    # "# Samples: 68K of event 'cycles'" → 68000 (perf abbreviates K/M/G).
    samples=$(sed -n "s/^# Samples: *\([0-9][0-9]*\)\([KMG]\{0,1\}\) .*/\1 \2/p" "$report" | head -1 |
        awk '{ m = 1; if ($2 == "K") m = 1000; if ($2 == "M") m = 1000000; if ($2 == "G") m = 1000000000; print $1 * m }')
    samples=${samples:-0}
    rows=$(grep -cE '^[[:space:]]+[0-9]+\.[0-9]+%' "$report" || true)
    if [ "$samples" -lt "$MIN_SAMPLES" ]; then
        echo "FAIL: $samples samples in the profile (need ≥ $MIN_SAMPLES) — the load did not reach the serving cores" | tee "$out/verdict.txt"
        return 1
    fi
    if [ "$rows" -lt "$MIN_ROWS" ]; then
        echo "FAIL: $rows symbol rows in the profile (need ≥ $MIN_ROWS) — the load did not reach the serving cores" | tee "$out/verdict.txt"
        return 1
    fi
    local missing="" pattern
    while IFS= read -r pattern; do
        [ -n "$pattern" ] || continue
        grep -qE -- "$pattern" "$report" || missing="$missing $pattern"
    done <<<"$EXPECTED"
    if [ -n "$missing" ]; then
        echo "FAIL: positive control missing —$missing not on the profiled cores (wrong CPUs, or the load never ran JSON.GET)" | tee "$out/verdict.txt"
        return 1
    fi
    if grep -E "$BANNED" "$report" >"$out/banned-hits.txt"; then
        echo "FAIL: parser/compiler symbols on the read path:" | tee "$out/verdict.txt"
        cat "$out/banned-hits.txt"
        return 1
    fi
    echo "PASS: zero parser/compiler symbols in $rows symbol rows, $samples samples, every symbol at percent-limit 0; positive control present ($(echo "$EXPECTED" | tr '\n' ' ' | sed 's/ $//')) ($report)" | tee "$out/verdict.txt"
}

mkdir -p "$OUT"
if [ -n "${INF_PROFILE_REPORT:-}" ]; then
    if verdict "$INF_PROFILE_REPORT" "$OUT"; then exit 0; else exit 1; fi
fi

WORK="${WORK:-$(mktemp -d)}"
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
# Two extractions: the call-graph view a reader cites (perf's default
# 0.05% floor keeps it readable), and the flat per-symbol report at
# percent-limit 0 that the verdict reads — every symbol with a sample.
perf report -i "$OUT/jget-read.perf" --stdio --percent-limit 0.05 \
    >"$OUT/jget-read-report.txt" 2>/dev/null
perf report -i "$OUT/jget-read.perf" --stdio --no-children -g none --percent-limit 0 \
    >"$OUT/jget-read-flat.txt" 2>/dev/null
# The raw sample file is machine-local intermediate data, not evidence:
# this one runs ~1 GB, which is 10x GitHub's hard file limit and blocked a
# push on 2026-08-16. The extracted reports above are what the gate reads
# and what a reviewer cites, so drop the raw file once extracted.
# `KEEP_PERF_DATA=1` retains it for local debugging (it is gitignored
# either way).
PERF_BYTES=$(stat -c %s "$OUT/jget-read.perf" 2>/dev/null || echo 0)
if [ "${KEEP_PERF_DATA:-0}" = "1" ]; then
    echo "raw perf data retained at $OUT/jget-read.perf ($PERF_BYTES B) — gitignored" >>"$OUT/perf.log"
else
    rm -f "$OUT/jget-read.perf"
    echo "raw perf data ($PERF_BYTES B) discarded after extraction; set KEEP_PERF_DATA=1 to retain" >>"$OUT/perf.log"
fi

kill $SRV 2>/dev/null || true; wait $SRV 2>/dev/null || true

if verdict "$OUT/jget-read-flat.txt" "$OUT"; then exit 0; else exit 1; fi
