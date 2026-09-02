#!/usr/bin/env bash
# Release-assert inventory gate (ADR-0107 D2; review of 2026-08-30, Theme 4
# — "release asserts justified by a claim about the caller", the
# panic-policy grep's third limit).
#
# Every release-panicking construct in production code of the cell-resident
# crate set — `assert!`/`assert_eq!`/`assert_ne!`, `.expect(`, `panic!`,
# `unreachable!` (never `debug_*`) — is a site in
# `docs/release-assert-inventory.tsv`, classified by the story that read it:
#
#   I   an invariant on the callee's own state (violation = corrupt code)
#   C   a claim about a caller — the justification names the check that
#       enforces it at every call site (`file.rs:function`)
#   F   a fail-stop on an operating condition the policy sanctions
#       (§8.4 fsync failure, the control thread's death, …) — cites it
#   U   unaudited (a crate outside the audited set); counted, disclosed
#
# The site identity is `file <TAB> kind <TAB> message` (the first string
# literal of the call, or the condition text when it has none) with a
# count — never a line number, so edits move nothing, and a changed
# message or a new site is a deliberate act: the gate is red until the
# inventory names it. A row the tree no longer has is red too (a stale
# inventory is the P1 shape). A `C` row without a proof pointer is red.
#
# A proof pointer RESOLVES (ADR-0107 D2, first amendment — batch 12):
# every `path/from/root.rs:Symbol` a `C` row cites must name a definition
# in that file's production code (rust-symbol-defined.awk over the
# stripped file: a free item, `Type::method` inside an `impl`/`trait`/
# `mod` block of that name, a const/static). A bare file name, a line
# number, a file that does not exist, or a symbol the file no longer
# defines is red — a renamed enforcing function fails the gate instead
# of waiting for a reader to notice (the review's L20 row).
#
# Test modules are stripped structurally (strip-test-modules.awk); the
# scanned set is scripts/cell-crates.sh (shared with the deny-list and
# the panic-policy grep); scope is asserted and disclosed on both verdicts.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
# shellcheck source=cell-crates.sh
. "$SCRIPT_DIR/cell-crates.sh"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"
CENSUS="$SCRIPT_DIR/release-assert-census.awk"
RESOLVE="$SCRIPT_DIR/rust-symbol-defined.awk"
INVENTORY=docs/release-assert-inventory.tsv
# A proof pointer: a path (with at least one `/`) to a .rs file, a colon,
# and a symbol path — `crates/x/src/a.rs:Type::method`. A line number
# after the colon is a citation, never a proof.
PTR_RE='[A-Za-z0-9_./-]+\.rs:[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*'
LINE_RE='[A-Za-z0-9_./-]+\.rs:[0-9]+'

DIRS=()
while IFS= read -r dir; do DIRS+=("$dir"); done < <(cell_crate_dirs)

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "release-asserts: mktemp failed" >&2; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

fail=0
files=0
lines=0
stripped=0
: > "$work/sites"
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
            echo "RELEASE-ASSERT SCOPE ERROR: $f — a test-only module never closed at its own indent"
            fail=1
            continue
        fi
        files=$((files + 1))
        lines=$((lines + $(wc -l < "$f")))
        stripped=$((stripped + $(printf '%s\n' "$report" | awk '$1 == "stripped" { print $2 }')))
        awk -f "$STRIP" "$f" | awk -f "$CENSUS" | awk -F'\t' -v f="$f" '{ print f "\t" $1 "\t" $2 }' >> "$work/sites"
    done < <(find "$dir" -name '*.rs' | sort)
done
if [ "$files" -eq 0 ]; then
    echo "RELEASE-ASSERT SCOPE ERROR: no .rs files found under: ${DIRS[*]}"
    exit 1
fi
# identity -> count
sort "$work/sites" | uniq -c | sed -E 's/^ *([0-9]+) /\1\t/' > "$work/census"
sites=$(awk -F'\t' '{ s += $1 } END { print s + 0 }' "$work/census")

if [ ! -f "$INVENTORY" ]; then
    echo "RELEASE-ASSERT SCOPE ERROR: $INVENTORY is missing ($sites sites in $files files need classifying)"
    exit 1
fi
# inventory rows: class<TAB>count<TAB>file<TAB>kind<TAB>message[<TAB>justification]
grep -v '^#' "$INVENTORY" | grep -v '^[[:space:]]*$' > "$work/inventory" || true
rows=$(wc -l < "$work/inventory" | tr -d ' ')
if [ "$rows" -eq 0 ]; then
    echo "RELEASE-ASSERT SCOPE ERROR: $INVENTORY has no rows ($sites sites in the tree)"
    exit 1
fi

# Shape + class rules.
: > "$work/pointers"
while IFS=$'\t' read -r class count file kind message justification; do
    case "$class" in
        I|F|U) ;;
        C)
            if [ -z "${justification:-}" ] || ! printf '%s' "$justification" | grep -Eq "$PTR_RE"; then
                echo "RELEASE-ASSERT violation: C row without a proof pointer (path/from/root.rs:Symbol): $file $kind \"$message\""
                fail=1
            fi
            if printf '%s' "${justification:-}" | grep -Eq "$LINE_RE"; then
                cite=$(printf '%s' "$justification" | grep -Eo "$LINE_RE" | head -1)
                echo "RELEASE-ASSERT violation: a line number is not a proof pointer — '$cite' in the C row $file $kind \"$message\" (name the function)"
                fail=1
            fi
            printf '%s' "${justification:-}" | grep -Eo "$PTR_RE" | while IFS= read -r ptr; do
                printf '%s\t%s\t%s\t%s\n' "$ptr" "$file" "$kind" "$message"
            done >> "$work/pointers"
            ;;
        *)
            echo "RELEASE-ASSERT violation: unknown class '$class' for $file $kind \"$message\" (I, C, F or U)"
            fail=1
            ;;
    esac
    case "$count" in ''|*[!0-9]*) echo "RELEASE-ASSERT violation: bad count '$count' for $file $kind \"$message\""; fail=1 ;; esac
done < "$work/inventory"

# Proof pointers resolve: one resolution per distinct pointer, against
# the production code of the file it names (test modules stripped, so a
# symbol that exists only under #[cfg(test)] is no proof).
sort -t "$(printf '\t')" -k1,1 -u "$work/pointers" > "$work/pointers.u"
pointers=0
while IFS=$'\t' read -r ptr file kind message; do
    path=${ptr%%:*}
    sym=${ptr#*:}
    pointers=$((pointers + 1))
    case "$path" in
        */*) ;;
        *)
            echo "RELEASE-ASSERT violation: proof pointer '$ptr' is a bare file name — write the path from the workspace root (row: $file $kind \"$message\")"
            fail=1
            continue
            ;;
    esac
    if [ ! -f "$path" ]; then
        echo "RELEASE-ASSERT violation: proof pointer '$ptr' names a file that does not exist (row: $file $kind \"$message\")"
        fail=1
        continue
    fi
    if ! awk -f "$STRIP" "$path" | awk -v sym="$sym" -f "$RESOLVE" > /dev/null; then
        echo "RELEASE-ASSERT violation: proof pointer '$ptr' does not resolve — $path defines no '$sym' in production code (renamed, moved, or test-only?) (row: $file $kind \"$message\")"
        fail=1
    fi
done < "$work/pointers.u"

# Census vs inventory, by identity (file, kind, message) and count.
awk -F'\t' '{ print $2 "\t" $3 "\t" $4 "\t" $1 }' "$work/census" | sort > "$work/census.keyed"
awk -F'\t' '{ print $3 "\t" $4 "\t" $5 "\t" $2 }' "$work/inventory" | sort > "$work/inventory.keyed"
# new or changed sites
while IFS=$'\t' read -r file kind message count; do
    row=$(awk -F'\t' -v f="$file" -v k="$kind" -v m="$message" '$1 == f && $2 == k && $3 == m { print $4; exit }' "$work/inventory.keyed")
    if [ -z "$row" ]; then
        echo "RELEASE-ASSERT violation: unclassified site — $file $kind \"$message\" (×$count): add an inventory row (I/C/F) with its justification"
        fail=1
    elif [ "$row" != "$count" ]; then
        echo "RELEASE-ASSERT violation: $file $kind \"$message\" occurs ×$count, the inventory says ×$row — classify the new site (or retire the vanished one)"
        fail=1
    fi
done < "$work/census.keyed"
# stale rows
while IFS=$'\t' read -r file kind message count; do
    if ! awk -F'\t' -v f="$file" -v k="$kind" -v m="$message" '$1 == f && $2 == k && $3 == m { found = 1 } END { exit !found }' "$work/census.keyed"; then
        echo "RELEASE-ASSERT violation: stale inventory row — $file $kind \"$message\" no longer exists in the tree"
        fail=1
    fi
done < "$work/inventory.keyed"

classes=$(awk -F'\t' '{ n[$1] += $2 } END { printf "I=%d C=%d F=%d U=%d", n["I"], n["C"], n["F"], n["U"] }' "$work/inventory")
scope="${#DIRS[@]} crates, $files files, $lines lines scanned, $stripped test-only lines stripped, $sites release-panic sites in $(wc -l < "$work/census" | tr -d ' ') identities; inventory $rows rows ($classes); $pointers proof pointers resolved"
if [ "$fail" -ne 0 ]; then
    echo "release-assert inventory FAILED ($scope)"
    echo "Every release assert!/expect()/panic!/unreachable! in cell code is a classified inventory row"
    echo "(docs/release-assert-inventory.tsv — ADR-0107 D2): I invariant, C caller-claim with the enforcing"
    echo "check named, F sanctioned fail-stop. An operating condition is a typed error, never a row."
    exit 1
fi
echo "release-assert inventory OK ($scope)"
cell_crate_exclusions
