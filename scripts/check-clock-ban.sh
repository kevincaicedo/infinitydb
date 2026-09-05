#!/usr/bin/env bash
# Type-resolved ambient-clock ban (review 2026-08-30 F-L18-05; ADR-0106
# first amendment). L7: cell code reads time only through the injected
# `inf_foundation::time::Clock`. check-cell-denylist.sh greps for the
# literal `Instant::now` — a renamed or aliased import (`Clock::now()`),
# `<Instant>::now()`, `elapsed()`, `UNIX_EPOCH.elapsed()`, `_rdtsc` and
# `libc::clock_gettime` all defeat it (three of them planted on the real
# tree scored zero hits; clippy reported every one). The
# enforcement is clippy's `disallowed-methods` in the workspace
# clippy.toml (resolved on the type, immune to spelling); this gate
# proves that enforcement is in force and honest:
#   1. the config carries every clock entry (a deleted entry is red);
#   2. no crate directory carries its own clippy.toml — a nearer config
#      REPLACES the workspace one silently (clippy walks up and stops);
#   3. in cell-crate production code (test modules stripped), no crate-
#      or file-level allow silences the lint, and every per-site allow
#      names a reason — each is listed in the scope line;
#   4. a planted-bypass probe (scripts/clock-ban-probe) is compiled under
#      target/ with the real config: every PLANT line must be reported
#      with its path, no CONTROL line may be, the ALLOWED shape must not.
# Portable bash 3.2. Step 4 runs cargo; a fixture root (INF_CHECK_ROOT)
# may set INF_CLOCK_BAN_PROBE=off and the scope line says so.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
# shellcheck source=cell-crates.sh
. "$SCRIPT_DIR/cell-crates.sh"
STRIP="$SCRIPT_DIR/strip-test-modules.awk"
PROBE_SRC="$SCRIPT_DIR/clock-ban-probe"

fail=0
# ---- 1. the config ------------------------------------------------------
ENTRIES="std::time::Instant::now std::time::Instant::elapsed std::time::SystemTime::now std::time::SystemTime::elapsed libc::clock_gettime libc::gettimeofday libc::time core::arch::x86_64::_rdtsc core::arch::x86_64::_rdtscp"
entries=0
if [ ! -f clippy.toml ]; then
    echo "CLOCK-BAN violation: clippy.toml missing at $(pwd) — the ban has no config"
    fail=1
else
    for p in $ENTRIES; do
        if grep -q "path = \"$p\"" clippy.toml; then
            entries=$((entries + 1))
        else
            echo "CLOCK-BAN violation: clippy.toml lacks disallowed-methods entry \"$p\""
            fail=1
        fi
    done
fi

# ---- 2. no shadow config ------------------------------------------------
shadows=0
for cfg in crates/*/clippy.toml crates/*/.clippy.toml bins/*/clippy.toml bins/*/.clippy.toml tests/*/clippy.toml tests/*/.clippy.toml; do
    [ -e "$cfg" ] || continue
    echo "CLOCK-BAN violation: $cfg shadows the workspace clippy.toml (clippy stops at the nearest config — every ban vanishes for that crate)"
    shadows=$((shadows + 1))
    fail=1
done

# ---- 3. allows in cell production code ----------------------------------
DIRS=()
while IFS= read -r dir; do DIRS+=("$dir"); done < <(cell_crate_dirs)
files=0
allowed=0
# rustfmt splits a long attribute over lines, so attributes are joined
# from `#[`/`#![` to the closing `)]` before classification. A crate- or
# file-level allow of the lint, its group (`style`), `clippy::all` or
# `warnings`; or a per-site allow/expect of a group — never sanctioned in
# cell code. A per-site allow of the lint must carry a reason.
for dir in "${DIRS[@]}"; do
    while IFS= read -r f; do
        files=$((files + 1))
        report=$(awk -v mode=report -f "$STRIP" "$f")
        if printf '%s\n' "$report" | grep -q '^unterminated'; then
            echo "CLOCK-BAN SCOPE ERROR: $f — a test-only module never closed at its own indent"
            fail=1
            continue
        fi
        out=$(awk -f "$STRIP" "$f" | awk '
            function classify(text, line,   inner, r) {
                inner = (text ~ /^[[:space:]]*#!\[/)
                if (text !~ /\[(allow|expect)\(/) return
                if (inner && text ~ /(warnings|clippy::all|clippy::style|clippy::disallowed_methods|clippy::disallowed_types)/) { print "B " line ": " text; return }
                if (text ~ /(warnings|clippy::all|clippy::style)/) { print "G " line ": " text; return }
                if (text ~ /clippy::disallowed_methods/) {
                    if (text ~ /reason = "[^"]+"/) { r = text; sub(/^.*reason = "/, "", r); sub(/".*$/, "", r); print "A " line ": " r }
                    else { print "R " line ": " text }
                }
            }
            acc != "" { acc = acc " " $0; if ($0 ~ /\)\]/) { classify(acc, start); acc = "" } ; next }
            /^[[:space:]]*#!?\[(allow|expect)\(/ {
                if ($0 ~ /\)\]/) { classify($0, NR) } else { acc = $0; start = NR }
            }')
        [ -n "$out" ] || continue
        while IFS= read -r row; do
            case "$row" in
                A\ *) allowed=$((allowed + 1)); echo "  allowed $f:${row#A }" ;;
                B\ *) echo "CLOCK-BAN violation: $f:${row#B }   <-- a crate/file-level allow silences the ban for the whole file"; fail=1 ;;
                G\ *) echo "CLOCK-BAN violation: $f:${row#G }   <-- a lint-group allow silences the ban"; fail=1 ;;
                R\ *) echo "CLOCK-BAN violation: $f:${row#R }   <-- allow without a reason"; fail=1 ;;
            esac
        done <<< "$out"
    done < <(find "$dir" -name '*.rs' | sort)
done
if [ "$files" -eq 0 ]; then
    echo "CLOCK-BAN SCOPE ERROR: no .rs files found under: ${DIRS[*]}"
    exit 1
fi

# ---- 4. the planted-bypass probe ----------------------------------------
probe="skipped (fixture mode)"
if [ -n "${INF_CHECK_ROOT:-}" ] && [ "${INF_CLOCK_BAN_PROBE:-on}" = off ]; then
    :
else
    if [ ! -f "$PROBE_SRC/src/lib.rs" ]; then
        echo "CLOCK-BAN SCOPE ERROR: probe crate missing at $PROBE_SRC"
        exit 1
    fi
    mkdir -p target
    work=$(mktemp -d "$(pwd)/target/clock-ban-probe.XXXXXX")
    [ -n "$work" ] && [ -d "$work" ] || { echo "CLOCK-BAN SCOPE ERROR: mktemp failed"; exit 2; }
    trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT
    cp -R "$PROBE_SRC/." "$work/probe"
    # The workspace config must be the one found: no CLIPPY_CONF_DIR, and
    # the copy sits under this root so the walk-up reaches ./clippy.toml.
    diag="$work/diag"
    (cd "$work/probe" && env -u CLIPPY_CONF_DIR cargo clippy --quiet --target-dir "$work/target" --message-format=short -- -W clippy::disallowed-methods -W clippy::disallowed-types >"$diag" 2>&1) || true
    plants=0
    controls=0
    # expected: line -> path, from the source markers
    while IFS=$'\t' read -r line kind path; do
        case "$kind" in
            PLANT|PLANT-TYPE)
                plants=$((plants + 1))
                if ! grep -q "src/lib.rs:$line:[0-9]*: warning: use of a disallowed \(method\|type\) \`$path\`" "$diag"; then
                    echo "CLOCK-BAN violation: probe line $line ($path) was NOT reported — the ban does not resolve this spelling"
                    fail=1
                fi ;;
            CONTROL|ALLOWED)
                controls=$((controls + 1))
                if grep -q "src/lib.rs:$line:" "$diag"; then
                    echo "CLOCK-BAN violation: probe line $line ($kind) WAS reported — the ban is wider than declared"
                    fail=1
                fi ;;
        esac
    done < <(awk '
        /\/\/ PLANT-TYPE / { sub(/^.*\/\/ PLANT-TYPE /, ""); print NR "\tPLANT-TYPE\t" $1; next }
        /\/\/ PLANT /      { sub(/^.*\/\/ PLANT /, "");      print NR "\tPLANT\t" $1; next }
        /\/\/ CONTROL$/    { print NR "\tCONTROL\t-"; next }
        /\/\/ ALLOWED$/    { print NR "\tALLOWED\t-"; next }' "$work/probe/src/lib.rs")
    if [ "$plants" -lt 10 ]; then
        echo "CLOCK-BAN SCOPE ERROR: probe carries $plants planted lines (expected ≥ 10) — the fixture was edited down"
        fail=1
    fi
    if grep -q "^error" "$diag"; then
        echo "CLOCK-BAN SCOPE ERROR: the probe did not compile:"
        sed 's/^/    | /' "$diag"
        fail=1
    fi
    probe="$plants planted bypasses red, $controls controls green"
fi

scope="config $entries/9 entries, $shadows shadow configs, ${#DIRS[@]} cell crates / $files files scanned, $allowed allowed sites in cell code; probe: $probe"
if [ "$fail" -ne 0 ]; then
    echo "clock ban FAILED ($scope)"
    echo "Cell code reads time through inf_foundation::time::Clock only (L7). A sanctioned site carries"
    echo "#[allow(clippy::disallowed_methods, reason = \"…\")] on its statement or fn — never on the crate or file."
    exit 1
fi
echo "clock ban OK ($scope)"
cell_crate_exclusions
