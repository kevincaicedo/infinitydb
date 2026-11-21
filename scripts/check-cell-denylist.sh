#!/usr/bin/env bash
# Cell-code deny list (M0-S06): no async runtimes, no std sync primitives,
# no thread spawn/park/sleep, no std channels inside cell-resident crates.
# Backstops clippy.toml (type-resolved for Mutex/RwLock/Condvar/sleep;
# this grep is the only enforcement for the rest).
# M4.5-S36 (ADR-0088): no ambient clocks or randomness either — L7's
# injected-time rule was enforced by convention until the device budget
# became the first clock consumer added since it was written.
# ADR-0209 D4: no process environment either. A read takes std's
# process-wide lock and scans the whole block, and its value is ambient
# input. ENV_PATTERN is the second reader of clippy.toml's std::env entries,
# by text, and the proven reader of libc's getenv, setenv, unsetenv and
# putenv, whose clippy entries resolve but have no plant (ADR-0209 A1); it
# carries glibc's secure_getenv and clearenv, which no portable entry names.
# No denylist-allow marker admits one of its hits.
#
# Review 2026-08-30, P1 (F-L00-01, F-L17-03, F-L18-05, F-L19-16,
# F-L20-01) — ADR-0106: the list named `crates/inf-server/src/cell`, a
# directory that did not exist, and `[ -d ] || continue` skipped it
# silently, so the command plane was never scanned and the script said
# OK for the whole life of the crate; inf-log and inf-query were never
# listed; the header promised "no thread spawn" with no spawn pattern.
# Now:
#   - the scanned set is scripts/cell-crates.sh (default-in, exclusions
#     with reasons); a missing directory or an empty set is a FAILURE;
#   - test-only modules are stripped by scripts/strip-test-modules.awk
#     (wall clocks for scratch-dir names live there legitimately);
#   - a sanctioned site carries `denylist-allow: <reason>` on its own
#     line or the line above (the control thread, the boot prefetch
#     thread, the injected clock's origin); a bare marker fails;
#   - the scope line discloses crates/files/lines scanned, lines
#     stripped and every allowed site.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
# shellcheck source=cell-crates.sh
. "$SCRIPT_DIR/cell-crates.sh"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"

# One ERE; awk dynamic-regex safe (no backslash escapes).
PATTERN='tokio::|async_std::|std::sync::Mutex|std::sync::RwLock|std::sync::Condvar|std::sync::mpsc|thread::sleep|thread::spawn|thread::Builder|thread::park|Instant::now|SystemTime::now|rand::'
# `env::var` covers var_os, vars and vars_os; `getenv` covers secure_getenv;
# `setenv` covers unsetenv. A hit is red with or without a marker.
ENV_PATTERN='env::var|env::set_var|env::remove_var|env::temp_dir|env::home_dir|getenv|setenv|putenv|clearenv'
MARKER='denylist-allow'

# Each listing is read whole before its loop, so a walk that fails fails the gate.
crate_set=$(cell_crate_dirs) || {
    echo "DENY-LIST SCOPE ERROR: the cell crate set is unknown (scripts/cell-crates.sh, above)"
    exit 2
}
DIRS=()
while IFS= read -r dir; do DIRS+=("$dir"); done <<<"$crate_set"

fail=0
files=0
lines=0
stripped=0
allowed=0
for dir in "${DIRS[@]}"; do
    listing=$(find "$dir" -name '*.rs' | sort) || {
        echo "DENY-LIST SCOPE ERROR: the walk of $dir failed, so its files are unread"
        exit 2
    }
    # Files declared `mod name;` under a test-only attribute are test-only
    # in their entirety (report mode names them); everything else is scanned.
    testfiles=""
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        while read -r key value; do
            [ "$key" = "modfile" ] || continue
            testfiles="$testfiles
$(dirname "$f")/$value.rs
$(dirname "$f")/$value/mod.rs"
        done < <(awk -v mode=report -f "$STRIP" "$f")
    done <<<"$listing"
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        case "$testfiles" in *"
$f"*) continue ;; esac
        report=$(awk -v mode=report -f "$STRIP" "$f")
        if printf '%s\n' "$report" | grep -q '^unterminated'; then
            echo "DENY-LIST SCOPE ERROR: $f — a test-only module never closed at its own indent"
            echo "  (the stripper would have blanked the rest of the file; run cargo fmt or fix the module)"
            fail=1
            continue
        fi
        n=$(wc -l < "$f")
        s=$(printf '%s\n' "$report" | awk '$1 == "stripped" { print $2 }')
        files=$((files + 1))
        lines=$((lines + n))
        stripped=$((stripped + s))
        # A hit is allowed when its line or the line above carries the
        # marker WITH a reason; full-line comments never execute. An
        # environment hit is never allowed.
        out=$(awk -f "$STRIP" "$f" | awk -v pat="$PATTERN" -v envpat="$ENV_PATTERN" -v marker="$MARKER" '
            function reason(s) { sub("^.*" marker ":[[:space:]]*", "", s); return s }
            {
                code = ($0 !~ /^[[:space:]]*\/\//)
                hit = code && ($0 ~ pat)
                if (code && $0 ~ envpat) {
                    print "V " NR ": " $0 "   <-- the process environment: no " marker " marker admits it"
                } else if (hit) {
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
                V\ *) echo "DENY-LISTED in cell code: $f:${row#V }"; fail=1 ;;
            esac
        done <<< "$out"
    done <<<"$listing"
done

if [ "$files" -eq 0 ]; then
    echo "DENY-LIST SCOPE ERROR: no .rs files found under: ${DIRS[*]}"
    exit 1
fi
# The scope line is printed on both verdicts: what was scanned is part of
# the evidence either way.
scope="${#DIRS[@]} crates, $files files, $lines lines scanned, $stripped test-only lines stripped, $allowed allowed sites"
if [ "$fail" -ne 0 ]; then
    echo "cell deny-list FAILED ($scope)"
    echo "Cell code must not block, lock, spawn, or read ambient time/randomness (L1/L6/L7)."
    echo "Sanctioned control-plane sites carry '$MARKER: <reason>' on the line or the one above;"
    echo "the process environment has no sanctioned site (ADR-0209 D4)."
    exit 1
fi
echo "cell deny-list OK ($scope)"
cell_crate_exclusions
