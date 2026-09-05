#!/usr/bin/env bash
# M0-S06 AC / ADR-0003: no atomic instructions in the executor waker path.
# Rewritten at ADR-0106 D8 (review 2026-08-30, F-L20-03) — the old gate was
# inert in three independent ways, each proven on this tree:
#
#   1. its awk reset `inside` at the first label matching `^[_A-Za-z0-9$.]+:`,
#      and `line-tables-only` debuginfo puts `.Lfunc_beginN:` on the FIRST
#      line of every body — so it scanned 2 lines and ZERO instructions per
#      waker (the finding said "the prologue"; the truth was nothing);
#   2. its mnemonic set was anchored at the line start, so an x86 `lock`
#      prefix — emitted as its own field, `lock<TAB><TAB>cmpxchgq` — never
#      matched: a planted refcount CAS loop in `waker_wake_by_ref` compiled
#      to `lock cmpxchgq` and the gate printed "zero atomic instructions";
#   3. it ran nowhere. No `just` recipe, no workflow — while
#      `crates/inf-runtime/SAFETY.md` cited it as the AC's enforcement.
#
# What it now does:
#   * resolves the waker set from the `RawWakerVTable` static's fn pointers,
#     never from the spelling `waker_*` (a renamed waker would have left the
#     old gate scanning nothing while printing a symbol count);
#   * delimits bodies structurally (`.cfi_startproc` … `.cfi_endproc`, ELF
#     `.size` fallback) and FAILS when a body scans zero instructions;
#   * follows direct calls transitively through every callee whose body is
#     in the same asm (`Rc::drop_slow`, `VecDeque::grow` today), so an
#     atomic one hop out of the waker is a hit with its call path; targets
#     not in the file are disclosed as unresolved edges, never assumed
#     clean;
#   * compiles a planted-bypass probe (`scripts/waker-atomics-probe`) whose
#     own vtable carries known-atomic and known-clean wakers, and asserts
#     each verdict — a gate that cannot go red is not a gate.
#
# Portable bash 3.2 / POSIX awk. A fixture root (INF_CHECK_ROOT) may set
# INF_WAKER_PROBE=off; the scope line says so.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
SCAN="$SCRIPT_DIR/asm-waker-scan.awk"
PROBE_SRC="$SCRIPT_DIR/waker-atomics-probe"

[ -f "$SCAN" ] || { echo "WAKER SCOPE ERROR: scanner missing at $SCAN"; exit 2; }

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "WAKER SCOPE ERROR: mktemp failed"; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

fail=0

# scan_asm <asm> <facts-out>: facts for every function in the file.
scan_asm() {
    awk -f "$SCAN" "$1" >"$2"
}

# verdict <facts> <label>: prints one line per reachable waker,
#   `<sym> <instrs> <atomics> <path>`; plus `UNRESOLVED <sym> <target>`.
# Reachability is the transitive closure of direct calls over bodies that
# exist in this asm — an atomic one hop out is still on the waker path.
verdict() {
    awk '
        $1 == "VTABLE" { roots[$2] = 1; next }
        $1 == "BODY"   { instrs[$2] = $5; seen[$2] = 1; next }
        $1 == "ATOMIC" { atomic[$2] = atomic[$2] " " $3 ":" $4; next }
        $1 == "CALL"   { edge[$2] = edge[$2] " " $4; next }
        $1 == "INDIRECT" { indirect[$2]++; next }
        $1 == "FLAVOR" { flavor = $2; next }
        $1 == "UNTERMINATED" { print "UNTERMINATED " $2; next }
        END {
            print "FLAVOR " flavor
            nq = 0
            for (r in roots) { queue[nq++] = r; from[r] = r; depth[r] = 0 }
            if (nq == 0) { print "NOVTABLE"; exit }
            for (i = 0; i < nq; i++) {
                s = queue[i]
                if (!(s in seen)) { print "UNRESOLVED " from[s] " " s; continue }
                print "SCANNED " s " " instrs[s] " " depth[s] " " from[s] atomic[s]
                n = split(edge[s], t, " ")
                for (j = 1; j <= n; j++) {
                    if (t[j] == "" || (t[j] in from)) continue
                    from[t[j]] = from[s] ">" t[j]
                    depth[t[j]] = depth[s] + 1
                    queue[nq++] = t[j]
                }
            }
        }
    ' "$1"
}

# ---- 1. the real tree ---------------------------------------------------
ASM="$work/inf_runtime.s"
if [ -n "${INF_WAKER_ASM:-}" ]; then
    # Self-test hook: a fixture asm stands in for the crate build so the
    # verdict logic (body delimitation, the mnemonic fields, the vtable
    # resolution, the zero-instruction scope error) is itself testable.
    cp "$INF_WAKER_ASM" "$ASM"
else
    RUSTFLAGS="--emit asm=$ASM" cargo rustc -p inf-runtime --release --lib >"$work/build.log" 2>&1 || {
        echo "WAKER SCOPE ERROR: inf-runtime release build failed:"; sed 's/^/    | /' "$work/build.log"; exit 2
    }
fi
[ -s "$ASM" ] || { echo "WAKER SCOPE ERROR: no asm emitted at $ASM"; exit 2; }
scan_asm "$ASM" "$work/facts"
verdict "$work/facts" >"$work/verdict"

flavor=$(awk '$1=="FLAVOR"{print $2}' "$work/verdict")
if grep -q '^NOVTABLE' "$work/verdict"; then
    echo "WAKER SCOPE ERROR: no RawWakerVTable fn pointers found in $ASM"
    echo "  (the waker set is resolved from the vtable static; a renamed static means this gate scans nothing)"
    exit 1
fi
if [ "$flavor" = unknown ]; then
    echo "WAKER SCOPE ERROR: unrecognised asm flavor (neither ELF @function nor Mach-O)"
    exit 1
fi
if grep -q '^UNTERMINATED' "$work/verdict"; then
    echo "WAKER SCOPE ERROR: a function body never reached .cfi_endproc:"
    grep '^UNTERMINATED' "$work/verdict" | sed 's/^/    /'
    exit 1
fi

wakers=0
reached=0
instrs_total=0
zero=0
while read -r _ sym n depth path rest; do
    reached=$((reached + 1))
    instrs_total=$((instrs_total + n))
    [ "$depth" = 0 ] && wakers=$((wakers + 1))
    if [ "$n" -eq 0 ]; then
        echo "WAKER SCOPE ERROR: $sym scanned 0 instruction lines (body delimitation broke)"
        zero=$((zero + 1))
        fail=1
    fi
    if [ -n "$rest" ]; then
        echo "ATOMIC on the waker path: $path"
        echo "  at $sym: $rest"
        fail=1
    fi
done < <(grep '^SCANNED ' "$work/verdict")

if [ "$wakers" -ne 4 ]; then
    echo "WAKER SCOPE ERROR: the vtable resolved $wakers waker bodies, expected 4 (clone/wake/wake_by_ref/drop)"
    fail=1
fi
# Edges whose callee has no body in this asm (allocator shim, memcpy,
# cold panic/unwind, a sibling CGU). Not followed, so never assumed clean —
# listed on every run so the boundary of the claim is visible.
{ grep '^UNRESOLVED ' "$work/verdict" || true; } | awk '{print $3}' | sort -u >"$work/unres"
unresolved=$(wc -l <"$work/unres" | tr -d ' ')
if [ "$unresolved" -gt 0 ]; then
    echo "  unfollowed callees (no body in this asm): $(tr '\n' ' ' <"$work/unres")"
fi

# ---- 2. the planted-bypass probe ----------------------------------------
probe="skipped (fixture mode)"
if [ -n "${INF_WAKER_ASM:-}" ] && [ "${INF_WAKER_PROBE:-on}" = off ]; then
    :
else
    [ -f "$PROBE_SRC/src/lib.rs" ] || { echo "WAKER SCOPE ERROR: probe crate missing at $PROBE_SRC"; exit 1; }
    cp -R "$PROBE_SRC/." "$work/probe"
    PASM="$work/probe.s"
    (cd "$work/probe" && RUSTFLAGS="--emit asm=$PASM" cargo build --release --quiet --target-dir "$work/ptarget") >"$work/probe.log" 2>&1 || {
        echo "WAKER SCOPE ERROR: the probe did not build:"; sed 's/^/    | /' "$work/probe.log"; exit 2
    }
    [ -s "$PASM" ] || { echo "WAKER SCOPE ERROR: probe emitted no asm"; exit 2; }
    scan_asm "$PASM" "$work/pfacts"
    verdict "$work/pfacts" >"$work/pverdict"
    plants=0
    cleans=0
    # `// WAKER-PROBE: expect-<atomic|clean|unscanned> <name>` in the probe
    # source is the contract; the gate asserts each verdict.
    while read -r kind name; do
        case "$kind" in
            expect-atomic)
                plants=$((plants + 1))
                if ! awk -v n="$name" '$1=="SCANNED" && index($2,n) && NF>5 {f=1} END{exit !f}' "$work/pverdict"; then
                    echo "WAKER violation: probe $name carries a planted atomic that was NOT reported"
                    fail=1
                fi ;;
            expect-clean)
                cleans=$((cleans + 1))
                if ! awk -v n="$name" '$1=="SCANNED" && index($2,n) && NF==5 && $3+0>0 {f=1} END{exit !f}' "$work/pverdict"; then
                    echo "WAKER violation: probe $name should scan clean with >0 instructions; it did not"
                    fail=1
                fi ;;
            expect-unscanned)
                cleans=$((cleans + 1))
                if awk -v n="$name" '$1=="SCANNED" && index($2,n) {f=1} END{exit !f}' "$work/pverdict"; then
                    echo "WAKER violation: probe $name is off the waker path but WAS scanned — the gate over-reports"
                    fail=1
                fi
                if ! grep -q "^ATOMIC .*$name" "$work/pfacts"; then
                    echo "WAKER SCOPE ERROR: probe $name is supposed to carry an atomic the scanner sees; it does not"
                    fail=1
                fi ;;
        esac
    done < <(sed -n 's|^.*WAKER-PROBE: \(expect-[a-z]*\)[[:space:]]\{1,\}\([A-Za-z0-9_]*\).*$|\1 \2|p' "$PROBE_SRC/src/lib.rs")
    if [ "$plants" -lt 3 ]; then
        echo "WAKER SCOPE ERROR: probe declares $plants planted wakers (expected >= 3) — the fixture was edited down"
        fail=1
    fi
    probe="$plants planted wakers red, $cleans controls green"
fi

scope="$flavor asm, vtable-resolved: 4 wakers + $((reached - wakers)) called bodies, $instrs_total instruction lines scanned, $unresolved unresolved edges, $zero zero-instruction bodies; probe: $probe"
if [ "$fail" -ne 0 ]; then
    echo "waker-atomics FAILED ($scope)"
    echo "The executor waker path is atomic-free by construction (Rc, not Arc — L1/ADR-0003)."
    exit 1
fi
echo "waker-atomics OK ($scope)"
