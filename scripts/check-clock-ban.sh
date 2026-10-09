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
#   3. source membership is nonempty; check-lint-scopes.sh owns the one
#      structural allow audit for clocks and filesystem acquisition;
#   4. a planted-bypass probe (scripts/clock-ban-probe) is compiled under
#      target/ with the real config: every PLANT line must be reported
#      with its path, no CONTROL line may be, the ALLOWED shape must not.
# Portable bash 3.2. Step 4 runs cargo; a fixture root (INF_CHECK_ROOT)
# may set INF_CLOCK_BAN_PROBE=off and the scope line says so.
# The two TSC spellings exist only on x86_64 (ADR-0106 D18): when the
# probe compiles for another architecture (probe-target.sh: the target
# CARGO_BUILD_TARGET names, else the host) clippy cannot resolve them and
# the probe's `cfg(x86_64)` plants are not compiled — both are disclosed on
# the scope line, never counted as failures, and the x86_64 leg remains
# authoritative for those two entries.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
# shellcheck source=cell-crates.sh
. "$SCRIPT_DIR/cell-crates.sh"
# shellcheck source=probe-target.sh
. "$SCRIPT_DIR/probe-target.sh"
PROBE_SRC="$SCRIPT_DIR/clock-ban-probe"

fail=0
# ---- 1. the config ------------------------------------------------------
ENTRIES="std::time::Instant::now std::time::Instant::elapsed std::time::SystemTime::now std::time::SystemTime::elapsed libc::clock_gettime libc::gettimeofday libc::time core::arch::x86_64::_rdtsc core::arch::x86_64::__rdtscp"
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

# ---- 3. source membership; the sole allow audit is check-lint-scopes.sh ----
# ADR-0144 I8: all suppression classes/scopes, including clock:, are audited
# together. Do not add a second reason scan here.
dirs=$(cell_crate_dirs)
files=0
# `find`, as every sibling gate: the CI runners ship no ripgrep.
while IFS= read -r dir; do
    n=$(find "$dir" -name '*.rs' | wc -l)
    files=$((files + n))
done <<< "$dirs"
if [ "$files" -eq 0 ]; then
    echo "CLOCK-BAN SCOPE ERROR: cell production scope contains no Rust files"
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
    probe_target || { echo "CLOCK-BAN SCOPE ERROR: the probe's target architecture is unknown"; exit 1; }
    (cd "$work/probe" && env -u CLIPPY_CONF_DIR cargo clippy --quiet --target "$PROBE_TARGET" --target-dir "$work/target" --message-format=short -- -W clippy::disallowed-methods -W clippy::disallowed-types >"$diag" 2>&1) || true
    plants=0
    controls=0
    foreign_plants=0
    foreign_entries=0
    # expected: line -> path, from the source markers
    while IFS=$'\t' read -r line kind path; do
        case "$kind" in
            PLANT|PLANT-TYPE)
                if probe_foreign "$path"; then
                    # Deferred to its own leg only if it really did not compile here.
                    foreign_plants=$((foreign_plants + 1))
                    if grep -q "src/lib.rs:$line:" "$diag"; then
                        echo "CLOCK-BAN violation: probe line $line ($path) was skipped as another leg's, yet compiled for $PROBE_ARCH"
                        fail=1
                    fi
                    continue
                fi
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
    # Exact, so an edited-down fixture is red; a plant added to the probe
    # changes this number with it.
    if [ "$((plants + foreign_plants))" -ne 12 ]; then
        echo "CLOCK-BAN SCOPE ERROR: probe carries $((plants + foreign_plants)) planted lines, expected exactly 12"
        fail=1
    fi
    # An entry that names no reachable item bans nothing: clippy only
    # warns ("does not refer to a reachable function") and the check stays
    # green — batch 14's `_rdtscp` (the intrinsic is `__rdtscp`) was inert
    # for four months (batch 34, ADR-0106 D7.5).
    while IFS= read -r row; do
        entry=${row#\`}
        entry=${entry%%\`*}
        if probe_foreign "$entry"; then
            foreign_entries=$((foreign_entries + 1))
            continue
        fi
        echo "CLOCK-BAN violation: clippy.toml entry does not resolve — $row"
        fail=1
    done < <(grep -o '`[^`]*` does not refer to a reachable [a-z]*' "$diag" | sort -u)
    if grep -q "^error" "$diag"; then
        echo "CLOCK-BAN SCOPE ERROR: the probe did not compile:"
        sed 's/^/    | /' "$diag"
        fail=1
    fi
    probe="$plants planted bypasses red, $controls controls green"
    if [ "$foreign_plants" -ne 0 ] || [ "$foreign_entries" -ne 0 ]; then
        probe="$probe; $foreign_plants plants and $foreign_entries entries under another leg's core::arch not resolvable on $PROBE_ARCH (enforced on their own leg)"
    fi
fi

scope="config $entries/9 entries, $shadows shadow configs, $files cell-source files; allows audited by check-lint-scopes.sh; probe: $probe"
if [ "$fail" -ne 0 ]; then
    echo "clock ban FAILED ($scope)"
    echo "Cell code reads time through inf_foundation::time::Clock only (L7). A sanctioned site carries"
    echo "#[allow(clippy::disallowed_methods, reason = \"…\")] on its statement or fn — never on the crate or file."
    exit 1
fi
echo "clock ban OK ($scope)"
cell_crate_exclusions
