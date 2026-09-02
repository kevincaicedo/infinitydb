#!/usr/bin/env bash
# Panic-policy grep (M2.5-S13, INFINITY_STYLE §Panics): on the durable and
# cell-resident paths, operating conditions return typed errors — a naked
# `.unwrap()` (or `todo!`/`unimplemented!`) is a review reject. `expect()`
# with an invariant justification is an assertion and is allowed; `panic!`/
# `unreachable!` are for violated internal invariants (audited in
# reviews/ + the interfaces-m2 invariant inventory). Backstops review.
#
# Review 2026-08-30, P1c (F-L00-19, F-L17-08, F-L18-04, F-L19-15,
# F-L20-02) — ADR-0106: the old grep cut each file at its FIRST
# `#[cfg(test)]` under a comment asserting that was safe; `ckpt.rs` gates
# a one-line accessor that way, so 78 % of the checkpoint writer was never
# scanned. It also hand-listed nine files (none of inf-store — the crate
# holding both proven node-kill asserts), skipped a vanished file
# silently, and printed the list length as if it were coverage. Now:
#   - the scanned set is scripts/cell-crates.sh, shared with the cell
#     deny-list (default-in; exclusions with reasons; a missing directory
#     or an empty set is a FAILURE);
#   - test-only modules are stripped structurally by
#     scripts/strip-test-modules.awk (an inline `#[cfg(test)]` item is
#     scanned as production — over-approximation errs safe);
#   - `panic-policy-allow: <reason>` on the line or the one above exempts
#     a site; a bare marker fails; full-line comments never match;
#   - the scope line discloses files/lines scanned, lines stripped, and
#     the inline test-only items scanned as production.
# Banned: naked unwrap and the unfinished-code macros. `expect(`, `assert`,
# `panic!`, `unreachable!` are intentionally NOT banned (assertions /
# audited fail-stops) — the Theme-4 gate (release asserts justified by a
# claim about a caller) is a separate instrument, still owed.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
# shellcheck source=cell-crates.sh
. "$SCRIPT_DIR/cell-crates.sh"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"

# One ERE; awk dynamic-regex safe (bracket classes instead of backslash
# escapes). `[.]unwrap[(][)]` cannot match `unwrap_or(`/`unwrap_or_default(`,
# so no line-level exclusion is needed for them (the old `grep -v unwrap_or`
# dropped a whole line, hiding any `.unwrap()` that shared it — L18 §4).
PATTERN='[.]unwrap[(][)]|todo!|unimplemented!'
MARKER='panic-policy-allow'

DIRS=()
while IFS= read -r dir; do DIRS+=("$dir"); done < <(cell_crate_dirs)

fail=0
files=0
lines=0
stripped=0
inline=0
allowed=0
for dir in "${DIRS[@]}"; do
    testfiles=""
    while IFS= read -r f; do
        while read -r key value; do
            [ "$key" = "modfile" ] || continue
            testfiles="$testfiles
$(dirname "$f")/$value.rs
$(dirname "$f")/$value/mod.rs"
        done < <(awk -v mode=report -f "$STRIP" "$f")
    done < <(find "$dir" -name '*.rs' | sort)
    while IFS= read -r f; do
        case "$testfiles" in *"
$f"*) continue ;; esac
        report=$(awk -v mode=report -f "$STRIP" "$f")
        if printf '%s\n' "$report" | grep -q '^unterminated'; then
            echo "PANIC-POLICY SCOPE ERROR: $f — a test-only module never closed at its own indent"
            echo "  (the stripper would have blanked the rest of the file; run cargo fmt or fix the module)"
            fail=1
            continue
        fi
        n=$(wc -l < "$f")
        s=$(printf '%s\n' "$report" | awk '$1 == "stripped" { print $2 }')
        i=$(printf '%s\n' "$report" | awk '$1 == "inline" { print $2 }')
        files=$((files + 1))
        lines=$((lines + n))
        stripped=$((stripped + s))
        inline=$((inline + i))
        out=$(awk -f "$STRIP" "$f" | awk -v pat="$PATTERN" -v marker="$MARKER" '
            function reason(s) { sub("^.*" marker ":[[:space:]]*", "", s); return s }
            {
                hit = ($0 !~ /^[[:space:]]*\/\//) && ($0 ~ pat)
                if (hit) {
                    withreason = marker ":[[:space:]]*[^[:space:]]"
                    if ($0 ~ withreason) { print "A " NR ": " reason($0) }
                    else if (prev ~ withreason) { print "A " NR ": " reason(prev) }
                    else if ($0 ~ marker || prev ~ marker) { print "V " NR ": " $0 "   <-- " marker " marker without a reason" }
                    else { print "V " NR ": " $0 }
                }
                prev = $0
            }')
        [ -n "$out" ] || continue
        while IFS= read -r row; do
            case "$row" in
                A\ *) allowed=$((allowed + 1)); echo "  allowed $f:${row#A }" ;;
                V\ *) echo "PANIC-POLICY violation (naked unwrap / todo / unimplemented): $f:${row#V }"; fail=1 ;;
            esac
        done <<< "$out"
    done < <(find "$dir" -name '*.rs' | sort)
done

if [ "$files" -eq 0 ]; then
    echo "PANIC-POLICY SCOPE ERROR: no .rs files found under: ${DIRS[*]}"
    exit 1
fi
# The scope line is printed on both verdicts: what was scanned is part of
# the evidence either way.
scope="${#DIRS[@]} crates, $files files, $lines lines scanned, $stripped test-only lines stripped, $inline inline cfg(test) items scanned as production, $allowed allowed sites"
if [ "$fail" -ne 0 ]; then
    echo "panic-policy grep FAILED ($scope)"
    echo "Operating conditions return typed errors (§Panics); use expect(\"<invariant>\")"
    echo "for a justified assertion, or a Result for an operating error."
    exit 1
fi
echo "panic-policy grep OK ($scope, 0 naked unwrap)"
cell_crate_exclusions
