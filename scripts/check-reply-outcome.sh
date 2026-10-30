#!/usr/bin/env bash
# Reply-outcome gate (ADR-0190 D4): no production site of
# crates/inf-server/src decides a reply's outcome from its bytes. A reply
# carries its outcome beside its bytes — the writer sets it where it writes
# an error line at the reply's start, the fabric carries it as a tag, and
# every consumer reads `outcome()` or `is_error()`. The compiler cannot
# forbid reading a byte of a `Vec<u8>`, so this gate refuses the shapes
# that do:
#   - for every RESP tag T (- + ! : $ _ * > % |): `Some(&b'T'`,
#     `Some(b'T'`, `[b'T'` (an or-pattern is a hit), `== b'T'`, `!= b'T'`,
#     `b'T' ==`, `b'T' !=`, `starts_with(b"T`, `strip_prefix(b"T`;
#   - an error line written outside the writer: a `b"-<capital>` literal
#     and `push(b'-')`;
#   - `received(` — the one constructor of an owned reply that takes its
#     outcome from a caller — outside the decode site that reads it from
#     the wire's variant.
# A `match` on a reply byte with a tag arm (`b'-' =>`) reads as a sign arm
# and is review-only: the types leave no site that needs the byte.
#
# Scope: the production code of crates/inf-server/src — test-only modules
# stripped by strip-test-modules.awk and the files its report names as
# test-only (`modfile`) excluded, comments blanked by
# rust-strip-comments.awk. A missing directory or an empty file set is a
# failure. `reply-outcome-allow: <reason>` on the line or the line above
# sanctions a site and is listed; a bare marker fails, and so does a marker
# that guards no hit. The OK line discloses what was scanned.
#
# Written for bash 3.2 and POSIX awk, with grep only for fixed strings: the
# CI matrix includes macOS.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"
NOCOMMENT="$SCRIPT_DIR/rust-strip-comments.awk"
DIR=crates/inf-server/src
MARKER='reply-outcome-allow'

# The ten RESP tags, one bracket class (`-` first is literal), and the
# shapes over it: one list (ADR-0190 D4).
TAG='[-+!:$_*>%|]'
PATTERN="Some[(]&b'$TAG'|Some[(]b'$TAG'|[[]b'$TAG'"
PATTERN="$PATTERN|==[[:space:]]*b'$TAG'|!=[[:space:]]*b'$TAG'"
PATTERN="$PATTERN|b'$TAG'[[:space:]]*==|b'$TAG'[[:space:]]*!="
PATTERN="$PATTERN|starts_with[(]b\"$TAG|strip_prefix[(]b\"$TAG"
PATTERN="$PATTERN|b\"-[A-Z]|push[(]b'-'[)]|received[(]"

if [ ! -d "$DIR" ]; then
    echo "REPLY-OUTCOME SCOPE ERROR: $DIR is missing — an absent scope is a failure"
    exit 1
fi

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "reply-outcome: mktemp failed" >&2; exit 2; }
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
allowed=0
while IFS= read -r f; do
    case "$testfiles" in *"
$f"*)
        modfiles=$((modfiles + 1))
        continue
        ;;
    esac
    if awk -v mode=report -f "$STRIP" "$f" | grep -q '^unterminated'; then
        echo "REPLY-OUTCOME SCOPE ERROR: $f — a test-only module never closed at its own indent"
        fail=1
        continue
    fi
    files=$((files + 1))
    lines=$((lines + $(wc -l < "$f")))
    awk -f "$STRIP" "$f" > "$work/raw"
    awk -f "$NOCOMMENT" "$work/raw" > "$work/code"
    # Two aligned streams, line numbers preserved: a hit is read in the
    # code stream (comments blanked), its marker in the raw one.
    out=$(awk -v pat="$PATTERN" -v marker="$MARKER" '
        function reason(s) { sub("^.*" marker ":[[:space:]]*", "", s); return s }
        NR == FNR { code[FNR] = $0; next }
        { raw[FNR] = $0; n = FNR }
        END {
            withreason = marker ":[[:space:]]*[^[:space:]]"
            for (i = 1; i <= n; i++) { hit[i] = (code[i] ~ pat); mark[i] = (raw[i] ~ marker) }
            for (i = 1; i <= n; i++) {
                above = (i > 1 && mark[i - 1] && !hit[i - 1])
                if (hit[i]) {
                    if (mark[i] && raw[i] ~ withreason) { print "A " i ": " reason(raw[i]) }
                    else if (above && raw[i - 1] ~ withreason) { print "A " i ": " reason(raw[i - 1]) }
                    else if (mark[i] || above) { print "V " i ": " raw[i] "   <-- " marker " marker without a reason" }
                    else { print "V " i ": " raw[i] }
                }
                if (mark[i] && !hit[i] && !(i < n && hit[i + 1])) { print "S " i ": " raw[i] }
            }
        }
    ' "$work/code" "$work/raw")
    [ -n "$out" ] || continue
    while IFS= read -r row; do
        case "$row" in
            A\ *)
                allowed=$((allowed + 1))
                echo "  allowed $f:${row#A }"
                ;;
            V\ *)
                echo "REPLY-OUTCOME violation (an outcome read from a reply's bytes): $f:${row#V }"
                fail=1
                ;;
            S\ *)
                echo "REPLY-OUTCOME violation (a marker guarding no hit): $f:${row#S }"
                fail=1
                ;;
        esac
    done <<< "$out"
done < <(find "$DIR" -name '*.rs' | sort)

if [ "$files" -eq 0 ]; then
    echo "REPLY-OUTCOME SCOPE ERROR: no production .rs files under $DIR"
    exit 1
fi
scope="$files files, $lines lines scanned under $DIR, $modfiles test-only files excluded, $allowed allowed sites"
if [ "$fail" -ne 0 ]; then
    echo "reply-outcome gate FAILED ($scope)"
    echo "A reply's outcome is read through outcome() or is_error(), never from its bytes"
    echo "(ADR-0190 D4); a sanctioned site carries 'reply-outcome-allow: <reason>'."
    exit 1
fi
echo "reply-outcome gate OK ($scope)"
