#!/usr/bin/env bash
# S34 campaign P (2026-08-27): P0 10k durable sweep → P1 everysec post-trim → P2 cold replay → P3 S18 10 GB. README.md carries the predeclared rules.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s34/campaign-P
DATA=$HOME/bench-data/s34/data-P
LOG=$ROOT/campaign.log
mkdir -p "$DATA" "$ROOT/stderr"
export INF_GATERUN_STDERR_DIR=$ROOT/stderr
hdr() { echo "# $(date -Is) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ') gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) no_turbo=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo) strays=$(pgrep -c -x infinityd || true) load=$(cut -d' ' -f1-3 /proc/loadavg) devstat=$(cat /sys/block/nvme0n1/stat | tr -s ' ') df=$(df --output=pcent / | tail -n 1 | tr -d ' ')"; }
armflags() { if [ "$1" = flush ]; then echo "--barrier-class flush --model-absent"; else echo "--barrier-class fua"; fi; }
hdr | tee "$LOG"
# The probe file with the read row, measured on the trimmed drive.
echo "=== $(date -Is) probe-device ===" | tee -a "$LOG"
rm -f "$DATA/io-properties.toml"
./target/release/inf probe-device "$DATA" --seconds 2 >> "$LOG" 2>&1
grep -E "^(barrier_class|read_bytes_per_s_256k|write_bytes_per_s_256k|fua_max_frame_bytes|probe_schema) " "$DATA/io-properties.toml" | tee -a "$LOG"
# P0 — the 10k durable sweep (one job; each shard capped).
echo "=== $(date -Is) P0 durable-sweep 10000 ===" | tee -a "$LOG"
SWEEP=$ROOT/p0-sweep
mkdir -p "$SWEEP"
for i in 0 1 2 3 4 5 6 7; do
  ( ulimit -v 4194304; nice -n 10 ./target/release/inf-sim --scenario m2-durable --sweep 10000 --seed 0xD5EE0000 --shard "$i/8" --out "$SWEEP" > "$SWEEP/shard-$i.log" 2>&1 ) &
done
wait
cat "$SWEEP"/manifest-shard-*.txt | tee -a "$LOG"
echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
sleep 40
# P1 — everysec penalty row, post-trim.
for spec in flush-0 fua-0 fua-1 flush-1 flush-2 fua-2; do
  arm=${spec%-*}
  echo "=== $(date -Is) P1 everysec $spec ===" | tee -a "$LOG"
  sleep 40
  taskset -c 8-23 ./target/release/inf-bench gate-run m2 --only-everysec --reference-box --cells 4 --pin-start 0 \
    --replicates 3 --duration 10 --data-root "$DATA" $(armflags $arm) \
    --artifacts-root "$ROOT/p1-esec-$spec" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
# P2 — cold replay row (flush-class baseline vs the fua arm, ABBA inside the run, both boots cold).
echo "=== $(date -Is) P2 s39d cold replay flush-class vs fua ===" | tee -a "$LOG"
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s39d --reference-box --cells 4 --pin-start 0 \
  --barrier-class fua --s39d-baseline flush-class --s39d-cold-boot --replicates 3 --leg-idle-s 40 \
  --s39d-warm-records 3000000 --s39d-tail-records 200000 --device-stat nvme0n1 \
  --data-root "$DATA" --artifacts-root "$ROOT/p2-s39d-cold" >> "$LOG" 2>&1
echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
sleep 40
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
