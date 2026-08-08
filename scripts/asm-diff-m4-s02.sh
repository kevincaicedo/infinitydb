#!/usr/bin/env bash
# M4-S02 asm-diff (the S02 AC artifact): the memory-mode hot lookup path
# must be instruction-identical to the M3 baseline after the Index slot
# reinterpretation — zero new instructions on the cache hot path.
#
# Usage:
#   scripts/asm-diff-m4-s02.sh BASELINE_RLIB CURRENT_RLIB OUT_DIR
#
# Both rlibs must be built WITHOUT LTO (bitcode-only members cannot be
# disassembled):
#   CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
#     CARGO_TARGET_DIR=target/asmdiff cargo build --release -p inf-store
#
# Rule: every baseline hot-path block must have a byte-identical twin in
# the M4 rlib (legacy mangling folds monomorphizations into one demangled
# name, so blocks are matched by name AND body). M4-only blocks are the
# new tiered instantiations — listed as informational, they do not fail
# the diff; the runtime proof that they never execute in memory mode is
# M4-S03's zero-counter A/B.
set -euo pipefail

baseline_rlib=$1
current_rlib=$2
out_dir=$3
mkdir -p "$out_dir"

# The §7.3 lookup path + batch prefetch pipeline + write-path entries
# (memory mode). Matches both non-generic (M3 `Index::`) and generic
# (M4 `Index<M>::`) demangled shapes.
hot='CellStore::get|CellStore::set|CellStore::resolve|CellStore::probe_prefetch|CellStore::prefetch|CellStore::hash_key|CellStore::write_record|index::Index'

extract() { # $1 = rlib, $2 = block dir
    mkdir -p "$2"
    # Normalizations (each disclosed in the artifact; a real added,
    # removed, or changed instruction still fails the twin rule):
    #   1. strip legacy-mangling `::h<hash>` suffixes (build-varying);
    #   2. fold the M4 generic spelling `Index<M>` onto the M3 concrete
    #      `Index` so the memory-mode instantiation twins by name;
    #   3. fold intra-function label offsets `+0xNNN` to `+OFF` — a
    #      panic-helper call in the cold tail encodes as reloc-indirect
    #      (6 B) vs direct (5 B) between builds, rippling every later
    #      cold-label offset by one byte;
    #   4. fold that call encoding itself: `call *0x0(%rip) # <T>` and
    #      `call <T>` are the same call through different linkage;
    #   5. fold local-data label names (`.Lanon.<hex>.<n>`, `.LCPI<n>_<m>`)
    #      to `.Llocal` — objdump symbolizes the same relocated target
    #      with whatever local label is nearest, and the numbering shifts
    #      whenever the CGU gains a function; per-build metadata, not code;
    #   6. drop `Disassembly of section` headers (raw mangled names).
    objdump -d --demangle --no-addresses --no-show-raw-insn "$1" |
        sed -e 's/::h[0-9a-f]\{16\}//g' \
            -e 's/Index<M>/Index/g' \
            -e 's/+0x[0-9a-f]*>/+OFF>/g' \
            -e 's/\(call\|jmp\)[[:space:]]*\*0x0(%rip)[[:space:]]*# /\1   /' \
            -e 's/\.Lanon\.[0-9a-f]*\.[0-9]*/.Llocal/g' \
            -e 's/\.LCPI[0-9]*_[0-9]*/.Llocal/g' \
            -e '/^Disassembly of section/d' |
        awk -v dir="$2" -v pat="$hot" '
            /^<.*>:$/ {
                if (out) close(out)
                out = ""
                if ($0 ~ pat) {
                    name = $0
                    gsub(/[^A-Za-z0-9_]/, "_", name)
                    seq[name]++
                    out = dir "/" name "." seq[name]
                    print $0 > out
                }
                next
            }
            out { print > out }
        '
}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
extract "$baseline_rlib" "$scratch/base"
extract "$current_rlib" "$scratch/m4"

# Concatenated views for the artifact (sorted, stable).
for side in base m4; do
    : >"$out_dir/$side-hotpath.asm"
    for block in $(ls "$scratch/$side" | sort); do
        cat "$scratch/$side/$block" >>"$out_dir/$side-hotpath.asm"
        echo >>"$out_dir/$side-hotpath.asm"
    done
done

# Binding set = the lookup/mutation hot path the AC names. Constructors,
# diagnostics, and cold probes (with_capacity, Debug, live_from/replace
# standalone copies, drop glue) are compared and reported but do not bind
# — their runtime cost is zero-per-op and the S03 A/B owns the runtime
# verdict. A baseline block ABSENT from M4 means the generic form inlined
# it into its callers; the callers are in the binding set above.
binding='CellStore::|Index::insert|Index::remove|Index::position_of|Index::find|Index::grow|Index::prefetch'

fail=0
matched=0
: >"$out_dir/verdict.md"
for block in $(ls "$scratch/base" | sort); do
    name=${block%.*}
    header=$(head -1 "$scratch/base/$block")
    twin=""
    candidates=0
    for candidate in "$scratch/m4/$name".*; do
        [ -e "$candidate" ] || continue
        candidates=$((candidates + 1))
        if cmp -s <(tail -n +2 "$scratch/base/$block") <(tail -n +2 "$candidate"); then
            twin=$candidate
            break
        fi
    done
    is_binding=0
    echo "$header" | grep -Eq "$binding" && is_binding=1
    if [ -n "$twin" ]; then
        matched=$((matched + 1))
        echo "IDENTICAL  $header" >>"$out_dir/verdict.md"
    elif [ "$candidates" = 0 ] && [ "$is_binding" = 0 ]; then
        echo "INLINED    $header (no standalone M4 copy; call sites compared above)" \
            >>"$out_dir/verdict.md"
    else
        if [ "$is_binding" = 1 ]; then
            fail=1
            echo "DIVERGED   $header (BINDING)" >>"$out_dir/verdict.md"
        else
            echo "DIVERGED   $header (informational — constructor/diagnostic, off the hot path)" \
                >>"$out_dir/verdict.md"
        fi
        for candidate in "$scratch/m4/$name".*; do
            [ -e "$candidate" ] || continue
            diff -u "$scratch/base/$block" "$candidate" \
                >>"$out_dir/$(basename "$block").diff" || true
        done
    fi
done
echo >>"$out_dir/verdict.md"
echo "M4-only blocks (new instantiations — runtime-dead in memory mode, proven by S03):" \
    >>"$out_dir/verdict.md"
for block in $(ls "$scratch/m4" | sort); do
    name=${block%.*}
    body=$(tail -n +2 "$scratch/m4/$block")
    twinned=0
    for candidate in "$scratch/base/$name".*; do
        [ -e "$candidate" ] || continue
        if [ "$body" = "$(tail -n +2 "$candidate")" ]; then
            twinned=1
            break
        fi
    done
    [ "$twinned" = 0 ] && echo "  $(head -1 "$scratch/m4/$block")" >>"$out_dir/verdict.md"
done

if [ "$fail" = 0 ]; then
    echo "asm-diff-m4-s02 OK: $matched baseline hot-path blocks instruction-identical in M4"
    cat "$out_dir/verdict.md"
else
    echo "asm-diff-m4-s02 FAIL: a baseline hot-path block has no identical twin — see $out_dir"
    cat "$out_dir/verdict.md"
    exit 1
fi
