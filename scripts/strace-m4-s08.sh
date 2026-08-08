#!/usr/bin/env bash
# M4-S08 AC: zero blocking file-read syscalls on the data plane during
# cold storms (§3.3 "no synchronous disk read anywhere on the data
# plane"). Runs the hardened cold-read integration tests (real io_uring,
# registered pool, chunked staging, cancellation) under strace and
# counts the blocking positional-read family — the count must be zero —
# with io_uring_enter as the positive control (the reads really ran, and
# they ran through the ring).
#
# Artifact: pass a path to tee the summary into
# .artifacts/m4/s08/strace-cold-storm.txt (dev-tier evidence).
set -euo pipefail
cd "$(dirname "$0")/.."

out="${1:-/dev/stdout}"

command -v strace >/dev/null || { echo "strace not installed"; exit 2; }

# Build first so the traced process is the test run, not rustc.
cargo test -p inf-runtime --features uring --test cold_hardened --no-run >/dev/null 2>&1
bin=$(cargo test -p inf-runtime --features uring --test cold_hardened --no-run 2>&1 \
    | grep -oE 'target/debug/deps/cold_hardened-[0-9a-f]+' | head -1)
[ -n "$bin" ] || { echo "test binary not found"; exit 2; }

# Plain event trace: the dynamic loader legitimately pread64s the ELF
# program headers before main (pre-"running N tests"); the verdict
# covers everything from the harness banner onward — the storm itself.
trace=$(mktemp)
strace -f -o "$trace" -e trace=pread64,preadv,preadv2,io_uring_enter,write \
    "$bin" --test-threads=1 >/dev/null 2>&1

banner_line=$(grep -n 'running' "$trace" | grep 'write' | head -1 | cut -d: -f1)
[ -n "$banner_line" ] || { echo "FAIL: test banner never observed"; rm -f "$trace"; exit 1; }
storm_preads=$(tail -n +"$banner_line" "$trace" | grep -cE '\bpread(64|v|v2)?\(' || true)
loader_preads=$(head -n "$banner_line" "$trace" | grep -cE '\bpread(64|v|v2)?\(' || true)
enters=$(tail -n +"$banner_line" "$trace" | grep -c 'io_uring_enter' || true)

{
    echo "# M4-S08 strace artifact — cold storms on real io_uring"
    echo "# binary: $bin"
    echo "# date: $(date -u +%FT%TZ)  (dev tier; the §3.3 grep + this run are box-independent)"
    echo "# pre-main loader preads (ELF phdrs, excluded from the verdict): $loader_preads"
    echo "storm pread-class syscalls: $storm_preads (gate: 0)"
    echo "storm io_uring_enter calls: $enters (positive control: > 0)"
} | tee "$out"
rm -f "$trace"

if [ "$storm_preads" -ne 0 ]; then
    echo "FAIL: blocking pread-class syscalls on the data plane"
    exit 1
fi
if [ "$enters" -eq 0 ]; then
    echo "FAIL: no io_uring_enter observed — the storm did not run through the ring"
    exit 1
fi
echo "strace OK: zero blocking positional reads; cold reads rode io_uring"
