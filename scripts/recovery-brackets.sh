#!/usr/bin/env bash
# M4 §7 recovery sub-gate — the two shapes `recovery-analysis.md` still owes.
#
# The 2026-08-15 leg measured ONE point: the worst-case shape (no checkpoint
# had completed, so the boot replayed the whole WAL tail) with a WARM page
# cache (dropping it needs sudo, which that session did not take). Both
# disclosures were recorded honestly, and both push in the same direction:
# the per-cell replay row FAILs at 0.266 GB/s/cell against a 1 GB/s gate.
#
# This script brackets that number properly by measuring the other two
# corners:
#   leg A  steady-state `ick-tail`  — a checkpoint COMPLETES before the stop,
#                                     so the boot replays only the tail after
#                                     it. Replays ~half as much. Expect this
#                                     to be the FRIENDLIEST corner.
#   leg B  cold page cache          — `vm.drop_caches=3` before the boot, so
#                                     every replayed byte comes off the
#                                     device. Expect the HARSHEST corner.
#
# Neither is expected to change the verdict: at 476k records/s/cell the gate
# needs ~3.8x more RECORD throughput, and neither cache state nor replay
# volume changes the per-record cost. The point is to publish the row with
# its brackets instead of a single unbracketed point — and to find out if
# the per-record rate is actually constant across all three corners, which
# is the load-bearing assumption behind "record-bound, not device-bound"
# (C38b / M4.5-S21).
#
# WHY THIS NEEDS sudo: exactly one line, `sysctl -w vm.drop_caches=3`, and
# only for leg B. Leg A needs no privileges. If you would rather not grant
# it, run with COLD=0 and leg B is skipped and reported as not run.
#
# Usage:
#   sudo -v                      # cache the credential, then:
#   scripts/recovery-brackets.sh [OUT_DIR]
#
# Env:
#   BIN        infinityd binary            (default: campaign 6bd25b1)
#   BENCH      inf-bench binary            (default: ./target/release/inf-bench)
#   DATA_ROOT  data dir parent, NOT tmpfs  (default: $HOME/.cache/inf-tmp)
#   CELLS      cell count                  (default: 4)
#   COLD       run leg B                   (default: 1)

set -euo pipefail

OUT="${1:-.artifacts/m4/s24/recovery-brackets}"
BIN="${BIN:-$HOME/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1}"
BENCH="${BENCH:-./target/release/inf-bench}"
DATA_ROOT="${DATA_ROOT:-$HOME/.cache/inf-tmp}"
CELLS="${CELLS:-4}"
COLD="${COLD:-1}"
PORT="${PORT:-6499}"

mkdir -p "$OUT"
: >"$OUT/run.log"
log() { echo "$@" | tee -a "$OUT/run.log"; }

# --- preconditions -----------------------------------------------------
[ -x "$BIN" ]   || { echo "no infinityd at $BIN" >&2; exit 1; }
[ -x "$BENCH" ] || { echo "no inf-bench at $BENCH" >&2; exit 1; }

FS=$(df -T "$DATA_ROOT" | tail -1 | awk '{print $2}')
[ "$FS" = "tmpfs" ] && { echo "DATA_ROOT is tmpfs — a recovery number off a RAM disk is meaningless" >&2; exit 1; }

if [ "$COLD" = "1" ] && ! sudo -n true 2>/dev/null; then
    echo "leg B needs a cached sudo credential; run 'sudo -v' first, or set COLD=0" >&2
    exit 1
fi

"$BENCH" env-check | tee -a "$OUT/run.log"

{
    echo "date: $(date -Is)"
    echo "governor: $(cat /sys/devices/system/cpu/cpu4/cpufreq/scaling_governor)"
    echo "no_turbo: $(cat /sys/devices/system/cpu/intel_pstate/no_turbo)"
    echo "epp: $(cat /sys/devices/system/cpu/cpu4/cpufreq/energy_performance_preference)"
    echo "kernel: $(uname -r)"
    echo "tree: $(git rev-parse --short HEAD)"
    echo "infinityd: $BIN ($(sha256sum "$BIN" | cut -c1-16))"
    df -T "$DATA_ROOT" | tail -1
} >"$OUT/box-state.txt"

# A stray server on the measured cores is exactly what contaminated the
# 2026-08-16 assembly leg (readiness F31) — an abandoned recovery-cold node
# burning ~4% of cpu 5 while env-check reported a clean box. Refuse up front
# rather than discover it in the numbers.
if STRAY=$(pgrep -ax infinityd 2>/dev/null) && [ -n "$STRAY" ]; then
    log "!! an infinityd is already running — stop it before measuring:"
    log "$STRAY"
    exit 1
fi

redis() { redis-cli -p "$PORT" "$@"; }

wait_ready() {
    for _ in $(seq 1 200); do
        [ "$(redis PING 2>/dev/null)" = "PONG" ] && return 0
        sleep 0.05
    done
    return 1
}

stop_node() {
    [ -n "${SRV:-}" ] || return 0
    kill "$SRV" 2>/dev/null || true
    wait "$SRV" 2>/dev/null || true
    SRV=""
}
trap 'stop_node' EXIT

# --- one leg -----------------------------------------------------------
# $1 = leg name · $2 = 1 to complete a checkpoint before the stop
#                 $3 = 1 to drop the page cache before the boot
run_leg() {
    local name="$1" ckpt="$2" cold="$3"
    local dir="$DATA_ROOT/recovery-$name-$$"
    rm -rf "$dir"; mkdir -p "$dir"
    log ""
    log "======== leg $name (checkpoint=$ckpt cold-cache=$cold) ========"

    "$BIN" --port "$PORT" --cells "$CELLS" --data-dir "$dir" \
        >"$OUT/$name-boot1.log" 2>&1 &
    SRV=$!
    wait_ready || { log "node never answered PING"; exit 1; }

    log "-- fill 10 GiB into a tiered namespace"
    local t0 t1
    t0=$(date +%s.%N)
    "$BENCH" ycsb --mem-budget-mb 1024 --dataset-multiple 10 --duration 20 \
        --conns 8 --pipeline 8 --cells "$CELLS" \
        --data-root "$dir" --infinityd-bin "$BIN" \
        --artifacts-root "$OUT/$name-fill" >>"$OUT/run.log" 2>&1 || true
    t1=$(date +%s.%N)
    log "fill wall: $(echo "$t1 - $t0" | bc) s"

    if [ "$ckpt" = "1" ]; then
        log "-- BGSAVE, then wait for the checkpoint to COMPLETE (the ick-tail shape)"
        local before after
        before=$(redis INFO persistence | grep -oP 'ckpt_last_unix_ms:\K[0-9]+' || echo 0)
        redis BGSAVE >>"$OUT/run.log"
        for _ in $(seq 1 600); do
            after=$(redis INFO persistence | grep -oP 'ckpt_last_unix_ms:\K[0-9]+' || echo 0)
            if [ "$(redis INFO persistence | grep -oP 'rdb_bgsave_in_progress:\K[0-9]+')" = "0" ] \
               && [ "$after" -gt "$before" ]; then
                log "checkpoint completed (ckpt_last_unix_ms $before -> $after)"
                break
            fi
            sleep 0.5
        done
        [ "${after:-0}" -gt "${before:-0}" ] || { log "!! checkpoint never completed — leg $name is INVALID"; return 1; }
    else
        log "-- no checkpoint (tail-only shape, matches the 2026-08-15 leg)"
    fi

    redis INFO persistence >"$OUT/$name-info-persistence-before.txt"
    redis INFO tiering     >"$OUT/$name-info-tiering-before.txt"
    du -sb "$dir" | tee -a "$OUT/run.log" >"$OUT/$name-image-bytes.txt"

    log "-- clean stop"
    stop_node

    if [ "$cold" = "1" ]; then
        log "-- drop page cache (sudo sysctl vm.drop_caches=3)"
        sync; sudo sysctl -w vm.drop_caches=3 >>"$OUT/run.log"
        grep -E '^(MemFree|Cached):' /proc/meminfo | tee -a "$OUT/run.log"
    else
        log "-- page cache left WARM (disclose it)"
    fi

    log "-- boot on the filled image, timed to first PONG"
    t0=$(date +%s.%N)
    "$BIN" --port "$PORT" --cells "$CELLS" --data-dir "$dir" \
        >"$OUT/$name-boot2.log" 2>&1 &
    SRV=$!
    wait_ready || { log "node never recovered"; exit 1; }
    t1=$(date +%s.%N)
    log "BOOT WALL ($name): $(echo "$t1 - $t0" | bc) s"

    redis INFO tiering >"$OUT/$name-info-tiering-after.txt"
    stop_node

    # The per-cell figure MUST come from boot2.log's replayed byte/record
    # counts, never from `du` of the directory: tier files are in the
    # directory but recovery does not replay them (the 2026-08-15 harness
    # made exactly that mistake and overstated the row by ~2.6x).
    log "-- replayed, per boot2.log (NOT du):"
    grep -E "cell [0-9]+ recovered|recovery complete" "$OUT/$name-boot2.log" | tee -a "$OUT/run.log"
    rm -rf "$dir"
}

run_leg ick-tail 1 0
[ "$COLD" = "1" ] && run_leg cold-cache 0 1 || log "leg B (cold cache) SKIPPED — COLD=0"

log ""
log "================================================================"
log "Now write the summary into recovery-analysis.md. Required reading"
log "before you quote anything:"
log "  * per-cell GB/s = (bytes in boot2.log) / cells / boot wall."
log "    NEVER du the data directory — it contains tier files recovery"
log "    never replays."
log "  * the load-bearing question is whether RECORDS/s/cell is the same"
log "    in all three corners (476k on 2026-08-15). If it is, 'record-"
log "    bound, not device-bound' is established and M4.5-S21 has its"
log "    target. If the cold leg is much slower, the story is mixed and"
log "    C38b's wording must change."
log "================================================================"
