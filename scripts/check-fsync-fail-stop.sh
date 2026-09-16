#!/usr/bin/env bash
# fsync fail-stop gate (M2-S17, §3.3/§8.4 — the PostgreSQL fsyncgate
# lesson): an fsync failure surfaces as a typed, non-recoverable error and
# **no caller may catch and continue**. Rewritten at ADR-0106 D10 (review
# 2026-08-30, F-L20-06); the old gate had two structural gaps, both proven
# on this tree:
#
#   1. its allow-list was per FILE, and the ten allow-listed files include
#      `inf-log/src/commit.rs` (1,722 lines), `segment.rs` (1,700) and
#      `inf-server/src/durable.rs` (2,358) — exactly where such a handler
#      would be written. A planted
#      `if let Err(LogError::Fsync(_)) = r { /* continue */ }` in
#      `commit.rs` left it printing "OK (10 audited sites)";
#   2. its pattern set was five hand-listed variant names, so a raw
#      `let _ = file.sync_data();` planted in `segment.rs` — the fsyncgate
#      poison with no named type at all — was equally invisible.
#
# Now:
#   * the type set is DERIVED from the source (every enum in `crates/*/src`
#     with an `Fsync` variant, plus every `*Fsync*` error struct), so a
#     future `CkptWriteFailure::Fsync` is under the gate the moment it is
#     declared; the derived set must still cover the audited minimum, or
#     the derivation itself is a scope error;
#   * the allow-list is per SITE: `fsync-fail-stop-allow: <reason>` on the
#     line or the line directly above (the deny-list / panic-policy marker
#     shape, ADR-0106 D4). Bare markers and stale markers fail; every
#     reason is printed on every run;
#   * a second rule flags a raw `sync_data`/`sync_all`/`fdatasync`/
#     `libc::fsync` whose result is DISCARDED (`let _ =`, `.ok()`,
#     `.unwrap_or…`, `.is_ok()/.is_err()`, `drop(`) — the act of syncing,
#     not the spelling of a type;
#   * comments and test-only modules are stripped (ADR-0106 D3): 11 of the
#     41 raw hits in this tree are prose naming the contract, and prose is
#     not a handler.
#
# Portable bash 3.2 / POSIX awk (macOS is in the CI matrix).
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"
NOCOMMENT="$SCRIPT_DIR/rust-strip-comments.awk"
[ -f "$STRIP" ] && [ -f "$NOCOMMENT" ] || { echo "FSYNC SCOPE ERROR: helper awk missing next to $0"; exit 2; }

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "FSYNC SCOPE ERROR: mktemp failed"; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

fail=0

# ---- 1. derive the fsync-error type set --------------------------------
# The audited minimum: the set this gate's allow-list was reviewed against.
# The derivation may only GROW it; a shrink means the parser broke.
REQUIRED="LogError::Fsync FsyncFailed on_fsync_error TierWriteFailure::Fsync TierFlushError::Fsync ExtentWriteFailure::Fsync"
: >"$work/derived"
for dir in crates/*/src; do
    [ -d "$dir" ] || continue
    while IFS= read -r f; do
        awk -f "$NOCOMMENT" "$f" | awk '
            /^[[:space:]]*(pub[[:space:]]+)?enum[[:space:]]+[A-Za-z0-9_]+/ {
                n = $0
                sub(/^.*enum[[:space:]]+/, "", n)
                sub(/[^A-Za-z0-9_].*$/, "", n)
                match($0, /^[[:space:]]*/)
                closer = substr($0, 1, RLENGTH) "}"
                name = n
                inenum = 1
                next
            }
            inenum == 1 && $0 == closer { inenum = 0; next }
            inenum == 1 && /^[[:space:]]*Fsync[[:space:]]*[({,]/ { print name "::Fsync" }
            # Error-shaped only: FsyncTicket / FsyncClass are plumbing.
            /^[[:space:]]*(pub[[:space:]]+)?struct[[:space:]]+[A-Za-z0-9_]*Fsync[A-Za-z0-9_]*/ {
                n = $0
                sub(/^.*struct[[:space:]]+/, "", n)
                sub(/[^A-Za-z0-9_].*$/, "", n)
                if (n ~ /(Failed|Error)/) { print n }
            }
            /pub fn on_fsync_error/ { print "on_fsync_error" }
        ' >>"$work/derived"
    done < <(find "$dir" -name '*.rs' | sort)
done
sort -u "$work/derived" -o "$work/derived"
for p in $REQUIRED; do
    grep -qx "$p" "$work/derived" || {
        echo "FSYNC SCOPE ERROR: the derived type set lost \"$p\" — the derivation broke, or the type was renamed"
        echo "  (derived: $(tr '\n' ' ' <"$work/derived"))"
        fail=1
    }
done
derived_count=$(wc -l <"$work/derived" | tr -d ' ')
PATTERN=$(awk '{ printf "%s%s", (NR>1?"|":""), $0 }' "$work/derived" | sed 's/[.[\*^$]/\\&/g')

# ---- 2. sites, per-site markers -----------------------------------------
files=0
lines=0
sites=0
allowed=0
DISCARD='(let[[:space:]]+_[[:space:]]*=|\.ok\(\)|\.unwrap_or|\.is_ok\(\)|\.is_err\(\)|drop\()'
# `[(]`, not `\(`: awk -v unescapes the backslash, and BSD awk then refuses
# the unbalanced group ("illegal primary") — the gate was red on macOS.
SYNCCALL='(sync_data|sync_all|fdatasync|libc::fsync)[[:space:]]*[(]'
syncs=0
# marker-up.awk (inline): from site line n, look at n itself and then up
# through the whole-line `//` comment block above it; print the reason
# (want=reason) or "1" for a bare marker (want=bare).
MARKER_UP="$work/marker-up.awk"
cat >"$MARKER_UP" <<'AWK'
{ line[NR] = $0 }
END {
    for (i = n; i >= 1; i--) {
        if (i < n && line[i] !~ /^[[:space:]]*\/\//) break
        if (match(line[i], /fsync-fail-stop-allow:[[:space:]]*[^ ].*/)) {
            if (want == "reason") {
                r = substr(line[i], RSTART + 22, RLENGTH - 22)
                sub(/^[[:space:]]*/, "", r); sub(/[[:space:]]*\*\/[[:space:]]*$/, "", r)
                # continuation lines of the wrapped reason, up to the site
                for (j = i + 1; j < n; j++) {
                    c = line[j]; sub(/^[[:space:]]*\/\/[[:space:]]*/, "", c)
                    if (c != "") r = r " " c
                }
                if (r != "") { print r; exit }
            }
        } else if (line[i] ~ /fsync-fail-stop-allow:[[:space:]]*$/ && want == "bare") { print "1"; exit }
    }
}
AWK
for dir in crates/*/src bins/*/src; do
    [ -d "$dir" ] || continue
    while IFS= read -r f; do
        files=$((files + 1))
        lines=$((lines + $(wc -l <"$f")))
        # `use` / `pub use` statements are blanked: an import or a
        # re-export names the typed error, it can never catch one.
        awk -f "$STRIP" "$f" | awk -f "$NOCOMMENT" | awk '
            inuse == 1 { if ($0 ~ /;/) { inuse = 0 }; print ""; next }
            /^[[:space:]]*(pub([[:space:]]*\([a-z:]+\))?[[:space:]]+)?use[[:space:]]/ {
                if ($0 !~ /;/) { inuse = 1 }
                print ""
                next
            }
            { print }' >"$work/code"
        # Markers live in the comments, so they are read from the raw file.
        while IFS=: read -r n text; do
            sites=$((sites + 1))
            # The marker heads the comment block directly above the site
            # (its reason may wrap onto further `//` lines — batch 64, the
            # 100-column gate) or sits on the site line itself.
            reason=$(awk -v n="$n" -f "$MARKER_UP" -v want=reason "$f")
            bare=$(awk -v n="$n" -f "$MARKER_UP" -v want=bare "$f")
            if [ -n "$reason" ]; then
                allowed=$((allowed + 1))
                echo "  audited $f:$n — $reason"
            elif [ -n "$bare" ]; then
                echo "UNAUDITED fsync-error site: $f:$n has a bare marker (no reason)"
                fail=1
            else
                echo "UNAUDITED fsync-error site: $f:$n:${text# }"
                fail=1
            fi
        done < <(grep -nE "$PATTERN" "$work/code" || true)
        # A raw sync whose result is thrown away — no named type involved.
        while IFS=: read -r n text; do
            syncs=$((syncs + 1))
            case "$text" in
                *"fn sync_data"*|*"fn sync_all"*) continue ;;
            esac
            # Same statement only: the continuation line counts only when
            # line n did not terminate (`staged.sync_data()?;` followed by
            # an unrelated `drop(staged);` is not a discard).
            window=$(awk -v n="$n" 'NR==n { print; if ($0 ~ /;[[:space:]]*$/) exit } NR==n+1 { print }' "$work/code")
            printf '%s' "$window" | grep -qE "$DISCARD" || continue
            if [ -n "$(awk -v n="$n" -f "$MARKER_UP" -v want=reason "$f")" ]; then
                continue
            fi
            echo "UNAUDITED discarded sync result: $f:$n:${text# }"
            fail=1
        done < <(grep -nE "$SYNCCALL" "$work/code" || true)
        # A marker that guards nothing is stale scope.
        while IFS=: read -r n _; do
            # A marker guards its own line or the first code line below its
            # comment block (the code view has comments blanked, so the
            # continuation lines are empty there).
            if ! awk -v n="$n" -v pat="$PATTERN" -v sc="$SYNCCALL" '
                NR==n { if ($0 ~ pat || $0 ~ sc) { f=1 } }
                NR>n && !done { if ($0 ~ /^[[:space:]]*$/) next; done=1; if ($0 ~ pat || $0 ~ sc) { f=1 } }
                END { exit !f }' "$work/code"; then
                echo "STALE fsync-fail-stop marker: $f:$n guards no fsync-error site"
                fail=1
            fi
        done < <(grep -n 'fsync-fail-stop-allow:' "$f" || true)
    done < <(find "$dir" -name '*.rs' | sort)
done

scope="$derived_count derived fsync-error patterns, $files files / $lines lines scanned, $sites typed sites ($allowed audited), $syncs raw sync call sites"
if [ "$fail" -ne 0 ]; then
    echo "fsync fail-stop FAILED ($scope)"
    echo "fsync failure is fail-stop (§8.4): prove the site is terminal and mark it"
    echo "  // fsync-fail-stop-allow: <why this site cannot catch and continue>"
    exit 1
fi
echo "fsync fail-stop OK ($scope)"
