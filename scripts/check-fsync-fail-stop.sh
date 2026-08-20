#!/usr/bin/env bash
# fsync fail-stop grep (M2-S17, §3.3/§8.4 — the PostgreSQL fsyncgate
# lesson): fsync failure surfaces as a typed, non-recoverable error
# (`LogError::Fsync` / `FsyncFailed`, and the terminal `on_fsync_error`
# ledger hook). No caller may catch and continue. Shell cannot parse Rust
# match arms, so the enforceable rule is an audited allowlist: the
# fsync-error types may be referenced only in the files below — the
# definitions/constructions in `inf-log` and the terminal fail-stop
# handlers. A new file referencing them fails the build until it is
# reviewed as fail-stop and added here (with the review note saying why).
set -euo pipefail
cd "$(dirname "$0")/.."

# Audited fail-stop sites (reviewed at M2-S17, ADR-0020 D4; tier rows
# added at M4-S11, ADR-0056 D4):
#   inf-log/segment.rs   — type definitions + the only constructions
#                          (seal fsync, dir-fsync barriers)
#   inf-log/commit.rs    — GroupCommit::on_fsync_error: freezes the
#                          watermark; exists so the freeze is observable,
#                          never so a caller can continue
#   inf-log/lib.rs       — re-export
#   inf-log/fs.rs        — trait doc naming the contract
#   inf-log/fault.rs     — fault-point inventory doc
#   inf-log/tier.rs      — TierWriteFailure::Fsync: the only tier-level
#                          constructions (sync/seal barriers); propagated,
#                          never handled (M4-S11 review)
#   inf-log/flush.rs     — TierFlushError::Fsync: classification only —
#                          the flushed watermark freezes by construction
#                          (no advance happens past a failed barrier) and
#                          the error propagates to the terminal handler
#                          (M4-S11 review)
#   inf-log/blob.rs      — ExtentWriteFailure::Fsync: the ADR-0061 D3
#                          typed **abort** — the one ADR-defined narrower
#                          behavior: at barrier time nothing durable
#                          references the extent, the file is abandoned
#                          (never retried — the fsyncgate poison is
#                          structurally absent) and the write fails typed
#                          (M4-S17 review)
#   inf-server/durable.rs — on_log_error/fail_stop: eprintln + exit(3),
#                          the terminal handler
#   inf-server/tier_cell.rs — drive_flush_round: a reactor-drive flush
#                          barrier's error completion re-surfaces as
#                          TierFlushError::Fsync at the next MAINTAIN;
#                          the watermark froze by construction (effects
#                          apply only on success) and maintain_ns
#                          propagates it to the plane's fatal arm →
#                          DurableCell::fail_stop. Construction only,
#                          never caught (M4.5-S31 review, ADR-0084 D4)
ALLOW=(
    crates/inf-log/src/segment.rs
    crates/inf-log/src/commit.rs
    crates/inf-log/src/lib.rs
    crates/inf-log/src/fs.rs
    crates/inf-log/src/fault.rs
    crates/inf-log/src/tier.rs
    crates/inf-log/src/flush.rs
    crates/inf-log/src/blob.rs
    crates/inf-server/src/durable.rs
    crates/inf-server/src/tier_cell.rs
)

fail=0
while IFS= read -r hit; do
    file=${hit%%:*}
    ok=0
    for allowed in "${ALLOW[@]}"; do
        [ "$file" = "$allowed" ] && ok=1 && break
    done
    if [ "$ok" -eq 0 ]; then
        echo "UNAUDITED fsync-error handling: $hit"
        fail=1
    fi
done < <(grep -rn --include='*.rs' -e 'LogError::Fsync' -e 'FsyncFailed' -e 'on_fsync_error' -e 'TierWriteFailure::Fsync' -e 'TierFlushError::Fsync' -e 'ExtentWriteFailure::Fsync' crates/*/src bins/*/src)

if [ "$fail" -ne 0 ]; then
    echo "fsync fail-stop grep FAILED: fsync errors are fail-stop (§8.4);"
    echo "prove the new site is terminal and extend the allowlist in this script."
    exit 1
fi
echo "fsync fail-stop grep OK (${#ALLOW[@]} audited sites)"
