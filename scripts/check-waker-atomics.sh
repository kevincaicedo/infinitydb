#!/usr/bin/env bash
# The waker's owner lane runs no atomic (ADR-0003 A1.5, A1.6; ADR-0106 D8's mechanics).
# A cell executor's `Waker` takes two lanes: on the cell's own thread the owner lane, which runs
# no atomic read-modify-write or fence; on any other thread, and once the executor is gone, the
# foreign lane, `inf_runtime::executor::foreign`, whose five `#[inline(never)]` functions are
# this gate's boundary. The gate reads `inf-runtime`'s release asm:
#
#   * it resolves the waker set from the `RawWakerVTable` static's fn pointers, never from the
#     spelling `waker_*` (a renamed waker would leave a name-based gate scanning nothing while it
#     printed a count);
#   * it delimits bodies structurally (`.cfi_startproc` … `.cfi_endproc`, ELF `.size` fallback)
#     and FAILS when a body scans zero instructions;
#   * it follows direct calls and branches, a conditional tail call among them, transitively
#     through every callee whose body is in the same asm: that is the owner lane, and an atomic
#     there is red with its call path. A call into
#     `inf_foundation::wake` is no exception: followed where its body is in the asm, and an
#     unlisted callee where it is not;
#   * the boundary (BOUNDARY below) is present, not followed: each of the five functions of
#     `executor::foreign` a root reaches is listed with its atomic count, and each that no root
#     reaches is red (the boundary the gate exempts must exist);
#   * a callee with no body in the asm is red unless it is in the closed callee list (CLOSED
#     below): the TLS resolver, `memcpy`/`memmove`/`memset`, a panic, abort or unwind entry, and
#     the ready queue's growth (ADR-0003 A1.5's allocator exception, which leaves the list with
#     the executor's intrusive ready list); the listed ones are printed on every run;
#   * an indirect call on the owner lane is red (its target cannot be followed); indirect jumps
#     are counted; a control transfer the scanner has no edge rule for is red (UNREAD);
#   * it compiles a planted-bypass probe (`scripts/waker-atomics-probe`), whose three vtables
#     and boundary module carry known verdicts, and asserts each one: a gate that cannot go red
#     is not a gate.
#
# The ordered-access clause (an acquire load or a release store) is seen on aarch64, whose
# `ldar`/`ldapr`/`stlr` the scanner counts; on x86-64 such an access is a plain `mov`, so there
# the clause is review's, as is `settle_held`'s body (ADR-0003 A1.6).
#
# Atomics are spelled per target — aarch64 Linux calls the outline-atomics helpers, Apple emits
# LSE and `stlur`/`ldapur` inline — so the tree and the probe compile for one explicit target
# (probe-target.sh: CARGO_BUILD_TARGET, else the host), with the toolchain rustup resolves here:
# the probe is built from this directory, so the pin in rust-toolchain.toml judges both. The
# scope line names the target.
#
# Portable bash 3.2 / POSIX awk. A fixture root (INF_CHECK_ROOT) may set INF_WAKER_PROBE=off;
# with INF_WAKER_ASM set, INF_WAKER_PROBE_ASM stands in for the probe's build and
# INF_WAKER_LIVE=off skips the live-boundary check of a fixture that holds no boundary (self-test
# only); the scope line says each.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
# shellcheck source=probe-target.sh
. "$SCRIPT_DIR/probe-target.sh"
SCAN="$SCRIPT_DIR/asm-waker-scan.awk"
PROBE_SRC="$SCRIPT_DIR/waker-atomics-probe"

[ -f "$SCAN" ] || { echo "WAKER SCOPE ERROR: scanner missing at $SCAN"; exit 2; }

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "WAKER SCOPE ERROR: mktemp failed"; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

# The boundary: the five functions of `executor::foreign` (ADR-0003 A1.6), one array for the
# tree and the probe, whose module sits at the same path. A body is a boundary function when its
# symbol's path ends `executor::foreign::<name>` (legacy or v0 mangling).
BOUNDARY="clone wake wake_by_ref drop settle_held"
# The closed callee list: a callee with no body in the asm that the owner lane may call. The TLS
# resolver (ELF's `__tls_get_addr`; Mach-O's thread-local thunk), `memcpy`/`memmove`/`memset`, a
# panic, abort or unwind entry, and the ready queue's growth (`VecDeque::grow`,
# `RawVec::grow_one`): an ERE over the mangled symbol.
CLOSED='^_*(_tls_get_addr|tls_get_addr|tlv_get_addr|tlv_bootstrap|memcpy|memmove|memset|Unwind_Resume|abort|rust_begin_unwind|rust_panic)$'
CLOSED="$CLOSED"'|4core9panicking|3std9panicking|4core4cell22panic_already_borrowed'
CLOSED="$CLOSED"'|4core4cell30panic_already_mutably_borrowed|3std7process5abort'
CLOSED="$CLOSED"'|4core6option13unwrap_failed|4core6option13expect_failed|4core6result13unwrap_failed'
CLOSED="$CLOSED"'|5alloc5alloc18handle_alloc_error|5alloc7raw_vec12handle_error'
CLOSED="$CLOSED"'|5alloc7raw_vec17capacity_overflow|9vec_deque.*4grow|7raw_vec.*8grow_one'

fail=0
if [ -n "${INF_WAKER_PROBE_ASM:-}" ] && [ -z "${INF_WAKER_ASM:-}" ]; then
    echo "WAKER SCOPE ERROR: INF_WAKER_PROBE_ASM without INF_WAKER_ASM would replace the real probe"
    exit 2
fi
if [ "${INF_WAKER_LIVE:-on}" = off ] && [ -z "${INF_WAKER_ASM:-}" ]; then
    echo "WAKER SCOPE ERROR: INF_WAKER_LIVE=off outside fixture mode would skip the live boundary"
    exit 2
fi
target=
need_target() { # the one target both builds compile for, resolved once
    [ -z "$target" ] || return 0
    probe_target || { echo "WAKER SCOPE ERROR: the build's target is unknown"; exit 2; }
    target=$PROBE_TARGET
}

# scan_asm <asm> <facts-out>: facts for every function in the file.
scan_asm() {
    awk -f "$SCAN" "$1" >"$2"
}

# verdict <facts>: per body a root reaches, `SCANNED <sym> <instrs> <depth> <path> [atomics…]`
# for the owner lane and `BOUNDARY <name> <sym> <instrs> <path> [atomics…]` for a boundary
# function (present, not followed); per callee with no body `LISTED <path> <sym>` or `UNLISTED
# <path> <sym>`; `ICALL <sym> <line> <path>` per indirect call and `IJUMP <sym> <n>`;
# `UNREAD <sym> <path> <line>:<mnemonic>…` per scanned body with a transfer no rule reads; and
# `DEAD <name>` per boundary function no root reaches. Reachability is the transitive closure
# of direct calls over bodies that exist in this asm, stopped at the boundary.
verdict() {
    awk -v boundary="$BOUNDARY" -v closed="$CLOSED" '
        # The boundary name of `sym`, or "": its path ends `8executor7foreign<len><name>`, then
        # nothing, a legacy hash `17h…E` or an LLVM `.` suffix.
        function bname(sym,   i, rest, len, name, tail) {
            i = index(sym, "8executor7foreign")
            if (i == 0) return ""
            rest = substr(sym, i + 17)
            if (!match(rest, /^[0-9]+/)) return ""
            len = substr(rest, 1, RLENGTH) + 0
            name = substr(rest, RLENGTH + 1, len)
            tail = substr(rest, RLENGTH + 1 + len)
            if (tail != "" && tail !~ /^17h[0-9a-f]+E/ && tail !~ /^[.]/) return ""
            return (name in isb) ? name : ""
        }
        BEGIN { nb = split(boundary, bl, " "); for (j = 1; j <= nb; j++) isb[bl[j]] = 1 }
        $1 == "VTABLE" { roots[$2] = 1; next }
        $1 == "BODY"   { instrs[$2] = $5; seen[$2] = 1; next }
        $1 == "ATOMIC" { atomic[$2] = atomic[$2] " " $3 ":" $4; next }
        $1 == "CALL"   { edge[$2] = edge[$2] " " $4; next }
        $1 == "UNREAD" { unread[$2] = unread[$2] " " $3 ":" $4; next }
        $1 == "INDIRECT" {
            if ($4 == "call") icall[$2] = icall[$2] " " $3
            else ijump[$2]++
            next
        }
        $1 == "FLAVOR" { flavor = $2; next }
        $1 == "UNTERMINATED" { print "UNTERMINATED " $2; next }
        END {
            print "FLAVOR " flavor
            nq = 0
            for (r in roots) { queue[nq++] = r; from[r] = r; depth[r] = 0 }
            if (nq == 0) { print "NOVTABLE"; exit }
            for (i = 0; i < nq; i++) {
                s = queue[i]
                if (!(s in seen)) {
                    print ((s ~ closed) ? "LISTED " : "UNLISTED ") from[s] " " s
                    continue
                }
                b = bname(s)
                if (b != "") {
                    reached[b] = 1
                    print "BOUNDARY " b " " s " " instrs[s] " " from[s] atomic[s]
                    continue
                }
                print "SCANNED " s " " instrs[s] " " depth[s] " " from[s] atomic[s]
                m = split(icall[s], c, " ")
                for (j = 1; j <= m; j++) print "ICALL " s " " c[j] " " from[s]
                if (s in ijump) print "IJUMP " s " " ijump[s]
                if (s in unread) print "UNREAD " s " " from[s] unread[s]
                n = split(edge[s], t, " ")
                for (j = 1; j <= n; j++) {
                    if (t[j] == "" || (t[j] in from)) continue
                    from[t[j]] = from[s] ">" t[j]
                    depth[t[j]] = depth[s] + 1
                    queue[nq++] = t[j]
                }
            }
            for (j = 1; j <= nb; j++) if (!(bl[j] in reached)) print "DEAD " bl[j]
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
    tree_target=fixture
else
    need_target
    tree_target=$target
    RUSTFLAGS="--emit asm=$ASM" cargo rustc -p inf-runtime --release --lib --target "$target" \
        >"$work/build.log" 2>&1 || {
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
        echo "ATOMIC on the waker's owner lane: $path"
        echo "  at $sym: $rest"
        fail=1
    fi
done < <(grep '^SCANNED ' "$work/verdict")

# The boundary: each function a root reaches is listed with its atomic count, and each that no
# root reaches is red, so the exemption names functions that exist.
boundary_seen=
while read -r _ name _ n _ rest; do
    count=0
    for _a in $rest; do count=$((count + 1)); done
    boundary_seen="$boundary_seen $name ($n instructions, $count atomics),"
done < <(grep '^BOUNDARY ' "$work/verdict" | sort -u -k2,2)
boundary_seen=${boundary_seen%,}
live="checked"
if [ -n "${INF_WAKER_ASM:-}" ] && [ "${INF_WAKER_LIVE:-on}" = off ]; then
    live="not checked (fixture)"
else
    while read -r _ name; do
        echo "DEAD BOUNDARY: executor::foreign::$name is a boundary function no waker reaches"
        echo "  (the gate exempts the boundary's bodies, so each must be reached to be one)"
        fail=1
    done < <(grep '^DEAD ' "$work/verdict")
fi

if [ "$wakers" -ne 4 ]; then
    echo "WAKER SCOPE ERROR: the vtable resolved $wakers waker bodies, expected 4 (clone/wake/wake_by_ref/drop)"
    fail=1
fi
# An indirect call leaves the scanned set for a body no fact names, so it is
# a violation, never an unfollowed edge; an indirect jump (a jump table, on
# x86 `jmpq *%rax`) stays in the body or tail-branches and is disclosed.
while read -r _ sym line path; do
    echo "INDIRECT CALL on the waker path: $path"
    echo "  at $sym: asm line $line (its target cannot be followed, so it is never assumed clean)"
    fail=1
done < <(grep '^ICALL ' "$work/verdict")
ijumps=$(awk '$1 == "IJUMP" { n += $3 } END { print n + 0 }' "$work/verdict")
# A transfer the scanner has no edge rule for leaves its target unread: red, never clean.
while read -r _ sym path rest; do
    echo "UNREAD BRANCH on the waker path: $path"
    echo "  at $sym (asm line:mnemonic):$rest; the scanner reads no edge for it, so it is never assumed clean"
    fail=1
done < <(grep '^UNREAD ' "$work/verdict")

# Callees with no body in this asm are never assumed clean: one outside the closed list is red,
# with its path, and the listed ones are printed on every run.
while read -r _ path sym; do
    echo "UNLISTED CALLEE on the waker's owner lane: $path"
    echo "  $sym has no body in this asm and is no entry of the closed callee list"
    fail=1
done < <(grep '^UNLISTED ' "$work/verdict")
{ grep -E '^(LISTED|UNLISTED) ' "$work/verdict" || true; } | awk '{print $3}' | sort -u >"$work/unres"
unresolved=$(wc -l <"$work/unres" | tr -d ' ')
{ grep '^LISTED ' "$work/verdict" || true; } | awk '{print $3}' | sort -u >"$work/listed"
if [ -s "$work/listed" ]; then
    echo "  listed callees (no body in this asm): $(tr '\n' ' ' <"$work/listed")"
fi

# ---- 2. the planted-bypass probe ----------------------------------------
probe="skipped (fixture mode)"
if [ -n "${INF_WAKER_ASM:-}" ] && [ "${INF_WAKER_PROBE:-on}" = off ]; then
    :
else
    [ -f "$PROBE_SRC/src/lib.rs" ] || { echo "WAKER SCOPE ERROR: probe crate missing at $PROBE_SRC"; exit 1; }
    PASM="$work/probe.s"
    if [ -n "${INF_WAKER_PROBE_ASM:-}" ]; then
        cp "$INF_WAKER_PROBE_ASM" "$PASM"
        probe_for="fixture asm"
    else
        need_target
        probe_for=$target
        cp -R "$PROBE_SRC/." "$work/probe"
        # Built from this directory, not the copy's: the same toolchain and
        # target as the tree, so the probe proves the scan of that codegen. It
        # builds cold, in a directory no earlier run wrote (ADR-0204 D4).
        # shellcheck source=target-root.sh
        . "$SCRIPT_DIR/target-root.sh"
        target_root_cargo_fresh "$work/ptarget" env RUSTFLAGS="--emit asm=$PASM" cargo build --release --quiet --target "$target" \
            --manifest-path "$work/probe/Cargo.toml" >"$work/probe.log" 2>&1 || {
            echo "WAKER SCOPE ERROR: the probe did not build:"; sed 's/^/    | /' "$work/probe.log"; exit 2
        }
    fi
    [ -s "$PASM" ] || { echo "WAKER SCOPE ERROR: probe emitted no asm"; exit 2; }
    scan_asm "$PASM" "$work/pfacts"
    verdict "$work/pfacts" >"$work/pverdict"
    # A probe name matches only its own symbol: the crate-root path
    # `19waker_atomics_probe10waker_wake` then, in legacy mangling, the
    # `17h<hash>E` suffix, or, in v0, the end of the symbol (an LLVM `.`
    # suffix may follow either). A substring let `waker_wake` match
    # `waker_wake_by_ref`, whose SeqCst store kept the fetch-add plant "red"
    # on arm64 Linux while the fetch-add itself went unreported; a prefix
    # would let a clean nested `waker_drop::inner` stand in for `waker_drop`.
    crate=$(sed -n 's/^name = "\(.*\)"$/\1/p' "$PROBE_SRC/Cargo.toml" | tr - _)
    [ -n "$crate" ] || { echo "WAKER SCOPE ERROR: no package name in the probe's Cargo.toml"; exit 1; }
    # probe_hit <atomic|clean|scanned|carries|boundary|unlisted> <name> <facts|verdict file>:
    # `atomic`, an owner-lane body on a path from the root `name` carries an atomic; `boundary`,
    # a boundary function reached from it carries one; `unlisted`, an unlisted callee is on a
    # path from it.
    probe_hit() {
        awk -v mode="$1" -v k="${#crate}$crate${#2}$2" '
            function own(sym,   i, rest) {
                i = index(sym, k)
                if (i == 0) return 0
                rest = substr(sym, i + length(k))
                return rest == "" || rest ~ /^[.]/ || rest ~ /^17h[0-9a-f]+E$/ || rest ~ /^17h[0-9a-f]+E[.]/
            }
            function root(path,   i) {
                i = index(path, ">")
                return i == 0 ? path : substr(path, 1, i - 1)
            }
            mode == "atomic"   && $1 == "SCANNED" && own(root($5)) && NF > 5 { f = 1 }
            mode == "clean"    && $1 == "SCANNED" && own($2) && NF == 5 && $3 + 0 > 0 { f = 1 }
            mode == "scanned"  && $1 == "SCANNED" && own($2) { f = 1 }
            mode == "carries"  && $1 == "ATOMIC" && own($2) { f = 1 }
            mode == "boundary" && $1 == "BOUNDARY" && own(root($5)) && NF > 5 { f = 1 }
            mode == "unlisted" && $1 == "UNLISTED" && own(root($2)) { f = 1 }
            END { exit !f }' "$3"
    }
    atomics=0
    unlisted=0
    dead=0
    reds=0
    cleans=0
    offpath=0
    boundaries=0
    greens=0
    # `// WAKER-PROBE: expect-<kind> <name>` in the probe source is the contract; the gate
    # asserts each verdict.
    while read -r kind name; do
        case "$kind" in
            expect-atomic)
                atomics=$((atomics + 1))
                if probe_hit atomic "$name" "$work/pverdict"; then
                    reds=$((reds + 1))
                else
                    echo "WAKER violation: probe $name carries a planted atomic that was NOT reported"
                    fail=1
                fi ;;
            expect-unlisted)
                unlisted=$((unlisted + 1))
                if probe_hit unlisted "$name" "$work/pverdict"; then
                    reds=$((reds + 1))
                else
                    echo "WAKER violation: probe $name calls an unlisted callee that was NOT reported"
                    fail=1
                fi ;;
            expect-dead-boundary)
                dead=$((dead + 1))
                if grep -qx "DEAD $name" "$work/pverdict"; then
                    reds=$((reds + 1))
                else
                    echo "WAKER violation: probe boundary $name is reached by no root, and it was NOT reported"
                    fail=1
                fi ;;
            expect-clean)
                cleans=$((cleans + 1))
                if probe_hit clean "$name" "$work/pverdict"; then
                    greens=$((greens + 1))
                else
                    echo "WAKER violation: probe $name should scan clean with >0 instructions; it did not"
                    fail=1
                fi ;;
            expect-boundary)
                boundaries=$((boundaries + 1))
                if probe_hit clean "$name" "$work/pverdict" && probe_hit boundary "$name" "$work/pverdict"; then
                    greens=$((greens + 1))
                else
                    echo "WAKER violation: probe $name should scan clean with its boundary's atomic listed; it did not"
                    fail=1
                fi ;;
            expect-unscanned)
                offpath=$((offpath + 1))
                ok=1
                if probe_hit scanned "$name" "$work/pverdict"; then
                    echo "WAKER violation: probe $name is off the waker path but WAS scanned — the gate over-reports"
                    fail=1
                    ok=0
                fi
                if ! probe_hit carries "$name" "$work/pfacts"; then
                    echo "WAKER SCOPE ERROR: probe $name is supposed to carry an atomic the scanner sees; it does not"
                    fail=1
                    ok=0
                fi
                greens=$((greens + ok)) ;;
        esac
    done < <(sed -n 's|^.*WAKER-PROBE: \(expect-[a-z-]*\)[[:space:]]\{1,\}\([A-Za-z0-9_]*\).*$|\1 \2|p' "$PROBE_SRC/src/lib.rs")
    # The plants: three refcount spellings, an atomic beside a boundary call, the drift into
    # another module's read-modify-write and a read-modify-write a conditional tail call
    # reaches; one unlisted callee; one dead boundary.
    if [ "$atomics" -lt 6 ] || [ "$unlisted" -ne 1 ] || [ "$dead" -ne 1 ]; then
        echo "WAKER SCOPE ERROR: probe declares $atomics atomic, $unlisted unlisted and $dead dead-boundary plants (expected >= 6, 1 and 1) — the fixture was edited down"
        fail=1
    fi
    # The controls are exact: the vtable's one clean waker, one atomic off the path and one
    # waker whose atomics stay behind the boundary. Deleting one would leave a gate that cannot
    # show it under-scans a clean body, over-reports an unreachable one, or reports the boundary.
    if [ "$cleans" -ne 1 ] || [ "$offpath" -ne 1 ] || [ "$boundaries" -ne 1 ]; then
        echo "WAKER SCOPE ERROR: probe declares $cleans clean, $offpath off-path and $boundaries boundary controls (expected exactly 1, 1 and 1)"
        fail=1
    fi
    plants=$((atomics + unlisted + dead))
    cleans=$((cleans + offpath + boundaries))
    probe="$probe_for, $reds of $plants plants red, $greens of $cleans controls green"
fi

# ---- 3. the source census (ADR-0003 A1.6, A1.7) --------------------------
# A hand-made `Waker` needs a `RawWakerVTable`, and the workspace ships one: in code (comments
# and string literals removed), every `.rs` file under crates/, bins/ and tests/ that names it
# is red unless it is an entry below, each a file with the rule that keeps it. An entry that is
# absent, or names it nowhere, is red too, so an entry leaves with what it holds. The one entry
# is the executor's waker vtable.
CENSUS_ENTRIES=(
    "crates/inf-runtime/src/executor.rs|the executor's waker vtable"
)
# A Rust file's code: comments (line and nested block), string literals (plain, byte and raw)
# and char literals blanked; lifetimes stay. One line out per line in.
cat >"$work/code-only.awk" <<'AWK'
{
    line = $0; out = ""; i = 1; n = length(line)
    while (i <= n) {
        c = substr(line, i, 1); two = substr(line, i, 2)
        if (depth > 0) {
            if (two == "*/") { depth--; i += 2 } else if (two == "/*") { depth++; i += 2 } else { i++ }
            continue
        }
        if (instr) {
            if (c == "\\") { i += 2 } else { if (c == "\"") instr = 0; i++ }
            continue
        }
        if (inraw) {
            if (substr(line, i, length(closer)) == closer) { inraw = 0; i += length(closer) } else { i++ }
            continue
        }
        if (two == "//") break
        if (two == "/*") { depth = 1; i += 2; continue }
        if (c == "\"") { instr = 1; i++; continue }
        prev = (i > 1) ? substr(line, i - 1, 1) : ""
        if ((c == "r" || c == "b") && prev !~ /[A-Za-z0-9_]/ && match(substr(line, i), /^b?r#*"/)) {
            closer = "\"" substr(substr(line, i, RLENGTH), (c == "b" ? 3 : 2), RLENGTH - (c == "b" ? 3 : 2))
            inraw = 1; i += RLENGTH; continue
        }
        if (c == "'" && match(substr(line, i), /^'([^\\']|\\[^x]|\\x[0-9a-fA-F][0-9a-fA-F]|\\u[{][0-9a-fA-F]+[}])'/)) {
            i += RLENGTH; continue
        }
        out = out c; i++
    }
    print out
}
AWK
census_read=0
census_named=
for dir in crates bins tests; do
    [ -d "$dir" ] || { echo "WAKER SCOPE ERROR: the census's directory $dir/ is missing"; exit 2; }
done
census_read=$(find crates bins tests -name '*.rs' -type f -not -path '*/target/*' | wc -l | tr -d ' ')
[ "$census_read" -gt 0 ] || { echo "WAKER SCOPE ERROR: the census read no .rs file"; exit 2; }
while IFS= read -r file; do
    # Through a file: under pipefail, `grep -q` closing the pipe early would fail awk's write.
    awk -f "$work/code-only.awk" "$file" >"$work/code-only.rs"
    if grep -q 'RawWakerVTable' "$work/code-only.rs"; then
        census_named="$census_named $file"
    fi
done < <(grep -rl --include='*.rs' --exclude-dir=target RawWakerVTable crates bins tests || true)
census_entries=
for entry in "${CENSUS_ENTRIES[@]}"; do
    file=${entry%%|*}
    census_entries="$census_entries $file (${entry#*|}),"
    case " $census_named " in
        *" $file "*) ;;
        *)
            if [ -f "$file" ]; then
                echo "WAKER CENSUS: $file is an entry that names RawWakerVTable nowhere in its code"
            else
                echo "WAKER CENSUS: $file is an entry that is absent"
            fi
            echo "  (an entry leaves the census with what it holds)"
            fail=1 ;;
    esac
done
for file in $census_named; do
    listed=0
    for entry in "${CENSUS_ENTRIES[@]}"; do
        [ "$file" = "${entry%%|*}" ] && listed=1
    done
    if [ "$listed" -eq 0 ]; then
        echo "WAKER CENSUS: $file names RawWakerVTable outside the census's entries"
        echo "  (the workspace ships one hand-made waker: use Waker::noop(), or the executor's)"
        fail=1
    fi
done
census_entries=${census_entries%,}

scope="$tree_target $flavor asm, vtable-resolved: 4 wakers + $((reached - wakers)) owner-lane bodies, $instrs_total instruction lines scanned, $unresolved callees with no body (each listed), $ijumps indirect jumps, $zero zero-instruction bodies; boundary ($live):${boundary_seen:- none}; census: $census_read .rs files read, entries:$census_entries; probe: $probe"
if [ "$fail" -ne 0 ]; then
    echo "waker-atomics FAILED ($scope)"
    echo "The waker's owner lane runs no atomic: its read-modify-writes are the foreign lane's (ADR-0003 A1.5)."
    exit 1
fi
echo "waker-atomics OK ($scope)"
