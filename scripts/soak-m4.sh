#!/usr/bin/env bash
# M4 §7 endurance gate (M4-S23): 24 h mixed YCSB A+B blend at 10× RAM with
# compaction, demotion, flush, and blob reclaim active — zero unbounded
# memory growth (RSS slope < 0.5%/24 h), disk bounded within budget, zero
# crashes, WA curve flat after warm-up, tripwires green. Alert thresholds
# are the gate values, checked live, not post-hoc.
#
# HONEST-SUBSET NOTICE (goes stale loudly): until command wiring (M4-S26)
# lifts the ADR-0062 D8 `USE` refusal, the tiered leg cannot exist. This
# script then runs the mechanics-validation form — the A+B blend against a
# durable (non-tiered) namespace with the tiered namespace standing — which
# validates samplers, alerting, and the verdict pipeline. Its verdict is
# stamped MECHANICS-ONLY and does NOT satisfy the §7 memory-honesty gate.
# Once wiring lands, the same invocation runs the real tiered soak.
#
# Usage:  ./scripts/soak-m4.sh [hours] [data-dir] [out-dir]
#   hours takes decimals: 0.5 = the 30 min sampler dry-run the plan requires
#   before committing 24 h of wall time.
# Env knobs:
#   SOAK_MEM_BUDGET_MB (64)    namespace memory budget
#   SOAK_MULTIPLE      (10)    dataset = multiple × budget
#   SOAK_VALUE_BYTES   (1024)  value size
#   SOAK_LEG_SECS      (1800)  one ycsb A+B leg per loop iteration
#   SOAK_CELLS         (4)     server cells
#   SOAK_PIN_START     (4)     server cell pinning start cpu
#   SOAK_LOADGEN_CPUS  (12-23) taskset set for every generator invocation
# Run from infinitydb/ on the reference box after `just check` on a clean
# tree. Full 24 h protocol: .artifacts/m4/s23/RUNBOOK-24h.md.
set -euo pipefail

HOURS="${1:-24}"
DATA_DIR="${2:-$HOME/.cache/inf-m4-soak/data}"
OUT="${3:-.artifacts/m4/s23/soak-$(date +%Y%m%d-%H%M)}"
PORT=7402
CELLS="${SOAK_CELLS:-4}"
PIN_START="${SOAK_PIN_START:-4}"
LOADGEN_CPUS="${SOAK_LOADGEN_CPUS:-12-23}"
MEM_BUDGET_MB="${SOAK_MEM_BUDGET_MB:-64}"
MULTIPLE="${SOAK_MULTIPLE:-10}"
VALUE_BYTES="${SOAK_VALUE_BYTES:-1024}"
LEG_SECS="${SOAK_LEG_SECS:-1800}"
SEED=486541350  # 0x1D0C2026 — the blessed m4 corpus seed
DURATION_S=$(awk -v h="$HOURS" 'BEGIN { printf "%d", h * 3600 }')
DISK_BUDGET_MB=$((MEM_BUDGET_MB * MULTIPLE * 4))
DISK_BUDGET_BYTES=$((DISK_BUDGET_MB * 1024 * 1024))
WA_GATE_MILLI=3000

mkdir -p "$OUT"
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

# The sampler's own output is capped (the sampler-fills-the-disk classic).
MAX_SAMPLES=$((DURATION_S / 10 + 120))
MAX_ALERTS=1000

if df -T "$DATA_DIR" | tail -n 1 | grep -q tmpfs; then
  echo "soak: REFUSING — $DATA_DIR is tmpfs; tier files must exercise a real device" >&2
  exit 1
fi

cargo build --release -p infinityd -p inf-bench

./target/release/inf-bench env-check >"$OUT/env-start.txt" 2>&1 || \
  echo "soak: env-check FAILED at start — run is not citation-grade (see env-start.txt)"
uname -a >>"$OUT/env-start.txt"

./target/release/infinityd --port $PORT --cells "$CELLS" --pin-start "$PIN_START" \
  --data-dir "$DATA_DIR" --segment-bytes $((64 << 20)) \
  --ckpt-interval-bytes $((64 << 20)) >"$OUT/infinityd.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
  [ "$(redis-cli -p $PORT ping 2>/dev/null)" = "PONG" ] && break
  sleep 0.2
done
[ "$(redis-cli -p $PORT ping 2>/dev/null)" = "PONG" ] || {
  echo "soak: server never became ready" >&2; exit 1; }

# Namespaces: the tiered target plus the durable honest-subset fallback.
redis-cli -p $PORT INF.NS CREATE soak MODE durable FSYNC everysec \
  MEM-BUDGET "${MEM_BUDGET_MB}mb" DISK-BUDGET "${DISK_BUDGET_MB}mb"
redis-cli -p $PORT INF.NS CREATE soakdur MODE durable FSYNC everysec

# Mode probe — the D8 refusal is a measured fact, recorded verbatim.
PROBE=$(redis-cli -p $PORT INF.NS USE soak 2>&1 || true)
if grep -q "not command-addressable" <<<"$PROBE"; then
  MODE="MECHANICS-ONLY"
  YCSB_NS_ARGS=(--ns soakdur --named-absent)
  echo "soak: tiered data plane unwired (\`$PROBE\`) — running the honest subset" \
    | tee "$OUT/mode.txt"
else
  MODE="TIERED"
  YCSB_NS_ARGS=(--ns soak)
  echo "soak: tiered data plane is wired — running the real §7 endurance form" \
    | tee "$OUT/mode.txt"
fi

YCSB_COMMON=(--attach-port $PORT --mem-budget-mb "$MEM_BUDGET_MB" \
  --dataset-multiple "$MULTIPLE" --value-size "$VALUE_BYTES" \
  --cells "$CELLS" --seed $SEED --artifacts-root "$OUT/legs" \
  --allow-dirty --unsafe-env "${YCSB_NS_ARGS[@]}")

# Fill once (deterministic loader + DBSIZE integrity assert), then legs
# reuse the dataset with --skip-fill.
taskset -c "$LOADGEN_CPUS" ./target/release/inf-bench ycsb \
  "${YCSB_COMMON[@]}" --fill-only >"$OUT/fill.log" 2>&1

redis-cli -p $PORT INFO tiering >"$OUT/info-start.txt"
redis-cli -p $PORT INFO persistence >>"$OUT/info-start.txt"

# Sampler: 10 s cadence — RSS, disk, WA, refusals, checkpoint gauges, and
# the grouping tripwire, summed across all cells (INFO answers per cell
# under REUSEPORT, so we connect until every cell has been seen).
scrape_cells_awk() {
  local tmp; tmp=$(mktemp)
  local seen=0 tries=0
  : >"$tmp"
  while [ $seen -lt "$CELLS" ] && [ $tries -lt 64 ]; do
    tries=$((tries + 1))
    local info cell
    info=$(redis-cli -p $PORT INFO 2>/dev/null) || continue
    cell=$(grep -oP '^cell:\K\d+' <<<"$info" | head -n 1)
    [ -n "$cell" ] || continue
    if ! grep -q "^CELL $cell\$" "$tmp"; then
      { echo "CELL $cell"; echo "$info"; } >>"$tmp"
      seen=$((seen + 1))
    fi
  done
  cat "$tmp"
  rm -f "$tmp"
}

sample_line() {
  local info rss
  rss=$(awk '/VmRSS/{print $2}' "/proc/$SERVER_PID/status" 2>/dev/null || echo 0)
  info=$(scrape_cells_awk)
  # Sums across cells; write_amp_milli_max takes the worst (max) cell.
  awk -F: -v ts="$(date +%s)" -v rss="$rss" '
    $1 == "tiering_disk_used_bytes"     { disk += $2 }
    $1 == "tiering_wal_bytes"           { wal  += $2 }
    $1 == "tiering_flush_bytes"         { flsh += $2 }
    $1 == "tiering_user_bytes"          { user += $2 }
    $1 == "tiering_write_amp_milli_max" { if ($2 + 0 > wam) wam = $2 + 0 }
    $1 == "tiering_diskfull_refusals"   { ref  += $2 }
    $1 == "ckpts_completed"             { ck   += $2 }
    $1 == "manifests_published"         { mf   += $2 }
    $1 == "raw_submits"                 { subs += $2 }
    $1 == "raw_sqes"                    { sqes += $2 }
    END { printf "%s,%s,%d,%d,%d,%d,%d,%d,%d,%d,%.1f\n",
          ts, rss, disk, wal, flsh, user, wam, ref, ck, mf,
          (subs > 0 ? sqes / subs : 0) }
  ' <<<"$info"
}

alert() {
  if [ "$(wc -l <"$OUT/alerts.log" 2>/dev/null || echo 0)" -lt $MAX_ALERTS ]; then
    echo "$(date +%s) ALERT $*" | tee -a "$OUT/alerts.log" >&2
  fi
}

(
  echo "unix_s,vmrss_kb,disk_used_bytes,wal_bytes,flush_bytes,user_bytes,wa_milli_max,diskfull_refusals,ckpts,manifests,sqes_per_submit" >"$OUT/samples.csv"
  : >"$OUT/alerts.log"
  n=0
  while kill -0 $SERVER_PID 2>/dev/null && [ $n -lt $MAX_SAMPLES ]; do
    line=$(sample_line)
    echo "$line" >>"$OUT/samples.csv"
    # Live gate-value alerts (the thresholds ARE the gate values).
    IFS=, read -r _ _ disk _ _ _ wam ref _ _ _ <<<"$line"
    [ "${wam:-0}" -ge $WA_GATE_MILLI ] && alert "write_amp_milli_max $wam >= $WA_GATE_MILLI"
    [ "${disk:-0}" -gt $DISK_BUDGET_BYTES ] && alert "disk_used $disk > budget $DISK_BUDGET_BYTES"
    [ "${ref:-0}" -gt 0 ] && alert "diskfull_refusals $ref > 0"
    n=$((n + 1))
    sleep 10
  done
) &
SAMPLER_PID=$!

# Load loop: alternating A+B blend legs at the 10× dataset, zipfian.
(
  LEG_FAILS=0
  END=$(( $(date +%s) + DURATION_S ))
  while [ "$(date +%s)" -lt $END ]; do
    left=$(( END - $(date +%s) ))
    leg=$(( left < LEG_SECS ? left : LEG_SECS ))
    [ $leg -lt 30 ] && break
    # One leg = rows a + b + the saturation probe ≈ 2.5× the row
    # duration; leg/3 keeps the leg inside its wall-time slot.
    if taskset -c "$LOADGEN_CPUS" ./target/release/inf-bench ycsb \
        "${YCSB_COMMON[@]}" --skip-fill --workloads a,b \
        --distribution zipfian --duration $((leg / 3 > 5 ? leg / 3 : 5)) \
        >>"$OUT/loadgen.log" 2>&1; then
      LEG_FAILS=0
    else
      LEG_FAILS=$((LEG_FAILS + 1))
      echo "$(date +%s) ALERT ycsb leg failed ($LEG_FAILS consecutive)" >>"$OUT/alerts.log"
      [ $LEG_FAILS -ge 3 ] && { echo "3 consecutive leg failures" >>"$OUT/alerts.log"; break; }
      sleep 5
    fi
  done
) &
LOAD_PID=$!
trap 'kill $LOAD_PID $SAMPLER_PID $SERVER_PID 2>/dev/null || true' EXIT

echo "soak[$MODE]: running ${HOURS} h against pid $SERVER_PID (out: $OUT)"
wait $LOAD_PID || true
kill $SAMPLER_PID 2>/dev/null || true
sleep 1

redis-cli -p $PORT INFO tiering >"$OUT/info-end.txt" 2>/dev/null || true
redis-cli -p $PORT INFO persistence >>"$OUT/info-end.txt" 2>/dev/null || true
./target/release/inf-bench env-check >"$OUT/env-end.txt" 2>&1 || true

SERVER_ALIVE=0
kill -0 $SERVER_PID 2>/dev/null && SERVER_ALIVE=1

python3 - "$OUT/samples.csv" "$HOURS" "$SERVER_ALIVE" "$DISK_BUDGET_BYTES" "$MODE" <<'EOF' | tee "$OUT/verdict.txt"
import csv, sys
rows = [r for r in csv.DictReader(open(sys.argv[1])) if int(r["vmrss_kb"]) > 0]
hours, alive, budget, mode = float(sys.argv[2]), sys.argv[3] == "1", int(sys.argv[4]), sys.argv[5]
fails = []
if not alive:
    fails.append("server died during the soak (see infinityd.log)")
if len(rows) < 6:
    fails.append(f"only {len(rows)} samples — sampler failure invalidates the run")
else:
    # RSS slope: first/last-5% medians, storm-resistant, scaled to /24 h.
    k = max(1, len(rows) // 20)
    med = lambda xs: sorted(xs)[len(xs) // 2]
    first = med([int(r["vmrss_kb"]) for r in rows[:k]])
    last = med([int(r["vmrss_kb"]) for r in rows[-k:]])
    slope = (last - first) / first * 100 / (hours / 24)
    print(f"rss slope {slope:+.3f}%/24h (first-5% median {first} kB -> last-5% median {last} kB, {len(rows)} samples)")
    if slope >= 0.5:
        fails.append(f"RSS slope {slope:+.3f}%/24h >= 0.5%/24h")
    disk_max = max(int(r["disk_used_bytes"]) for r in rows)
    print(f"disk max {disk_max} bytes (budget {budget})")
    if disk_max > budget:
        fails.append(f"disk {disk_max} exceeded budget {budget}")
    wa_max = max(int(r["wa_milli_max"]) for r in rows)
    print(f"write-amp milli max {wa_max} (gate < 3000)")
    if wa_max >= 3000:
        fails.append(f"write amplification {wa_max / 1000:.3f}x breached the 3x gate")
    # WA flatness after warm-up: second-quarter mean vs last-quarter mean.
    q = len(rows) // 4
    if q > 0:
        mean = lambda xs: sum(xs) / len(xs)
        warm = mean([int(r["wa_milli_max"]) for r in rows[q:2 * q]])
        tail = mean([int(r["wa_milli_max"]) for r in rows[-q:]])
        if warm > 0 and tail > warm * 1.05:
            fails.append(f"WA curve not flat: {warm:.0f} -> {tail:.0f} milli (> +5%)")
        print(f"wa flatness: q2 mean {warm:.0f} -> q4 mean {tail:.0f} milli")
verdict = "FAIL" if fails else "PASS"
stamp = " (MECHANICS-ONLY: tiered leg named-absent — does NOT satisfy the §7 gate; M4-S26)" if mode != "TIERED" else ""
print(f"{verdict}{stamp}")
for f in fails:
    print(f"  - {f}")
EOF

ALERTS=$(wc -l <"$OUT/alerts.log" 2>/dev/null || echo 0)
echo "soak: done — $ALERTS alert lines; artifacts in $OUT"
