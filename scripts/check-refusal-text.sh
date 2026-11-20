#!/usr/bin/env bash
# Refusal-text gate (ADR-0172 D2): a reservation refused before any effect
# answers one of four classes, and `inf_wire::Refusal` renders it from one
# table of class prefixes, suffixes and domain tokens. A server site returns
# the typed value and renders it; no site writes the text. So no production
# line of crates/inf-server/src spells a class suffix:
#   `capacity in use`, `limit reached`, `reservation refused`,
#   `identity space exhausted` (in any letter case).
# The matcher reads the suffixes, never a bare prefix: `ERR`, `TRYAGAIN`
# and Redis's own `OOM command not allowed …` reply are ordinary text.
#
# Scope: the production code of crates/inf-server/src — test-only modules
# stripped by strip-test-modules.awk and the files its report names as
# test-only (`modfile`) excluded, comments blanked by
# rust-strip-comments.awk. A missing directory or an empty file set is a
# failure. No marker sanctions a site. A suffix assembled from pieces
# (`concat!`, a string continuation that splits it) is not seen: that is
# review's. The OK line discloses what was scanned. A script until the
# command table generates the check (L13).
#
# Written for bash 3.2 and POSIX awk: the CI matrix includes macOS.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"
NOCOMMENT="$SCRIPT_DIR/rust-strip-comments.awk"
DIR=crates/inf-server/src
# The four class suffixes, as `Refusal`'s table spells them, lower case.
PATTERN='capacity in use|limit reached|reservation refused|identity space exhausted'

if [ ! -d "$DIR" ]; then
    echo "REFUSAL-TEXT SCOPE ERROR: $DIR is missing — an absent scope is a failure"
    exit 1
fi

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "refusal-text: mktemp failed" >&2; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

testfiles=""
while IFS= read -r f; do
    while read -r key value; do
        [ "$key" = "modfile" ] || continue
        testfiles="$testfiles
$(dirname "$f")/$value.rs
$(dirname "$f")/$value/mod.rs"
    done < <(awk -v mode=report -f "$STRIP" "$f")
done < <(find "$DIR" -name '*.rs' | sort)

fail=0
files=0
lines=0
modfiles=0
while IFS= read -r f; do
    case "$testfiles" in *"
$f"*)
        modfiles=$((modfiles + 1))
        continue
        ;;
    esac
    if awk -v mode=report -f "$STRIP" "$f" | grep -q '^unterminated'; then
        echo "REFUSAL-TEXT SCOPE ERROR: $f — a test-only module never closed at its own indent"
        fail=1
        continue
    fi
    files=$((files + 1))
    lines=$((lines + $(wc -l < "$f")))
    awk -f "$STRIP" "$f" > "$work/raw"
    awk -f "$NOCOMMENT" "$work/raw" > "$work/code"
    # Two aligned streams, line numbers preserved: a hit is read in the
    # code stream (comments blanked) and printed from the raw one.
    out=$(awk -v pat="$PATTERN" '
        NR == FNR { code[FNR] = $0; next }
        { if (tolower(code[FNR]) ~ pat) print FNR ": " $0 }
    ' "$work/code" "$work/raw")
    [ -n "$out" ] || continue
    while IFS= read -r row; do
        echo "REFUSAL-TEXT violation (a refusal class suffix outside inf_wire::Refusal): $f:$row"
        fail=1
    done <<< "$out"
done < <(find "$DIR" -name '*.rs' | sort)

if [ "$files" -eq 0 ]; then
    echo "REFUSAL-TEXT SCOPE ERROR: no production .rs files under $DIR"
    exit 1
fi
scope="$files files, $lines lines scanned under $DIR, $modfiles test-only files excluded"
if [ "$fail" -ne 0 ]; then
    echo "refusal-text gate FAILED ($scope)"
    echo "A refusal's text is inf_wire::Refusal::render's alone: return the typed"
    echo "refusal from the admission step and render it (ADR-0172 D2)."
    exit 1
fi
echo "refusal-text gate OK ($scope)"
