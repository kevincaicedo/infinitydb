#!/usr/bin/env bash
# v0.4.0 unified stability gate (ADR-0066 D1): ONE 24 h soak over a single
# node carrying the cache/KV, document, and tiered planes at once, replacing
# three separate per-milestone soak nights.
#
# Why one run is stronger, not just cheaper: the three planes share a node's
# allocator, checkpoint cadence, and RSS. Three isolated soaks each prove a
# plane in a quiet house and prove nothing about the profile the release
# actually ships. This is the same multi-namespace node the M4-S20 mixed
# audit drives, held for 24 h.
#
# The per-milestone harnesses REMAIN and are named here so no substitution
# is silent (ADR-0025 D3):
#   scripts/soak-m2.sh  KV / durability     — legs `kv_mem`, `kv_esec`, `kv_alw`
#   scripts/soak-m3.sh  documents           — legs `doc_ingest`, `doc_read`, `doc_mut`, `doc_esec`
#   scripts/soak-m4.sh  tiered endurance    — leg `tier` (see the notice below)
#
# HONEST-SUBSET NOTICE (goes stale loudly, same contract as soak-m4.sh):
# until command wiring (M4-S26) lifts the ADR-0062 D8 `USE` refusal, no data
# command can reach a tiered namespace. This script probes that at startup.
# If the plane is unwired the tiered namespace still STANDS (VA reservation,
# budgets, index structures — the memory terms the unified RSS profile must
# account for) but serves no traffic, and the verdict is stamped
# TIER-MECHANICS-ONLY: it discharges the M2.5 stability gate and the M3 §7
# document-soak gate, and explicitly does NOT discharge the M4 §7
# memory-honesty gate. Once S26 lands, the same invocation drives the tiered
# leg for real and the stamp changes to FULL.
#
# Usage:  ./scripts/soak-unified.sh [hours] [data-dir] [out-dir]
#   hours takes decimals: 0.5 = a 30 min mechanics dry-run. The plan forbids
#   spending 24 h of wall time on an unproven pipeline — do a short run first.
# Env knobs:
#   SOAK_CELLS         (4)      server cells
#   SOAK_PIN_START     (4)      server cell pinning start cpu
#   SOAK_LOADGEN_CPUS  (12-23)  taskset set for every generator
#   SOAK_MEM_BUDGET_MB (64)     tiered namespace memory budget
#   SOAK_MULTIPLE      (10)     tiered dataset = multiple x budget
#   SOAK_LEG_SECS      (1800)   one tiered ycsb leg per loop iteration
#   SOAK_TIER_WORKLOADS (a,b)   YCSB rows the tiered leg drives. The default
#                               is the endurance blend the gate is written
#                               against and the one attempt 2 ran; it is
#                               already write-heavy enough to drive
#                               compaction hard (measured 2026-08-08: 6.5 M
#                               compact slices, 22.4 GB compacted, 54.9 GB
#                               dead over 24 h). Add `f` (read-modify-write)
#                               for more write pressure — but a changed mix
#                               is a changed workload, so say so in the
#                               run's notes rather than comparing across it.
#   SOAK_WARMUP_HOURS  (8)      ADR-0069 D1: declared warm-up excluded from
#                               the slope gates. Prospective — declared here,
#                               before the run, never fitted afterwards. The
#                               citation-grade form is `soak-unified.sh 32`
#                               (8 h warm-up + 24 h steady window); runs
#                               shorter than 2x the warm-up gate on the
#                               whole run and the verdict says so.
#   SOAK_WARMUP_DEAD_CHECK   (1)  ADR-0071 D5: warm-up-end reclaim watchdog.
#                                 At warm-up end, node dead_bytes below the
#                                 bar means the run can only repeat the
#                                 20260811 zero-compaction FAIL — abort at
#                                 hour ~8 instead of hour 30. 0 disables.
#   SOAK_WARMUP_DEAD_MIN_PCT (5)  the bar, as % of the tiered dataset
#                                 (budget x multiple). Prospective, declared
#                                 before the run; the healthy post-F22 shape
#                                 is ~50% of dataset by hour 8, the broken
#                                 shape <0.5% — 5% separates them with an
#                                 order of magnitude on each side.
# Run from infinitydb/ on the reference box after `just check` on a clean
# tree, with the box otherwise idle (§6: a soak and a campaign leg are
# mutually exclusive).
set -euo pipefail

HOURS="${1:-24}"
DATA_DIR="${2:-$HOME/.cache/inf-unified-soak/data}"
OUT="${3:-.artifacts/v0.4.0/soak-unified-$(date +%Y%m%d-%H%M)}"
PORT=7405
CELLS="${SOAK_CELLS:-4}"
PIN_START="${SOAK_PIN_START:-4}"
LOADGEN_CPUS="${SOAK_LOADGEN_CPUS:-12-23}"
MEM_BUDGET_MB="${SOAK_MEM_BUDGET_MB:-64}"
MULTIPLE="${SOAK_MULTIPLE:-10}"
LEG_SECS="${SOAK_LEG_SECS:-1800}"
TIER_WORKLOADS="${SOAK_TIER_WORKLOADS:-a,b}"
WARMUP_HOURS="${SOAK_WARMUP_HOURS:-8}"
# Mirrors TierSpec::compaction_dead_ratio_pct (inf-store/src/ns.rs,
# default 50, clamped 50..=100 by ADR-0059 D1). The verdict needs it to
# say whether a zero-compaction run was ever *eligible* to compact.
# Override only if the soak namespace is created with a non-default
# COMPACTION-DEAD-RATIO, or the verdict will reason against the wrong bar.
DEAD_RATIO_PCT="${SOAK_DEAD_RATIO_PCT:-50}"
SEED=0x1D0C2026          # the blessed corpus seed (ADR-0046 D3)
SEED_DEC=486541350
DURATION_S=$(awk -v h="$HOURS" 'BEGIN { printf "%d", h * 3600 }')
DISK_BUDGET_MB=$((MEM_BUDGET_MB * MULTIPLE * 4))
DISK_BUDGET_BYTES=$((DISK_BUDGET_MB * 1024 * 1024))
WA_GATE_MILLI=3000
WORK="${WORK:-$(mktemp -d)}"

rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"

# A RAM "device byte" is not a device byte, and a tmpfs RSS slope is a lie.
if df -T "$DATA_DIR" | tail -n 1 | grep -q tmpfs; then
  echo "soak-unified: REFUSING — $DATA_DIR is tmpfs; the durable and tiered planes need a real device" >&2
  exit 1
fi

# ENOSPC at hour 20 costs a box-night. The tier namespace is entitled to its
# full DISK-BUDGET, and the durable planes hold segments + checkpoints beside
# it — require the declared budget plus margin up front, not mid-window.
FREE_MB=$(df -Pm "$DATA_DIR" | awk 'NR == 2 { print $4 }')
NEED_MB=$((DISK_BUDGET_MB + DISK_BUDGET_MB / 4 + 10240))
if [ "$FREE_MB" -lt "$NEED_MB" ]; then
  echo "soak-unified: REFUSING — $FREE_MB MB free under $DATA_DIR, need $NEED_MB MB (DISK-BUDGET $DISK_BUDGET_MB MB + 25% + 10 GB durable-plane margin)" >&2
  exit 1
fi

# /tmp carries usrquota on this box since the 2026-08 OS update (EDQUOT
# fail-stops observed on tmpfs-default data dirs). $WORK holds the generated
# .resp pipes — probe the quota now, at minute 1, not when doc-corpus dies.
if ! dd if=/dev/zero of="$WORK/.quota-probe" bs=1M count=64 status=none 2>/dev/null; then
  echo "soak-unified: REFUSING — cannot write 64 MB to WORK=$WORK (quota/space); set WORK to a writable dir" >&2
  rm -f "$WORK/.quota-probe"
  exit 1
fi
rm -f "$WORK/.quota-probe"

echo "soak-unified: NOTE — quiet-box rule (§6): no campaign legs beside a soak; drive state drifts tail results — \`sudo fstrim -v /\` before a box-night is the standing recipe (20260809)."

MAX_SAMPLES=$((DURATION_S / 10 + 120))
MAX_ALERTS=1000

cargo build --release -p infinityd -p inf-bench

# env-check runs BEFORE `$OUT` exists, into a temp file. `.artifacts/` is
# tracked, so creating the output directory first makes the tree dirty and
# the probe fails on this run's *own* artifacts — a self-inflicted FAIL
# banner that says nothing about binary provenance. Same reason the
# end-of-run probe is annotated rather than trusted bare.
ENV_TMP=$(mktemp)
./target/release/inf-bench env-check >"$ENV_TMP" 2>&1 || \
  echo "soak-unified: env-check FAILED at start — run is not citation-grade (see env-start.txt)"
uname -a >>"$ENV_TMP"
TREE_TMP=$(mktemp)
git -C . rev-parse HEAD >"$TREE_TMP" 2>/dev/null || true
git -C . status --porcelain >>"$TREE_TMP" 2>/dev/null || true

mkdir -p "$OUT"
mv "$ENV_TMP" "$OUT/env-start.txt"
mv "$TREE_TMP" "$OUT/tree.txt"

# ---- document workload pipes (generated once, replayed) ---------------------
# Same reduced corpus-v2 mix soak-m3.sh uses, so the document leg is the M3
# harness's workload and not a new one.
./target/release/inf-bench doc-corpus --seed $SEED --pipe "$WORK/ingest.resp" \
    --counts "small-200B=2000,gate-1KiB=2000,medium-2KiB=1000,large-64KiB=100,deep-32=2000,wide-array=20" \
    > "$OUT/ingest-manifest.txt"
python3 - "$WORK" <<'EOF'
import sys
work = sys.argv[1]
def frame(args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out
with open(f"{work}/reads.resp", "wb") as f:
    for i in range(2000):
        f.write(frame([b"JSON.GET", b"gate-1KiB:%d" % i, b"$.child.child.child.child.id"]))
    for i in range(20):
        for k in (0, 999, 9999):
            f.write(frame([b"JSON.GET", b"wide-array:%d" % i, b"$[%d].qty" % k]))
with open(f"{work}/mutations.resp", "wb") as f:
    for i in range(2000):
        f.write(frame([b"JSON.NUMINCRBY", b"gate-1KiB:%d" % i, b"$.score", b"1"]))
    for i in range(200):
        f.write(frame([b"JSON.SET", b"small-200B:%d" % i, b"$.name", b'"soak"']))
EOF

# ---- server ----------------------------------------------------------------
# ADR-0069 D3: prefer a dedicated cgroup scope so the scope's memory.stat
# isolates the server's file cache from the loadgens (which re-read
# $WORK/*.resp every loop and polluted the shared terminal scope with
# ~1 GB of file cache in the 20260807 analysis). Falls back to a plain
# child if systemd-run is unavailable; either way the attribution sampler
# resolves the actual cgroup from /proc/<pid>/cgroup and the mode is
# recorded here.
SERVER_ARGS=(--port $PORT --cells "$CELLS" --pin-start "$PIN_START"
  --data-dir "$DATA_DIR" --segment-bytes $((64 << 20))
  --ckpt-interval-bytes $((64 << 20)))
LAUNCH_MODE=plain
if command -v systemd-run >/dev/null 2>&1; then
  systemd-run --user --scope --unit="inf-soak-$(date +%s)" --quiet -- \
    ./target/release/infinityd "${SERVER_ARGS[@]}" >"$OUT/infinityd.log" 2>&1 &
  SERVER_PID=""
  for _ in $(seq 1 50); do
    SERVER_PID=$(pgrep -nx infinityd 2>/dev/null || true)
    [ -n "$SERVER_PID" ] && { LAUNCH_MODE=scoped; break; }
    sleep 0.2
  done
fi
if [ -z "${SERVER_PID:-}" ]; then
  ./target/release/infinityd "${SERVER_ARGS[@]}" >"$OUT/infinityd.log" 2>&1 &
  SERVER_PID=$!
fi
echo "soak-unified: server launch mode $LAUNCH_MODE (pid $SERVER_PID)" | tee "$OUT/launch-mode.txt"
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
  [ "$(redis-cli -p $PORT ping 2>/dev/null)" = "PONG" ] && break
  sleep 0.2
done
[ "$(redis-cli -p $PORT ping 2>/dev/null)" = "PONG" ] || {
  echo "soak-unified: server never became ready" >&2; exit 1; }

# ---- the unified profile: every plane on one node --------------------------
# KV (M2.5/M2): memory default DB + durable everysec + durable always.
redis-cli -p $PORT INF.NS CREATE kv_esec MODE durable FSYNC everysec
redis-cli -p $PORT INF.NS CREATE kv_alw  MODE durable FSYNC always
# Documents (M3): durable-everysec doc namespace beside the memory default.
redis-cli -p $PORT INF.NS CREATE doc_esec MODE durable FSYNC everysec
# Tiered (M4): budgeted namespace — stands whether or not it can serve.
redis-cli -p $PORT INF.NS CREATE tier MODE durable FSYNC everysec \
  MEM-BUDGET "${MEM_BUDGET_MB}mb" DISK-BUDGET "${DISK_BUDGET_MB}mb"

# Mode probe — the D8 refusal is a measured fact, recorded verbatim.
PROBE=$(redis-cli -p $PORT INF.NS USE tier 2>&1 || true)
if grep -q "not command-addressable" <<<"$PROBE"; then
  MODE="TIER-MECHANICS-ONLY"
  echo "soak-unified: tiered data plane unwired (\`$PROBE\`) — tiered namespace stands but serves no traffic (M4-S26)" \
    | tee "$OUT/mode.txt"
else
  MODE="FULL"
  echo "soak-unified: tiered data plane is wired — running the full three-plane form" \
    | tee "$OUT/mode.txt"
fi

redis-cli -p $PORT INFO persistence >"$OUT/info-start.txt"
redis-cli -p $PORT INFO memory     >>"$OUT/info-start.txt"
redis-cli -p $PORT INFO tiering    >>"$OUT/info-start.txt" 2>/dev/null || true

# ---- sampler: 10 s cadence, all planes, cell-summed -------------------------
scrape_cells() {
  local tmp; tmp=$(mktemp); local seen=0 tries=0
  : >"$tmp"
  while [ $seen -lt "$CELLS" ] && [ $tries -lt 64 ]; do
    tries=$((tries + 1))
    local info cell
    info=$(redis-cli -p $PORT INFO 2>/dev/null) || continue
    cell=$(grep -oP '^cell:\K\d+' <<<"$info" | head -n 1)
    [ -n "$cell" ] || continue
    if ! grep -q "^CELL $cell\$" "$tmp"; then
      { echo "CELL $cell"; echo "$info"; } >>"$tmp"; seen=$((seen + 1))
    fi
  done
  cat "$tmp"; rm -f "$tmp"
}

sample_line() {
  local info rss
  rss=$(awk '/VmRSS/{print $2}' "/proc/$SERVER_PID/status" 2>/dev/null || echo 0)
  info=$(scrape_cells)
  # `INFO memory` fields are a NODE-WIDE fold republished by every cell
  # (the M3-S25 MemoryBoard fix) — take the max, never the sum, or a
  # 4-cell node reports 4x the documents it holds. `INFO persistence` and
  # `INFO tiering` counters are genuinely per-cell and are summed.
  awk -F: -v ts="$(date +%s)" -v rss="$rss" '
    $1 == "docs_live"                   { if ($2 + 0 > docs) docs = $2 + 0 }
    $1 == "doc_resident_bytes"          { if ($2 + 0 > dres) dres = $2 + 0 }
    $1 == "used_memory"                 { if ($2 + 0 > um)   um   = $2 + 0 }
    $1 == "tiering_disk_used_bytes"     { disk += $2 }
    $1 == "tiering_write_amp_milli_max" { if ($2 + 0 > wam) wam = $2 + 0 }
    $1 == "tiering_diskfull_refusals"   { ref  += $2 }
    $1 == "ckpts_completed"             { ck   += $2 }
    $1 == "manifests_published"         { mf   += $2 }
    $1 == "log_segments_live"           { segs += $2 }
    $1 == "raw_submits"                 { subs += $2 }
    $1 == "raw_sqes"                    { sqes += $2 }
    $1 == "tiering_committed_bytes"     { tcom += $2 }
    $1 == "tiering_index_bytes"         { tidx += $2 }
    $1 == "log_staging_bytes"           { stag += $2 }
    $1 == "ckpt_buffer_bytes"           { ckb  += $2 }
    $1 == "cold_reads_issued"           { cread += $2 }
    $1 == "tiering_flush_slices"        { fsl  += $2 }
    $1 == "tiering_compact_slices"      { csl  += $2 }
    $1 == "tiering_dead_bytes"          { dead += $2 }
    $1 == "tiering_live_bytes"          { live += $2 }
    $1 == "tiering_compact_idle_pressure" { cidle += $2 }
    END { printf "%s,%s,%d,%d,%d,%d,%d,%d,%d,%d,%.1f,%d,%d,%d,%d,%d,%d,%d,%d,%d,%d,%d\n",
          ts, rss, docs, dres, disk, wam, ref, ck, mf, segs,
          (subs > 0 ? sqes / subs : 0), int(um / 1024),
          tcom, tidx, stag, ckb, cread, fsl, csl, dead, live, cidle }
  ' <<<"$info"
}

alert() {
  if [ "$(wc -l <"$OUT/alerts.log" 2>/dev/null || echo 0)" -lt $MAX_ALERTS ]; then
    echo "$(date +%s) ALERT $*" | tee -a "$OUT/alerts.log" >&2
  fi
}

(
  # used_memory_kb (accounted, node-wide fold — max across cells like
  # docs_live) trails so no positional consumer shifts. It exists so a
  # failing RSS slope decomposes into accounted vs unaccounted growth
  # (the 20260805 FAIL could not be attributed without it — readiness F9).
  # Trailing columns after used_memory_kb (ADR-0069 D3): node-summed
  # cell-scope memory terms — the 20260807 analysis showed the Tiering/
  # Persistence INFO sections are per-cell, so a single-cell scrape
  # under-reports them 4x and the RSS "gap" was exactly that.
  # Trailing again (ADR-0071 D4): the tiered plane's own liveness series.
  # The 20260808 run passed every memory gate while its tiered leg had been
  # dead for 31.4 of 32.1 h — invisible, because no sampled column could
  # have shown it. cold_reads_issued / flush_slices / compact_slices are
  # monotonic per-cell counters; the verdict requires them to ADVANCE
  # across the measured window, which is what "the tier was under load"
  # actually means.
  # Trailing once more (readiness F22, 2026-08-12): the *reclaim* side.
  # The 20260811 run kept its tier demonstrably alive (99.9 M cold reads)
  # and still never compacted, and the bundle could not say whether that
  # meant "no dead space was created" (workload) or "dead space was
  # ignored" (engine) — because no sampled column carried dead space.
  # `INFO tiering` already emits all three per cell; not sampling them was
  # the gap. dead_bytes is the reclaimable backlog and live_bytes is its
  # denominator — compaction triggers on the RATIO (dead / (dead + live)
  # >= COMPACTION-DEAD-RATIO, default 50%), so dead alone cannot say
  # whether compaction *should* have run. The 20260812 shake proved that
  # the hard way: 641 MB of dead read as alarming in isolation and was in
  # fact 2.8% of live, far below any trigger. compact_idle_pressure
  # counts the times compaction was pressured and found nothing, which
  # separates "never asked" from "asked, nothing there". A gate that can
  # detect a frozen tier but not diagnose one is half an instrument.
  echo "unix_s,vmrss_kb,docs_live,doc_resident_bytes,disk_used_bytes,wa_milli_max,diskfull_refusals,ckpts,manifests,segs_live,sqes_per_submit,used_memory_kb,tier_committed_bytes,tier_index_bytes,log_staging_bytes,ckpt_buffer_bytes,cold_reads_issued,flush_slices,compact_slices,dead_bytes,live_bytes,compact_idle_pressure" >"$OUT/samples.csv"
  : >"$OUT/alerts.log"
  n=0
  while kill -0 $SERVER_PID 2>/dev/null && [ $n -lt $MAX_SAMPLES ]; do
    line=$(sample_line)
    echo "$line" >>"$OUT/samples.csv"
    IFS=, read -r _ _ _ _ disk wam ref _ _ _ _ <<<"$line"
    [ "${wam:-0}" -ge $WA_GATE_MILLI ] && alert "write_amp_milli_max $wam >= $WA_GATE_MILLI"
    [ "${disk:-0}" -gt $DISK_BUDGET_BYTES ] && alert "disk_used $disk > budget $DISK_BUDGET_BYTES"
    [ "${ref:-0}" -gt 0 ] && alert "diskfull_refusals $ref > 0"
    n=$((n + 1))
    sleep 10
  done
) &
SAMPLER_PID=$!

# ---- attribution sampler (ADR-0069 D3) --------------------------------------
# smaps_rollup at start/hourly/end so RSS decomposes into anon/file/shmem,
# plus the server's cgroup memory.stat for the file-cache disclosure the
# M4 §7 gate text names. Caveat recorded in-band: infinityd runs inside the
# launching user session's cgroup scope, so memory.stat is the slice, not
# the process — smaps_rollup is the per-process truth, memory.stat bounds
# the file-cache term.
CG_PATH=$(awk -F: 'NR==1{print $3}' "/proc/$SERVER_PID/cgroup" 2>/dev/null || true)
attr_snap() {
  {
    echo "=== $(date +%s) $1 ==="
    cat "/proc/$SERVER_PID/smaps_rollup" 2>/dev/null || echo "smaps_rollup unavailable"
    if [ -n "$CG_PATH" ] && [ -r "/sys/fs/cgroup$CG_PATH/memory.stat" ]; then
      echo "--- cgroup $CG_PATH memory.stat (session-slice scope, see header) ---"
      cat "/sys/fs/cgroup$CG_PATH/memory.stat"
    else
      echo "--- cgroup memory.stat unavailable (path: ${CG_PATH:-none}) ---"
    fi
  } >>"$OUT/attribution.log"
}
attr_snap start
(
  while kill -0 $SERVER_PID 2>/dev/null; do
    sleep 3600
    kill -0 $SERVER_PID 2>/dev/null && attr_snap hourly
  done
) &
ATTR_PID=$!

# ---- legs -------------------------------------------------------------------
LEGS=()

# KV legs (soak-m2.sh shapes): memory 1:10, everysec 1:1, always SET-only.
kv_leg() { # name, ns (or ""), mix, conns, pipeline
  local name=$1 ns=$2 mix=$3 conns=$4 pipe=$5
  while true; do
    taskset -c "$LOADGEN_CPUS" ./target/release/inf-bench load --port $PORT \
      --conns "$conns" --pipeline "$pipe" --duration 300 --mix "$mix" \
      --keys 200000 --value-size 512 ${ns:+--setup "INF.NS USE $ns"} \
      >>"$OUT/loadgen-$name.log" 2>&1 || true
  done
}
kv_leg kv_mem  ""       1:10 16 8 & LEGS+=($!)
kv_leg kv_esec kv_esec  1:1  16 8 & LEGS+=($!)
kv_leg kv_alw  kv_alw   1:0   8 8 & LEGS+=($!)

# Document legs (soak-m3.sh shapes): ingest / path reads / scalar mutations
# on the memory default DB, plus a durable-everysec ingest.
doc_leg() { # name, pipe, ns (or "")
  local name=$1 pipe=$2 ns=$3
  while true; do
    if [ -n "$ns" ]; then
      { printf '*3\r\n$6\r\nINF.NS\r\n$3\r\nUSE\r\n$%d\r\n%s\r\n' "${#ns}" "$ns"; cat "$pipe"; } \
        | redis-cli -p $PORT --pipe >>"$OUT/loadgen-$name.log" 2>&1 || true
    else
      redis-cli -p $PORT --pipe < "$pipe" >>"$OUT/loadgen-$name.log" 2>&1 || true
    fi
    sleep 0.2
  done
}
doc_leg doc_ingest "$WORK/ingest.resp"    ""       & LEGS+=($!)
doc_leg doc_read   "$WORK/reads.resp"     ""       & LEGS+=($!)
doc_leg doc_mut    "$WORK/mutations.resp" ""       & LEGS+=($!)
doc_leg doc_esec   "$WORK/ingest.resp"    doc_esec & LEGS+=($!)

# Tiered leg (soak-m4.sh shape): only when the plane can actually serve.
if [ "$MODE" = "FULL" ]; then
  YCSB_COMMON=(--attach-port $PORT --mem-budget-mb "$MEM_BUDGET_MB" \
    --dataset-multiple "$MULTIPLE" --value-size 1024 --cells "$CELLS" \
    --artifacts-root "$OUT/legs" --allow-dirty --unsafe-env --ns tier)
  # The FILL keeps the blessed corpus seed: the dataset must be identical
  # across runs for any cross-run comparison to mean anything. Key
  # identity is `load::make_key(index)` — prefix + zero-padded index, no
  # seed in it — so the seed selects which keys a leg *draws*, never
  # which keys *exist*.
  taskset -c "$LOADGEN_CPUS" ./target/release/inf-bench ycsb \
    "${YCSB_COMMON[@]}" --seed $SEED_DEC --fill-only >"$OUT/fill.log" 2>&1
  (
    END=$(( $(date +%s) + DURATION_S )); FAILS=0; LEG_INDEX=0
    while [ "$(date +%s)" -lt $END ]; do
      left=$(( END - $(date +%s) ))
      leg=$(( left < LEG_SECS ? left : LEG_SECS ))
      [ $leg -lt 30 ] && break
      # ADR-0064 D7 amended 2026-08-12 (owner) — readiness F22. Every leg
      # used to be handed the SAME seed, so the 145 legs of the 20260811
      # run replayed one identical scrambled-zipfian key stream: a 32 h
      # endurance soak was one leg repeated 145 times, which is not the
      # workload the gate imagines. Fixed on its own merits.
      # HONESTY NOTE — this is NOT established as the cause of that run's
      # zero compaction: the 20260807 run (attempt 2) carried the same
      # fixed seed and still wrote 125.8 GB of user bytes, accumulated
      # 54.9 GB dead and drove 6.5 M compaction slices. Whatever collapsed
      # attempt 4's tiered *writes* (reads were unchanged: 99.9 M vs
      # 102.3 M cold reads) is still open — see readiness F22. The stream
      # now advances per leg while staying fully reproducible: leg i is a
      # pure function of the run's master seed and i (golden-ratio step,
      # so adjacent legs are not adjacent streams), echoed into the log.
      LEG_SEED=$(( SEED_DEC + LEG_INDEX * 2654435761 ))
      echo "== soak tier leg $LEG_INDEX: seed $LEG_SEED (master $SEED_DEC + $LEG_INDEX x 2654435761) ==" \
        >>"$OUT/loadgen-tier.log"
      LEG_INDEX=$((LEG_INDEX + 1))
      if taskset -c "$LOADGEN_CPUS" ./target/release/inf-bench ycsb \
          "${YCSB_COMMON[@]}" --seed $LEG_SEED \
          --skip-fill --workloads "$TIER_WORKLOADS" --distribution zipfian \
          --duration $((leg / 3 > 5 ? leg / 3 : 5)) >>"$OUT/loadgen-tier.log" 2>&1; then
        FAILS=0
      else
        FAILS=$((FAILS + 1))
        echo "$(date +%s) ALERT ycsb tiered leg failed ($FAILS consecutive)" >>"$OUT/alerts.log"
        # A dead tiered leg is not a footnote: it is the difference
        # between a run that measured the tiered plane and one that did
        # not. Leaving a sentinel is what lets the verdict downgrade its
        # own stamp instead of discharging a gate it never exercised
        # (ADR-0071 D4; readiness F17).
        if [ $FAILS -ge 3 ]; then
          echo "3 consecutive tiered leg failures" >>"$OUT/alerts.log"
          {
            echo "broken_at_unix=$(date +%s)"
            echo "reason=3 consecutive \`inf-bench ycsb\` failures — see loadgen-tier.log"
            echo "consequence=the tiered plane served no traffic after this point; this run"
            echo "consequence=cannot discharge the M4 §7 memory-honesty gate"
          } >"$OUT/tier-leg-broken.txt"
          break
        fi
        sleep 5
      fi
    done
  ) & LEGS+=($!)
else
  echo "tiered leg: NOT RUN — plane unwired (M4-S26). The namespace stands; its VA reservation, budgets, and index structures are in the RSS profile." >"$OUT/tier-leg.txt"
fi

# ---- warm-up-end reclaim watchdog (ADR-0071 D5; readiness F22) --------------
# Attempt 4 spent 30.35 h to learn its tier had stopped accumulating dead
# space within the first hours. That shape is decidable at warm-up end: at
# the measured post-F22 rate the reclaim backlog is on pace for the
# compaction trigger by hour 8 (~half the dataset), while the broken shape
# sits at tens of MB — orders apart, so a 5%-of-dataset bar separates them
# with margin on both sides. Below the bar, the remaining 24 h cannot reach
# the trigger and the run can only repeat the 20260811 FAIL — abort now.
# The watchdog kills the LEGS and leaves the server up, so the end-of-run
# scrapes and the verdict still run and stamp the FAIL with the ratio
# discriminator; early-abort.txt carries the reason into the bundle.
WARMUP_DEAD_CHECK="${SOAK_WARMUP_DEAD_CHECK:-1}"
WARMUP_DEAD_MIN_PCT="${SOAK_WARMUP_DEAD_MIN_PCT:-5}"
WATCH_PID=""
if [ "$MODE" = "FULL" ] && [ "$WARMUP_DEAD_CHECK" = "1" ] \
   && awk -v h="$HOURS" -v w="$WARMUP_HOURS" 'BEGIN { exit !(h > 2 * w) }'; then
  (
    sleep "$(awk -v w="$WARMUP_HOURS" 'BEGIN { printf "%d", w * 3600 + 600 }')"
    kill -0 $SERVER_PID 2>/dev/null || exit 0
    BAR=$((MEM_BUDGET_MB * MULTIPLE * 1024 * 1024 / 100 * WARMUP_DEAD_MIN_PCT))
    # dead_bytes is column 20 of samples.csv (node-summed by the sampler);
    # take the last complete row's value.
    DEAD=$(awk -F, 'NF >= 20 && $20 ~ /^[0-9]+$/ { d = $20 } END { print d + 0 }' "$OUT/samples.csv")
    if [ "$DEAD" -lt "$BAR" ]; then
      {
        echo "aborted_at_unix=$(date +%s)"
        echo "reason=warm-up-end reclaim check: node dead_bytes $DEAD < bar $BAR (${WARMUP_DEAD_MIN_PCT}% of the $((MEM_BUDGET_MB * MULTIPLE)) MB tiered dataset)"
        echo "consequence=dead-space accumulation matches the 20260811 zero-compaction shape; the remaining hours cannot reach the compaction trigger, so the run was aborted at warm-up end instead of hour 30"
      } >"$OUT/early-abort.txt"
      echo "$(date +%s) ALERT warm-up-end reclaim check: dead_bytes $DEAD < bar $BAR — early abort" >>"$OUT/alerts.log"
      kill ${LEGS[*]} 2>/dev/null || true
    else
      echo "warm-up-end reclaim check: ON-TRACK — dead_bytes $DEAD >= bar $BAR (${WARMUP_DEAD_MIN_PCT}% of the tiered dataset)" \
        | tee "$OUT/warmup-check.txt"
    fi
  ) & WATCH_PID=$!
fi

trap 'kill ${LEGS[*]} $SAMPLER_PID $ATTR_PID $WATCH_PID $SERVER_PID 2>/dev/null || true' EXIT

echo "soak-unified[$MODE]: running ${HOURS} h against pid $SERVER_PID (out: $OUT)"
END_TS=$(($(date +%s) + DURATION_S))
while [ "$(date +%s)" -lt "$END_TS" ] && [ ! -f "$OUT/early-abort.txt" ]; do
  REMAIN=$((END_TS - $(date +%s)))
  sleep $((REMAIN < 60 ? REMAIN : 60))
done
if [ -f "$OUT/early-abort.txt" ]; then
  echo "soak-unified: EARLY ABORT at warm-up end —"
  cat "$OUT/early-abort.txt"
fi
kill ${LEGS[*]} 2>/dev/null || true
sleep 2

redis-cli -p $PORT INFO persistence >"$OUT/info-end.txt" 2>/dev/null || true
redis-cli -p $PORT INFO memory     >>"$OUT/info-end.txt" 2>/dev/null || true
redis-cli -p $PORT INFO tiering    >>"$OUT/info-end.txt" 2>/dev/null || true
redis-cli -p $PORT INFO tripwires  >>"$OUT/info-end.txt" 2>/dev/null || true
{
  echo "# NOTE: this run's own artifacts under $OUT are untracked by now,"
  echo "# so git-dirty-tree is EXPECTED to fail here. env-start.txt is the"
  echo "# provenance probe; this one only re-checks governor/EPP/thermal."
} >"$OUT/env-end.txt"
./target/release/inf-bench env-check >>"$OUT/env-end.txt" 2>&1 || true
attr_snap end

SERVER_ALIVE=0
kill -0 $SERVER_PID 2>/dev/null && SERVER_ALIVE=1

ALERT_LINES=$(wc -l <"$OUT/alerts.log" 2>/dev/null || echo 0)
TIER_BROKEN=0
[ -f "$OUT/tier-leg-broken.txt" ] && TIER_BROKEN=1

python3 - "$OUT/samples.csv" "$HOURS" "$SERVER_ALIVE" "$DISK_BUDGET_BYTES" "$MODE" "$WARMUP_HOURS" "$ALERT_LINES" "$TIER_BROKEN" "$DEAD_RATIO_PCT" <<'EOF' | tee "$OUT/verdict.txt"
import csv, sys
rows = [r for r in csv.DictReader(open(sys.argv[1])) if int(r["vmrss_kb"]) > 0]
hours, alive, budget = float(sys.argv[2]), sys.argv[3] == "1", int(sys.argv[4])
mode, warmup = sys.argv[5], float(sys.argv[6])
alert_lines, tier_broken = int(sys.argv[7]), sys.argv[8] == "1"
dead_ratio_pct = float(sys.argv[9]) if len(sys.argv) > 9 else 50.0
fails = []
if not alive:
    fails.append("server died during the soak (see infinityd.log)")
# ADR-0071 D4. Alerts were advisory until 2026-08-08, when a run raised
# three of them, lost its tiered workload, and still stamped PASS. An
# alert is the run telling you it stopped being the run you launched.
if alert_lines > 0:
    fails.append(
        f"{alert_lines} alert line(s) in alerts.log — an alerting soak is not a clean soak; "
        "root-cause each one in run-notes.md (§19) and re-run"
    )
if tier_broken:
    fails.append(
        "tiered leg broke mid-run (tier-leg-broken.txt) — the tiered plane stopped serving, so "
        "this run cannot speak for the tiered profile whatever its memory series say"
    )
if len(rows) < 6:
    fails.append(f"only {len(rows)} samples — sampler failure invalidates the run")
else:
    med = lambda xs: sorted(xs)[len(xs) // 2]
    def sl(rs, key):
        # First/last-5% medians, storm-resistant, normalized to %/24 h of
        # the window's actual timestamp span (not the nominal duration).
        k = max(1, len(rs) // 20)
        first = med([int(r[key]) for r in rs[:k]])
        last = med([int(r[key]) for r in rs[-k:]])
        span_h = (int(rs[-1]["unix_s"]) - int(rs[0]["unix_s"])) / 3600
        return (last - first) / max(first, 1) * 100 / (max(span_h, 1e-9) / 24), first, last
    # ADR-0069 D1: the RSS gate binds over a prospectively declared
    # steady-state window (first `warmup` hours excluded). The citation
    # form is a 32 h run = 8 h warm-up + 24 h window. Runs too short for
    # the window (<= 2x warm-up) gate on the whole run and say so.
    t0 = int(rows[0]["unix_s"])
    steady = [r for r in rows if int(r["unix_s"]) >= t0 + warmup * 3600]
    windowed = hours > 2 * warmup and len(steady) >= 60
    s_all, f_all, l_all = sl(rows, "vmrss_kb")
    print(f"rss slope whole-run {s_all:+.3f}%/24h (first-5% median {f_all} kB -> last-5% median {l_all} kB, {len(rows)} samples; disclosure)")
    if windowed:
        s_gate, f_st, l_st = sl(steady, "vmrss_kb")
        print(f"rss slope steady-state {s_gate:+.3f}%/24h (first {warmup:g} h excluded per ADR-0069 D1, {len(steady)} samples, {f_st} kB -> {l_st} kB; GATE)")
    else:
        s_gate = s_all
        print(f"steady-state window not applicable (run {hours:g} h <= 2x warm-up {warmup:g} h) — whole-run slope gates; not the ADR-0069 citation form")
    if s_gate >= 0.5:
        fails.append(f"RSS slope {s_gate:+.3f}%/24h >= 0.5%/24h" + (" (steady-state window)" if windowed else ""))
    # ADR-0069 D2: accounted slope is a HARD sub-gate — a data-structure
    # leak cannot hide inside allocator/high-water noise. Missing or zero
    # accounted series fails the run outright.
    if "used_memory_kb" in rows[0] and int(rows[-1]["used_memory_kb"]) > 0:
        u_all, uf, ul = sl(rows, "used_memory_kb")
        print(f"accounted slope whole-run {u_all:+.3f}%/24h (used_memory first-5% median {uf} kB -> last-5% {ul} kB; disclosure)")
        if windowed:
            u_gate = sl(steady, "used_memory_kb")[0]
            print(f"accounted slope steady-state {u_gate:+.3f}%/24h (GATE)")
        else:
            u_gate = u_all
        if u_gate >= 0.5:
            fails.append(f"accounted slope {u_gate:+.3f}%/24h >= 0.5%/24h — data-structure growth, not allocator shape")
    else:
        fails.append("accounted series (used_memory_kb) missing or zero — ADR-0069 D2 requires it")
    # ADR-0069 D3 disclosure: end-of-run attribution reconciliation. The
    # tier/staging/ckpt columns are node-summed by the sampler; fixed boot
    # overhead (uring + fabric rings, stacks, text, recycle pools) lands
    # in the residual (~30 MB expected). Disclosure, not a gate — the
    # attribution table in the bundle carries the full model.
    if "tier_committed_bytes" in rows[-1] and int(rows[-1].get("tier_committed_bytes") or 0) >= 0:
        r = rows[-1]
        named_kb = (int(r["tier_committed_bytes"]) + int(r["tier_index_bytes"])
                    + int(r["log_staging_bytes"]) + int(r["ckpt_buffer_bytes"])) // 1024
        model_kb = int(r["used_memory_kb"]) + named_kb
        rss_kb = int(r["vmrss_kb"])
        resid = rss_kb - model_kb
        print(f"attribution (end): rss {rss_kb} kB vs used_memory + tier_committed + tier_index + staging + ckpt = {model_kb} kB — residual {resid} kB ({resid / max(rss_kb, 1) * 100:+.1f}%; disclosure)")
    disk_max = max(int(r["disk_used_bytes"]) for r in rows)
    print(f"tiering disk max {disk_max} bytes (budget {budget})")
    if disk_max > budget:
        fails.append(f"disk {disk_max} exceeded budget {budget}")
    wa_max = max(int(r["wa_milli_max"]) for r in rows)
    print(f"write-amp milli max {wa_max} (gate < 3000)")
    if wa_max >= 3000:
        fails.append(f"write amplification {wa_max / 1000:.3f}x breached the 3x gate")
    refusals = max(int(r["diskfull_refusals"]) for r in rows)
    print(f"diskfull refusals {refusals}")
    if refusals > 0:
        fails.append(f"{refusals} DISKFULL refusals — the run hit its disk budget")
    docs = [int(r["docs_live"]) for r in rows]
    print(f"docs_live {docs[0]} -> {docs[-1]} (document plane live throughout: {'yes' if docs[-1] > 0 else 'NO'})")
    if docs[-1] == 0:
        fails.append("document plane reported zero live documents at the end — the doc legs did not run")
    ck = [int(r["ckpts"]) for r in rows]
    print(f"checkpoints completed {ck[0]} -> {ck[-1]} (delta {ck[-1] - ck[0]})")
    if ck[-1] - ck[0] <= 0:
        fails.append("no checkpoints completed — the durable plane was not exercised")
    # ADR-0071 D4: tiered-plane liveness over the *measured* window. Every
    # memory gate above can pass on a node whose tier is a frozen 2.7 GiB
    # of committed regions; the 20260808 run proved it. These three
    # counters are what "the tiered plane was under load" reduces to, and
    # `compact_slices` is specifically the S23 story title ("24 h
    # endurance with compaction active") made checkable.
    window = steady if windowed else rows
    live = {
        "cold_reads_issued": "cold reads",
        "flush_slices": "flush slices",
        "compact_slices": "compaction slices",
    }
    scope = "steady window" if windowed else "whole run"
    # readiness F22: the reclaim series, sampled since 2026-08-12. It does
    # not gate — it explains. `compact_slices == 0` is a FAIL either way
    # (the story's title is "endurance WITH COMPACTION ACTIVE"), but the
    # two causes have opposite severity and the message must say which.
    dead_delta = idle_delta = peak_ratio = None
    if "dead_bytes" in rows[0] and "live_bytes" in rows[0]:
        d0, d1 = int(window[0]["dead_bytes"]), int(window[-1]["dead_bytes"])
        dead_delta = d1 - d0
        i0, i1 = int(window[0]["compact_idle_pressure"]), int(window[-1]["compact_idle_pressure"])
        idle_delta = i1 - i0
        # Compaction triggers on the RATIO, so the ratio is what the
        # verdict must reason about. Peak, not final: compaction that ran
        # would have pulled the ratio back down, so the highest ratio the
        # run ever reached is the honest "did it ever qualify?" statistic.
        ratios = [int(r["dead_bytes"]) / max(int(r["dead_bytes"]) + int(r["live_bytes"]), 1) * 100
                  for r in window]
        peak_ratio = max(ratios)
        print(f"tier reclaim: dead_bytes {d0} -> {d1} over the {scope} (delta {dead_delta}); "
              f"live_bytes {int(window[-1]['live_bytes'])}; peak dead ratio {peak_ratio:.2f}% "
              f"vs the {dead_ratio_pct}% compaction trigger; "
              f"compact_idle_pressure delta {idle_delta}")
    if all(c in rows[0] for c in live):
        for column, label in live.items():
            first, last = int(window[0][column]), int(window[-1][column])
            print(f"tier liveness: {column} {first} -> {last} over the {scope} (delta {last - first})")
            if mode == "FULL" and last - first <= 0:
                why = ""
                if column == "compact_slices" and peak_ratio is not None:
                    # The discriminator is the ratio against the trigger,
                    # NOT an absolute byte count. Compaction is *supposed*
                    # to stay idle below the trigger, so "lots of dead
                    # bytes" alone proves nothing: the 20260812 shake hit
                    # 641 MB dead — alarming in isolation, 2.8% of live in
                    # fact, and correctly no compaction.
                    if peak_ratio >= dead_ratio_pct:
                        why = (f" — and the dead ratio reached {peak_ratio:.2f}%, at or past the "
                               f"{dead_ratio_pct}% trigger, while compaction stayed idle "
                               f"(compact_idle_pressure delta {idle_delta}): this is the ENGINE-SIDE "
                               "shape, escalate before spending another run")
                    else:
                        why = (f" — but the dead ratio only reached {peak_ratio:.2f}% against a "
                               f"{dead_ratio_pct}% trigger, so compaction was never eligible to run: "
                               "the run never posed the question. This is the WORKLOAD/CONFIG shape "
                               "— lengthen the run, raise write pressure, or lower the memory budget "
                               "(the trigger scales with the live dataset), not an engine fault")
                fails.append(
                    f"tiered plane served no {label} across the {scope} — the tier was static, so "
                    "the M4 §7 rows measured a node that was not doing the work they describe"
                    + why
                )
    elif mode == "FULL":
        fails.append(
            "tier-liveness columns absent from samples.csv — this sampler predates ADR-0071 D4 "
            "and cannot prove the tiered plane was under load"
        )
verdict = "FAIL" if fails else "PASS"
# The stamp is decided by what the run DID, never by what it was launched
# as. `mode` is fixed at boot from a wiring probe; a leg that dies at
# +35 min cannot be allowed to keep the launch-time claim (ADR-0071 D4).
tier_served = mode == "FULL" and not tier_broken
if fails:
    # A failing run discharges nothing. Printing a "discharges:" list next
    # to FAIL is how the 20260807 verdict read, and it invites exactly the
    # misquote it looks like.
    stamp = " — discharges: NOTHING (a failing run is not evidence for any gate)"
elif tier_served:
    stamp = " — discharges: M2.5 stability soak, M3 §7 doc soak, M4 §7 memory honesty"
elif mode == "FULL":
    stamp = (" (PARTIAL: launched FULL, but the tiered leg broke mid-run)"
             "\n  discharges: M2.5 stability soak, M3 §7 doc soak"
             "\n  does NOT discharge: M4 §7 memory honesty (the tiered plane stopped serving)")
else:
    stamp = (" (TIER-MECHANICS-ONLY: tiered namespace stands but serves no traffic — M4-S26)"
             "\n  discharges: M2.5 stability soak, M3 §7 doc soak"
             "\n  does NOT discharge: M4 §7 memory honesty (needs the wired tiered leg)")
print(f"{verdict}{stamp}")
for f in fails:
    print(f"  - {f}")
EOF

ALERTS=$(wc -l <"$OUT/alerts.log" 2>/dev/null || echo 0)
echo "soak-unified: done — $ALERTS alert lines; artifacts in $OUT"
