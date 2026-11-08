#!/usr/bin/env bash
# Reply-outcome gate (ADR-0190 D3, D4, A2): no production site of
# crates/inf-server/src decides a reply's outcome from its bytes, and none
# names a fabric reply tag outside the functions that set and read it. A
# reply carries its outcome beside its bytes — the writer sets it where it
# writes an error line at the reply's start, the fabric carries it as a tag,
# and every consumer reads `outcome()` or `is_error()`.
#
# The compiler cannot forbid reading a byte of a `Vec<u8>`, so this gate
# refuses the shapes that do (ADR-0190 D4):
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
# The fabric's reply tag meets a reply's bytes in two functions, one per
# direction (ADR-0190 D3): `wire_reply` sets it from the reply's outcome and
# `owned_outcome` reads it back into one. A reply tag (R1, R2) outside the
# functions ROWS lists is red, another name for its enum (R3) is red
# anywhere, and no marker sanctions either (ADR-0190 A2):
#   - R1, a path through `Outcome` to anything but a variant NONREPLY lists:
#     `Outcome::Bytes`, `Outcome::ErrorReply`, a turbofish, a glob or a
#     group, a macro paste, a path split after `::`, an associated function.
#     A new variant is a reply tag until it is listed there.
#   - R2, a path through `ReplyOutcome` to anything: an outcome chosen by
#     hand where a tag is paired. A consumer reads `is_error()` instead.
#   - R3, another name for either enum: a rename (`E as`), an alias (`type
#     X = E`), an impl for it (`for E`, on the impl's line or rustfmt's
#     continuation), a qualified path (`<E>::`, its `<` after neither an
#     identifier nor `::`), and a macro fragment pasted before a reply
#     variant's name (FRAGMENT) or before another fragment.
# A token is a whole identifier, read with the spaces around `::` dropped
# and `r#name` as `name`. Three forms are red that a writer may not expect:
# a string literal is code (rephrase the message), an inline `#[cfg(test)]`
# item outside a test module is production (move it into one), and a type
# of inf-server's own named `Outcome` reads as the fabric's (name it
# otherwise). A row allows its tokens from its function's declaration — at
# column 0: an optional visibility, `fn`, the name whole, then `<` or `(` —
# to the next line that begins with `}`, rustfmt's close of a module-level
# item; an indented function gets no allowance. A row whose file is scanned
# is red when its function is not found or its range names none of its
# tokens; a row whose file is not scanned is named on the OK line.
#
# Scope: the production code of crates/inf-server/src — test-only modules
# stripped by strip-test-modules.awk and the files its report names as
# test-only (`modfile`) excluded, comments blanked by
# rust-strip-comments.awk. A missing directory or an empty file set is a
# failure. `reply-outcome-allow: <reason>` on the line or the line above
# sanctions a byte-shape site and is listed; a bare marker fails, and so
# does a marker that guards no hit. The OK line discloses what was scanned.
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

# One list (ADR-0190 D4, A2). D4's shapes: the ten RESP tags, one bracket
# class (`-` first is literal), and the shapes over it. A2's tokens, read on
# a line padded with one space at each end: R1 is the enum's path, judged
# against NONREPLY by the scan; R2 and R3 are patterns.
TAG='[-+!:$_*>%|]'
PATTERN="Some[(]&b'$TAG'|Some[(]b'$TAG'|[[]b'$TAG'"
PATTERN="$PATTERN|==[[:space:]]*b'$TAG'|!=[[:space:]]*b'$TAG'"
PATTERN="$PATTERN|b'$TAG'[[:space:]]*==|b'$TAG'[[:space:]]*!="
PATTERN="$PATTERN|starts_with[(]b\"$TAG|strip_prefix[(]b\"$TAG"
PATTERN="$PATTERN|b\"-[A-Z]|push[(]b'-'[)]|received[(]"
NONREPLY='Ok|Int|Nil|Bool|Err'
FRAGMENT='Bytes|ErrorReply|Success|Error'
W='[^A-Za-z0-9_]'
ID='[A-Za-z_][A-Za-z0-9_]*'
ENUM="(::)?($ID::)*(Outcome|ReplyOutcome)"
R1="${W}Outcome::"
R2="${W}ReplyOutcome::"
R3="${W}(Outcome|ReplyOutcome)(<[^<>]*>)?[[:space:]]+as$W"
R3="$R3|${W}type[[:space:]][^=]*=[[:space:]]*$ENUM$W"
R3="$R3|${W}for[[:space:]]+$ENUM$W"
R3="$R3|[^A-Za-z0-9_:]<[[:space:]]*$ENUM(<[^<>]*>)?[[:space:]]*>::"
R3="$R3|[\$]$ID::(($FRAGMENT)$W|[\$])"

# The functions that set and read a reply's tag: the file under $DIR, the
# function, the tokens it may name and the reason the OK line prints. A row
# is a design site, not a deviation, so it has no expiry.
IFS= read -r -d '' ROWS <<'EOF' || true
plane/fabric.rs|wire_reply|R1 R2|the owner side's one tag, set from the reply's outcome; the canary's arm
plane/fabric.rs|owned_outcome|R1 R2|the origin's one decode: the variant read, the outcome set from it
plane/scatter.rs|stages|R2|the staging gate compares the reply's outcome
EOF

if [ ! -d "$DIR" ]; then
    echo "REPLY-OUTCOME SCOPE ERROR: $DIR is missing — an absent scope is a failure"
    exit 1
fi

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "reply-outcome: mktemp failed" >&2; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

# row_reason <file> <function>: the reason ROWS gives the row.
row_reason() {
    local file name tokens why
    while IFS='|' read -r file name tokens why; do
        if [ "$file" = "$1" ] && [ "$name" = "$2" ]; then
            echo "$why"
        fi
    done <<<"$ROWS"
}

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
fail_d4=0
fail_a2=0
files=0
lines=0
modfiles=0
allowed=0
judged=0
judged_lines=0
scanned='|'
while IFS= read -r f; do
    case "$testfiles" in *"
$f"*)
        modfiles=$((modfiles + 1))
        continue
        ;;
    esac
    if awk -v mode=report -f "$STRIP" "$f" | grep -Fqx 'unterminated 1'; then
        echo "REPLY-OUTCOME SCOPE ERROR: $f — a test-only module never closed at its own indent"
        fail=1
        continue
    fi
    files=$((files + 1))
    lines=$((lines + $(wc -l < "$f")))
    rel=${f#"$DIR"/}
    scanned="$scanned$rel|"
    rows=""
    while IFS='|' read -r file name tokens why; do
        [ "$file" = "$rel" ] || continue
        rows="$rows${rows:+,}$name:${tokens// /}"
    done <<<"$ROWS"
    awk -f "$STRIP" "$f" > "$work/raw"
    awk -f "$NOCOMMENT" "$work/raw" > "$work/code"
    # Two aligned streams, line numbers preserved: a hit is read in the
    # code stream (comments blanked), its marker in the raw one.
    out=$(awk -v pat="$PATTERN" -v marker="$MARKER" -v nonreply="$NONREPLY" \
        -v r1="$R1" -v r2="$R2" -v r3="$R3" -v rows="$rows" '
        function reason(s) { sub("^.*" marker ":[[:space:]]*", "", s); return s }
        # The line as the tokens read it: spaces around `::` dropped, one
        # space at each end, `r#name` read as `name`.
        function plain(s,   out) {
            gsub(/[[:space:]]*::[[:space:]]*/, "::", s)
            s = " " s " "
            out = ""
            while (match(s, /[^A-Za-z0-9_]r#[A-Za-z_]/)) {
                out = out substr(s, 1, RSTART)
                s = substr(s, RSTART + 3)
            }
            return out s
        }
        # R1: the enum path followed by anything but a non-reply variant.
        function tag1(s,   rest) {
            while (match(s, r1)) {
                rest = substr(s, RSTART + RLENGTH)
                if (rest !~ ("^(" nonreply ")[^A-Za-z0-9_]")) { return 1 }
                s = ":" rest
            }
            return 0
        }
        NR == FNR { code[FNR] = $0; next }
        { raw[FNR] = $0; n = FNR }
        END {
            withreason = marker ":[[:space:]]*[^[:space:]]"
            # Each row: every declaration at column 0, to the next line
            # that begins with `}`.
            nrows = split(rows, row, ",")
            for (k = 1; k <= nrows; k++) {
                split(row[k], field, ":")
                name[k] = field[1]; tokens[k] = field[2]; found[k] = 0; count[k] = 0; at[k] = ""
                decl = "^(pub([(][^)]*[)])?[[:space:]]+)?fn[[:space:]]+" name[k] "[<(]"
                for (i = 1; i <= n; i++) {
                    if (code[i] !~ decl) { continue }
                    found[k] = 1
                    for (j = i; j <= n; j++) {
                        inrow[j] = k
                        if (j > i && substr(code[j], 1, 1) == "}") { break }
                    }
                    i = j
                }
            }
            for (i = 1; i <= n; i++) {
                hit[i] = (code[i] ~ pat); mark[i] = (raw[i] ~ marker); tagv[i] = 0; namev[i] = 0
                if (index(code[i], "Outcome") == 0 && index(code[i], "$") == 0) { continue }
                s = plain(code[i])
                t1 = tag1(s); t2 = (s ~ r2); namev[i] = (s ~ r3)
                if (!t1 && !t2) { continue }
                k = inrow[i]
                if (k != "" && (!t1 || index(tokens[k], "R1")) && (!t2 || index(tokens[k], "R2"))) {
                    count[k]++
                    at[k] = at[k] (at[k] == "" ? "" : ", ") i
                } else {
                    tagv[i] = 1
                }
            }
            for (i = 1; i <= n; i++) { any[i] = (hit[i] || tagv[i] || namev[i]) }
            for (i = 1; i <= n; i++) {
                above = (i > 1 && mark[i - 1] && !any[i - 1])
                if (hit[i]) {
                    if (mark[i] && raw[i] ~ withreason) { print "A " i ": " reason(raw[i]) }
                    else if (above && raw[i - 1] ~ withreason) { print "A " i ": " reason(raw[i - 1]) }
                    else if (mark[i] || above) { print "V " i ": " raw[i] "   <-- " marker " marker without a reason" }
                    else { print "V " i ": " raw[i] }
                }
                suffix = (mark[i] || above) ? "   <-- a marker does not sanction a reply tag" : ""
                if (tagv[i]) { print "T " i ": " raw[i] suffix }
                if (namev[i]) { print "N " i ": " raw[i] suffix }
                if (mark[i] && !any[i] && !(i < n && any[i + 1])) { print "S " i ": " raw[i] }
            }
            for (k = 1; k <= nrows; k++) {
                if (!found[k]) { print "M " name[k] }
                else if (count[k] == 0) { print "X " name[k] }
                else { print "R " name[k] " " count[k] " " at[k] }
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
                fail_d4=1
                ;;
            S\ *)
                echo "REPLY-OUTCOME violation (a marker guarding no hit): $f:${row#S }"
                fail=1
                fail_d4=1
                ;;
            T\ *)
                echo "REPLY-OUTCOME violation (a reply tag outside its functions): $f:${row#T }"
                fail=1
                fail_a2=1
                ;;
            N\ *)
                echo "REPLY-OUTCOME violation (another name for a reply tag's enum): $f:${row#N }"
                fail=1
                fail_a2=1
                ;;
            R\ *)
                set -- ${row#R }
                name=$1 count=$2
                shift 2
                judged=$((judged + 1))
                judged_lines=$((judged_lines + count))
                [ "$count" -eq 1 ] && noun=line || noun=lines
                echo "  row $f:$name ($noun $*): $(row_reason "$rel" "$name")"
                ;;
            M\ *)
                judged=$((judged + 1))
                echo "REPLY-OUTCOME SCOPE ERROR: $f:${row#M } is not found: move the row with its function"
                fail=1
                fail_a2=1
                ;;
            X\ *)
                judged=$((judged + 1))
                echo "REPLY-OUTCOME violation (a row with no reply tag): $f:${row#X }: remove the row or restore its tag"
                fail=1
                fail_a2=1
                ;;
        esac
    done <<< "$out"
done < <(find "$DIR" -name '*.rs' | sort)

if [ "$files" -eq 0 ]; then
    echo "REPLY-OUTCOME SCOPE ERROR: no production .rs files under $DIR"
    exit 1
fi
unjudged=""
while IFS='|' read -r file name tokens why; do
    [ -n "$file" ] || continue
    case "$scanned" in *"|$file|"*) continue ;; esac
    unjudged="$unjudged${unjudged:+, }$DIR/$file:$name"
done <<<"$ROWS"
scope="$files files, $lines lines scanned under $DIR, $modfiles test-only files excluded, $allowed allowed sites"
scope="$scope, $judged rows judged allowing $judged_lines lines; rows not judged, file not in the scan: ${unjudged:-none}"
if [ "$fail" -ne 0 ]; then
    echo "reply-outcome gate FAILED ($scope)"
    if [ "$fail_d4" -ne 0 ]; then
        echo "A reply's outcome is read through outcome() or is_error(), never from its bytes"
        echo "(ADR-0190 D4); a sanctioned site carries 'reply-outcome-allow: <reason>'."
    fi
    if [ "$fail_a2" -ne 0 ]; then
        echo "Tag a reply with wire_reply(bytes, reply.outcome()); read an outcome with is_error()."
        echo "A new site that names a reply tag changes ADR-0190 D3 and A2; no marker sanctions it."
    fi
    exit 1
fi
echo "reply-outcome gate OK ($scope)"
