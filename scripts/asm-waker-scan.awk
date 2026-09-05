# Waker-body scanner for check-waker-atomics.sh (ADR-0106 D8).
#
# Reads a rustc `--emit asm` file and prints, one fact per line:
#
#   FLAVOR <elf|macho|unknown>
#   VTABLE <sym>                     a fn pointer stored in a RawWakerVTable
#   BODY <sym> <start> <end> <instrs>
#   ATOMIC <sym> <line> <mnemonic>   an atomic instruction inside a body
#   CALL <sym> <line> <target>       a direct call/branch leaving the body
#   INDIRECT <sym> <line>            an indirect branch inside a body
#
# Bodies are delimited structurally — the symbol label line, then
# `.cfi_startproc` … `.cfi_endproc` (emitted for both ELF and Mach-O), with
# ELF's `.size <sym>, …` as a fallback. Compiler-local labels (`.LBB…`,
# `.Ltmp…`, `.Lfunc_begin…`) are NOT boundaries: the old awk reset on them,
# and because `line-tables-only` debuginfo puts `.Lfunc_beginN:` on the
# FIRST line of every body, it scanned two lines and zero instructions per
# waker while printing OK (review 2026-08-30, F-L20-03).
#
# Mnemonics are matched on the instruction's FIRST TWO fields, so an x86
# `lock` prefix — emitted as its own tab-separated field, `lock<TAB><TAB>
# cmpxchgq` — is caught. The old pattern anchored the whole set at the line
# start and therefore missed exactly the spelling a refcount CAS produces.
#
# POSIX awk only (mawk, gawk, BSD awk): the CI matrix includes macOS.

function is_atomic(m) {
    return m ~ /^(lock|xacquire|xrelease|xchg[bwlq]?|cmpxchg([0-9]+b|[bwlq])?|xadd[bwlq]?|mfence|lfence|sfence)$/ ||
           m ~ /^(ldxr[bh]?|ldaxr[bh]?|ldxp|ldaxp|stxr[bh]?|stlxr[bh]?|stxp|stlxp|ldar[bh]?|stlr[bh]?|ldapr[bh]?)$/ ||
           m ~ /^(ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|swp|cas|casp)[abhl]*$/ ||
           m ~ /^(dmb|dsb|isb)$/
}

function is_branch(m) {
    return m ~ /^(call|callq|calll|jmp|jmpq|bl|br|blr|b)$/
}

BEGIN { flavor = "unknown"; inbody = 0 }

# ---- flavor -------------------------------------------------------------
/,[[:space:]]*@function/ { if (flavor == "unknown") flavor = "elf" }
/\.subsections_via_symbols/ { flavor = "macho" }

# ---- the RawWakerVTable static ------------------------------------------
# `<mangled WAKER_VTABLE sym>:` followed by one `.quad`/`.long`/`.xword`
# per fn pointer. The waker set is resolved from the vtable, never from the
# spelling of a function's name: a waker renamed away from `waker_*` would
# otherwise leave the gate scanning nothing while it printed a count.
/^[A-Za-z_$.][A-Za-z0-9_$.]*WAKER_VTABLE[A-Za-z0-9_$.]*:[[:space:]]*$/ { invtable = 1; next }
invtable == 1 {
    if ($1 ~ /^\.(quad|long|xword)$/) {
        if ($2 !~ /^\./) { print "VTABLE " $2 }
        next
    }
    invtable = 0
}

# ---- function bodies ----------------------------------------------------
inbody == 1 {
    if ($1 == ".cfi_endproc" || ($1 == ".size" && index($0, cur) > 0)) {
        print "BODY " cur " " start " " NR " " instrs
        inbody = 0
        next
    }
    # Directives and labels are not instructions.
    if ($0 ~ /^[[:space:]]*\./ || $0 ~ /^[^[:space:]]/ || $0 ~ /^[[:space:]]*#/ || $0 ~ /^[[:space:]]*$/) { next }
    instrs++
    if (is_atomic($1)) { print "ATOMIC " cur " " NR " " $1 }
    else if (is_atomic($2)) { print "ATOMIC " cur " " NR " " $2 }
    if (is_branch($1)) {
        tgt = $2
        sub(/^\*/, "", tgt)
        if (tgt ~ /^[%]/ || tgt ~ /^[xw][0-9]+$/ || tgt == "") { print "INDIRECT " cur " " NR }
        else if (tgt !~ /^\.L/) {
            sub(/@.*$/, "", tgt)
            sub(/\(%rip\)$/, "", tgt)
            print "CALL " cur " " NR " " tgt
        }
    }
    next
}

# A bare symbol label at column 0 arms the next `.cfi_startproc`.
/^[A-Za-z_$.][A-Za-z0-9_$.]*:[[:space:]]*$/ {
    lbl = $0
    sub(/:[[:space:]]*$/, "", lbl)
    if (lbl !~ /^\.L/) { pending = lbl; pline = NR }
    next
}

$1 == ".cfi_startproc" && pending != "" {
    cur = pending
    start = pline
    instrs = 0
    inbody = 1
    pending = ""
    next
}

END {
    print "FLAVOR " flavor
    if (inbody == 1) { print "UNTERMINATED " cur }
}
