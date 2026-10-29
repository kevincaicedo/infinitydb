# Sourced by check-cell-denylist.sh and check-panic-policy.sh (ADR-0106
# D2): ONE definition of the cell-resident + durable-path source set, so
# the two gates cannot drift apart (review 2026-08-30, F-L17-08 asked for
# exactly this). Not executable on its own.
#
# Default-in: every `crates/*/src` is scanned — a new crate is under both
# gates from its first commit, the way a new scenario should be born run.
# Exclusions are named with their reason, and a listed exclusion that no
# longer exists fails the caller: a stale entry would silently change the
# set, which is the P1 failure shape (a path that evaporated, a gate that
# kept saying OK).
#
# Portable bash 3.2 (macOS CI): no mapfile, no associative arrays.

CELL_CRATE_EXCLUDE=(
    "crates/inf-probe/src|the boot-time device probe (ADR-0086 D7 / ADR-0091): a dev tool that runs before any cell starts; its own header sanctions Instant::now + std::thread"
)

# Prints one directory per line. Returns 1 (after an explanation on
# stderr) when an exclusion names a missing directory or the set is empty.
cell_crate_dirs() {
    local dir excluded entry path reason found=0
    for entry in "${CELL_CRATE_EXCLUDE[@]}"; do
        path=${entry%%|*}
        if [ ! -d "$path" ]; then
            echo "cell-crates: exclusion names a directory that does not exist: $path" >&2
            echo "  (delete the entry or fix the path — a stale exclusion is a silent scope change)" >&2
            return 1
        fi
    done
    for dir in crates/*/src; do
        [ -d "$dir" ] || continue
        excluded=0
        for entry in "${CELL_CRATE_EXCLUDE[@]}"; do
            path=${entry%%|*}
            [ "$dir" = "$path" ] && excluded=1
        done
        [ "$excluded" -eq 1 ] && continue
        echo "$dir"
        found=$((found + 1))
    done
    if [ "$found" -eq 0 ]; then
        echo "cell-crates: no crates/*/src directories found from $(pwd)" >&2
        return 1
    fi
    return 0
}

# Prints the exclusions as `path — reason`, for the gates' scope line.
cell_crate_exclusions() {
    local entry
    for entry in "${CELL_CRATE_EXCLUDE[@]}"; do
        echo "  excluded ${entry%%|*} — ${entry#*|}"
    done
}
