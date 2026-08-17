#!/usr/bin/env bash
# M4 §7 recovery sub-gate — the two shapes `recovery-analysis.md` still owes.
#
# The 2026-08-15 leg measured ONE point: the worst-case shape (no checkpoint
# had completed, so the boot replayed the whole WAL tail) with a WARM page
# cache. Both disclosures push the same way, so the resulting FAIL
# (0.266 GB/s/cell against a 1 GB/s gate) is conservative. This script
# measures the other two corners of that 2x2 so the row ships bracketed:
#
#   leg A  ick-tail   — a checkpoint COMPLETES before the stop, so the boot
#                       replays only the tail after it (~half as much).
#                       Warm cache, as in the 2026-08-15 point. Isolates
#                       the SHAPE variable. Expect the friendliest corner.
#   leg B  cold-cache — tail-only shape as before, but `vm.drop_caches=3`
#                       first, so every replayed byte comes off the device.
#                       Isolates the CACHE variable. Expect the harshest.
#
# The verdict is not expected to move: at 476k records/s/cell the gate needs
# ~3.8x more RECORD throughput. What is actually being tested is whether
# records/s/cell is CONSTANT across all three corners. If it is,
# "record-bound, not device-bound" is established and M4.5-S21 has a hard
# target. If the cold leg collapses, that story is wrong and ledger C38b's
# wording has to change. Running this also settles ledger C20, whose
# "steady-state boot shape" claim is exactly leg A and is currently
# Evidence-pending for v0.4.0.
#
# WHY sudo: exactly one line, `sysctl -w vm.drop_caches=3`, and only for
# leg B. Leg A needs no privileges. `COLD=0` skips leg B entirely.
#
# Usage:
#   sudo -v && scripts/recovery-brackets.sh                 # both legs
#   LEGS=ick-tail COLD=0 scripts/recovery-brackets.sh OUT    # leg A only, no sudo
#   LEGS=cold-cache scripts/recovery-brackets.sh OUT         # leg B only
#   LEGS=tail-only COLD=0 scripts/recovery-brackets.sh OUT   # same-session repro of the 2026-08-15 point
#
# Env: BIN BENCH DATA_ROOT WORK CELLS COLD PORT MEM_BUDGET_MB DATASET_MULT

set -euo pipefail

OUT="${1:-.artifacts/m4/s24/recovery-brackets}"        # in-tree, written LAST
WORK="${WORK:-$HOME/.cache/inf-campaign/recovery-brackets}"
BIN="${BIN:-$HOME/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1}"
BENCH="${BENCH:-./target/release/inf-bench}"
DATA_ROOT="${DATA_ROOT:-$HOME/.cache/inf-tmp}"
CELLS="${CELLS:-4}"
COLD="${COLD:-1}"
PORT="${PORT:-6499}"
MEM_BUDGET_MB="${MEM_BUDGET_MB:-1024}"                 # matches the 2026-08-15 leg
DATASET_MULT="${DATASET_MULT:-10}"                     # => 10 GiB of user data
NS="${NS:-ycsb}"
LEGS="${LEGS:-ick-tail,cold-cache}"                    # comma list; run one leg at a time if you like

# ---------------------------------------------------------------------------
# Phase 0 — every check that can fail runs BEFORE anything is written inside
# the checkout. `.artifacts/` is TRACKED here, so creating a log file in it
# dirties the tree and fails the very env-check we are about to run (and the
# inner `inf-bench ycsb` env-check later). All working output goes to $WORK,
# outside the checkout — the same rule the S24 runbook already applies to
# campaign binaries and INF_GATERUN_STDERR_DIR.
# ---------------------------------------------------------------------------
[ -x "$BIN" ]   || { echo "no infinityd at $BIN" >&2; exit 1; }
[ -x "$BENCH" ] || { echo "no inf-bench at $BENCH (cargo build --release -p inf-bench)" >&2; exit 1; }
command -v redis-cli >/dev/null || { echo "redis-cli not on PATH" >&2; exit 1; }

case "$OUT" in /*) echo "OUT must be a repo-relative path" >&2; exit 1;; esac
[ -e "$OUT" ] && { echo "$OUT already exists — move or delete it first (a stale dir dirties the tree)" >&2; exit 1; }

FS=$(df -T "$DATA_ROOT" 2>/dev/null | tail -n 1 | awk '{print $2}')
[ "$FS" = "tmpfs" ] && { echo "DATA_ROOT ($DATA_ROOT) is tmpfs — a recovery number off a RAM disk is meaningless" >&2; exit 1; }

# A stray server on the measured cores is what contaminated the 2026-08-16
# assembly leg (readiness F31) while env-check reported a clean box.
if STRAY=$(pgrep -ax infinityd 2>/dev/null) && [ -n "$STRAY" ]; then
    echo "!! an infinityd is already running — stop it before measuring:" >&2
    echo "$STRAY" >&2
    exit 1
fi

if [ "$COLD" = "1" ] && ! sudo -n true 2>/dev/null; then
    echo "leg B needs a cached sudo credential: run 'sudo -v' first, or set COLD=0" >&2
    exit 1
fi

"$BENCH" env-check || { echo "env-check failed — fix the box before measuring" >&2; exit 1; }

mkdir -p "$WORK"
rm -rf "${WORK:?}"/*
: >"$WORK/run.log"
log() { echo "$@" | tee -a "$WORK/run.log"; }

{
    echo "date: $(date -Is)"
    echo "governor: $(cat /sys/devices/system/cpu/cpu4/cpufreq/scaling_governor)"
    echo "no_turbo: $(cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null || echo n/a)"
    echo "epp: $(cat /sys/devices/system/cpu/cpu4/cpufreq/energy_performance_preference)"
    echo "kernel: $(uname -r)"
    echo "tree: $(git rev-parse --short HEAD) (clean at env-check)"
    echo "infinityd: $BIN ($(sha256sum "$BIN" | cut -c1-16))"
    echo "cells: $CELLS · mem-budget: ${MEM_BUDGET_MB}mb · dataset-multiple: ${DATASET_MULT}"
    df -T "$DATA_ROOT" | tail -n 1
} >"$WORK/box-state.txt"
log "box state -> $WORK/box-state.txt"

redis() { redis-cli -p "$PORT" "$@"; }

# Recovery answers -LOADING until the cells finish replaying, so poll for the
# literal PONG rather than for a successful exit.
wait_ready() {  # $1 = timeout seconds
    local deadline=$(( $(date +%s) + ${1:-120} ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        [ "$(redis PING 2>/dev/null)" = "PONG" ] && return 0
        sleep 0.1
    done
    return 1
}

SRV=""
stop_node() {
    [ -n "$SRV" ] || return 0
    kill "$SRV" 2>/dev/null || true
    wait "$SRV" 2>/dev/null || true
    SRV=""
}
cleanup() { stop_node; [ -n "${LEG_DIR:-}" ] && rm -rf "$LEG_DIR"; }
trap cleanup EXIT

# ---------------------------------------------------------------------------
# One leg. $1 = name · $2 = 1 to complete a checkpoint before the stop
#                       $3 = 1 to drop the page cache before the boot
# ---------------------------------------------------------------------------
run_leg() {
    local name="$1" ckpt="$2" cold="$3"
    LEG_DIR="$DATA_ROOT/recovery-$name-$$"
    rm -rf "$LEG_DIR"; mkdir -p "$LEG_DIR"
    log ""
    log "======== leg $name (checkpoint-first=$ckpt cold-cache=$cold) ========"

    "$BIN" --port "$PORT" --cells "$CELLS" --data-dir "$LEG_DIR" \
        >"$WORK/$name-boot1.log" 2>&1 &
    SRV=$!
    wait_ready 60 || { log "!! node never answered PING on first boot"; return 1; }

    # In attach mode `inf-bench ycsb` does NOT create the namespace and does
    # NOT manage the data dir — which is exactly what we need, because its
    # own DataDirGuard DELETES the directory on drop (ycsb.rs:1272) and would
    # destroy the image we are about to recover from. Create the namespace
    # here with the same shape ycsb uses when it spawns (ycsb.rs:1386-1404).
    local disk_mb=$(( MEM_BUDGET_MB * DATASET_MULT * 4 ))
    log "-- create tiered namespace $NS (MEM-BUDGET ${MEM_BUDGET_MB}mb, DISK-BUDGET ${disk_mb}mb)"
    redis INF.NS CREATE "$NS" MODE durable FSYNC everysec \
        MEM-BUDGET "${MEM_BUDGET_MB}mb" DISK-BUDGET "${disk_mb}mb" | tee -a "$WORK/run.log"

    log "-- fill $(( MEM_BUDGET_MB * DATASET_MULT / 1024 )) GiB via inf-bench ycsb --attach-port"
    local t0 t1
    t0=$(date +%s.%N)
    "$BENCH" ycsb --attach-port "$PORT" --ns "$NS" \
        --mem-budget-mb "$MEM_BUDGET_MB" --dataset-multiple "$DATASET_MULT" \
        --duration 20 --conns 8 --pipeline 8 --cells "$CELLS" \
        --artifacts-root "$WORK/$name-fill" >>"$WORK/run.log" 2>&1 \
        || { log "!! ycsb fill failed — see $WORK/run.log"; return 1; }
    t1=$(date +%s.%N)
    log "fill wall: $(echo "$t1 - $t0" | bc) s"

    redis INFO tiering >"$WORK/$name-info-tiering-before.txt"
    grep -q '^tiering_tables:[1-9]' "$WORK/$name-info-tiering-before.txt" \
        || { log "!! no tiered table after the fill — leg $name is INVALID"; return 1; }

    if [ "$ckpt" = "1" ]; then
        log "-- BGSAVE, then wait for the checkpoint to COMPLETE (the ick-tail shape)"
        local before after deadline
        before=$(redis INFO persistence | grep -oP 'ckpt_last_unix_ms:\K[0-9]+' || echo 0)
        redis BGSAVE | tee -a "$WORK/run.log"
        after=$before
        deadline=$(( $(date +%s) + 600 ))
        while [ "$(date +%s)" -lt "$deadline" ]; do
            after=$(redis INFO persistence | grep -oP 'ckpt_last_unix_ms:\K[0-9]+' || echo 0)
            [ "$(redis INFO persistence | grep -oP 'rdb_bgsave_in_progress:\K[0-9]+')" = "0" ] \
                && [ "$after" -gt "$before" ] && break
            sleep 0.5
        done
        [ "$after" -gt "$before" ] \
            || { log "!! checkpoint never completed — leg $name is INVALID (not a slow leg: an unmeasured one)"; return 1; }
        log "checkpoint completed (ckpt_last_unix_ms $before -> $after)"
    else
        log "-- no checkpoint (tail-only shape, matches the 2026-08-15 leg)"
    fi

    redis INFO persistence >"$WORK/$name-info-persistence-before.txt"
    du -sb "$LEG_DIR" >"$WORK/$name-image-bytes.txt"
    log "image on disk: $(cut -f1 "$WORK/$name-image-bytes.txt") B (INCLUDES tier files that recovery never replays)"

    log "-- clean stop"
    stop_node

    if [ "$cold" = "1" ]; then
        log "-- drop page cache (sudo sysctl vm.drop_caches=3)"
        sync; sudo sysctl -w vm.drop_caches=3 >>"$WORK/run.log"
        grep -E '^(MemFree|Cached):' /proc/meminfo | tee -a "$WORK/run.log"
    else
        log "-- page cache left WARM (disclose it)"
    fi

    log "-- boot on the filled image, timed to first PONG"
    t0=$(date +%s.%N)
    "$BIN" --port "$PORT" --cells "$CELLS" --data-dir "$LEG_DIR" \
        >"$WORK/$name-boot2.log" 2>&1 &
    SRV=$!
    wait_ready 300 || { log "!! node never finished recovery"; return 1; }
    t1=$(date +%s.%N)
    log "BOOT WALL ($name): $(echo "$t1 - $t0" | bc) s"

    redis INFO tiering >"$WORK/$name-info-tiering-after.txt"
    stop_node

    # Per-cell throughput MUST come from boot2.log's replayed byte/record
    # counts, never from `du` of the data directory: tier files live in that
    # directory but recovery does not replay them (it reads them on demand).
    # The 2026-08-15 harness made exactly that mistake and overstated the row
    # ~2.6x before recovery-analysis.md corrected it.
    log "-- replayed, per boot2.log (authoritative — NOT du):"
    grep -E "cell [0-9]+ recovered|recovery complete" "$WORK/$name-boot2.log" \
        | tee -a "$WORK/run.log" || log "   (no recovery lines found — check $WORK/$name-boot2.log)"

    rm -rf "$LEG_DIR"; LEG_DIR=""
}

FAILED=0
want() { case ",$LEGS," in *",$1,"*) return 0;; *) return 1;; esac; }

# Same-session reproduction of the 2026-08-15 point (tail-only + warm).
# Not one of the two owed corners — it exists because this box's drive state
# drifts enough to swamp a shape comparison (F20/F29: 10% -> 34% on the m2
# everysec row from drive state alone). Comparing today's ick-tail against
# an August-15 tail-only confounds shape with drive state; this leg removes
# that. Run it whenever a cross-day recovery comparison is being made.
if want tail-only; then
    run_leg tail-only 0 0 || { log "!! leg tail-only FAILED"; FAILED=1; }
fi

if want ick-tail; then
    run_leg ick-tail 1 0 || { log "!! leg ick-tail FAILED"; FAILED=1; }
else
    log ""; log "leg A (ick-tail) SKIPPED — not in LEGS=$LEGS"
fi

if want cold-cache; then
    if [ "$COLD" = "1" ]; then
        run_leg cold-cache 0 1 || { log "!! leg cold-cache FAILED"; FAILED=1; }
    else
        log ""; log "leg B (cold-cache) SKIPPED — COLD=0 (needs sudo for vm.drop_caches)"
    fi
else
    log ""; log "leg B (cold-cache) SKIPPED — not in LEGS=$LEGS"
fi

# ---------------------------------------------------------------------------
# Only now touch the tree.
# ---------------------------------------------------------------------------
mkdir -p "$OUT"
cp -a "$WORK"/. "$OUT"/
log ""
log "================================================================"
log "artifacts copied to $OUT"
log "commit them:  git add -f $OUT && git commit"
log ""
log "Before quoting anything:"
log "  * per-cell GB/s = (bytes in boot2.log) / cells / boot wall."
log "    NEVER du the data directory — it holds tier files recovery"
log "    never replays."
log "  * the load-bearing question is whether RECORDS/s/cell is the"
log "    same in all three corners (476k on 2026-08-15). If yes,"
log "    'record-bound, not device-bound' is established and"
log "    M4.5-S21 has its target. If the cold leg is much slower,"
log "    ledger C38b's wording must change."
log "  * leg A is the shape ledger C20 claims >= 1 GB/s for; it is"
log "    Evidence-pending until this run lands."
log "================================================================"
exit "$FAILED"
