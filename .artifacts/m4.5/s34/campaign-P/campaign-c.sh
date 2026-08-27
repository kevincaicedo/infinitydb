#!/usr/bin/env bash
# S34 campaign P chain c (2026-08-27): P3 S18 10 GB (P0–P2 done in chains a/b).
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s34/campaign-P
DATA=$HOME/bench-data/s34/data-P
LOG=$ROOT/campaign.log
mkdir -p "$DATA" "$ROOT/stderr"
export INF_GATERUN_STDERR_DIR=$ROOT/stderr
hdr() { echo "# $(date -Is) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ') gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) no_turbo=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo) strays=$(pgrep -c -x infinityd || true) load=$(cut -d' ' -f1-3 /proc/loadavg) devstat=$(cat /sys/block/nvme0n1/stat | tr -s ' ') df=$(df --output=pcent / | tail -n 1 | tr -d ' ')"; }
armflags() { if [ "$1" = flush ]; then echo "--barrier-class flush --model-absent"; else echo "--barrier-class fua"; fi; }
echo "# RESTART (chain c): P3 (17:24) was refused by env-check on a dirty tree — two governing-doc edits (ARCHITECTURE.md, ops-tiered-storage.md) landed in the checkout; moved to the worktree; P0-P2 stand" | tee -a "$LOG"
echo "# (chain b note kept:: the 16:57 P1 legs flush-0/fua-0 were refused by env-check on a dirty tree (an S37 edit landed in the checkout mid-chain — stashed; development moved to a worktree); P0 stands, P1-P3 re-run from the top in the predeclared order" | tee -a "$LOG"
hdr | tee -a "$LOG"
# P3 — the S18 row at 10 GB (cold, both arms).
echo "=== $(date -Is) P3 s39d S18 10 GB cold ===" | tee -a "$LOG"
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s39d --reference-box --cells 4 --pin-start 0 \
  --barrier-class fua --s39d-baseline flush-class --s39d-cold-boot --replicates 3 --leg-idle-s 40 \
  --s39d-warm-records 10000000 --s39d-tail-records 200000 --device-stat nvme0n1 \
  --data-root "$DATA" --artifacts-root "$ROOT/p3-s18-10g" >> "$LOG" 2>&1
echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
hdr | tee -a "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
echo "CHAIN DONE $(date -Is)" >> "$LOG"
