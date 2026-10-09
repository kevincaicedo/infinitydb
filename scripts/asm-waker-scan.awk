# Waker-body scanner for check-waker-atomics.sh (ADR-0106 D8).
#
# Reads a rustc `--emit asm` file and prints, one fact per line:
#
#   FLAVOR <elf|macho|unknown>
#   VTABLE <sym>                     a fn pointer stored in a RawWakerVTable
#   BODY <sym> <start> <end> <instrs>
#   ATOMIC <sym> <line> <mnemonic>   an atomic instruction inside a body,
#                                    or the outline-atomics helper it calls
#   CALL <sym> <line> <target>       a direct call/branch leaving the body
#   INDIRECT <sym> <line> <call|jump>  an indirect branch inside a body
#
# Bodies are delimited structurally — the symbol label line, then
# `.cfi_startproc` … `.cfi_endproc` (emitted for both ELF and Mach-O), with
# ELF's `.size <sym>, …` as a fallback. Compiler-local labels (`.LBB…`,
# `.Ltmp…`, `.Lfunc_begin…`) are NOT boundaries: the old awk reset on them,
# and because `line-tables-only` debuginfo puts `.Lfunc_beginN:` on the
# FIRST line of every body, it scanned two lines and zero instructions per
# waker while printing OK (review 2026-08-30, F-L20-03).
#
# Local labels are ELF's `.L…` and Mach-O's `L…` (no dot: `Lfunc_begin0`,
# `LBB1_2`, `Ltmp3`). The batch-69 scanner knew only `.L`, so on Mach-O the
# DWARF `Lfunc_beginN:` right under every symbol became the body's owner
# and the vtable's four wakers had no body at all — "0 instruction lines
# scanned, 4 unresolved edges" on every Mach-O host (lane L11 N18, batch
# 70). The flavor is known before the first body: Mach-O opens with
# `.section __TEXT,…` / `.build_version`, ELF with `.text` and `@function`.
#
# Mnemonics are matched on the instruction's FIRST TWO fields, so an x86
# `lock` prefix — emitted as its own tab-separated field, `lock<TAB><TAB>
# cmpxchgq` — is caught. The old pattern anchored the whole set at the line
# start and therefore missed exactly the spelling a refcount CAS produces.
#
# Atomics are spelled per target, and each spelling is a row below (the
# gate is ADR-0106 D8's):
#   * x86_64: the `lock` prefix, `xchg` (implicitly locked), the fences.
#   * aarch64 Apple (cpu apple-m1, LSE and RCpc inline): `casal`, `ldaddal`,
#     and `stlur`/`ldapur` — a SeqCst store or Acquire load at a nonzero
#     offset (a task-header field) is `stlur x8, [x0, #8]`, not `stlr`.
#     `st<op>` is the architectural alias of `ld<op>` into the zero register.
#   * aarch64 Linux (`outline-atomics` on by default; LSE chosen at run
#     time): an RMW or CAS is a CALL to a compiler-rt helper, so no atomic
#     instruction is in the body at all — the probe's CAS and fetch-add went
#     unreported on the arm64 Linux leg. See is_outline_atomic.
#
# POSIX awk only (mawk, gawk, BSD awk): the CI matrix includes macOS.

function is_atomic(m) {
    return m ~ /^(lock|xacquire|xrelease|xchg[bwlq]?|cmpxchg([0-9]+b|[bwlq])?|xadd[bwlq]?|mfence|lfence|sfence)$/ ||
           m ~ /^(ldxr[bh]?|ldaxr[bh]?|ldxp|ldaxp|stxr[bh]?|stlxr[bh]?|stxp|stlxp|ldar[bh]?|stlr[bh]?|ldapr[bh]?)$/ ||
           m ~ /^(ldapur[bh]?|ldapurs[bhw]|stlur[bh]?)$/ ||
           m ~ /^(ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|swp|cas|casp)[abhl]*$/ ||
           m ~ /^st(add|clr|eor|set|smax|smin|umax|umin)l?[bh]?$/ ||
           m ~ /^(dmb|dsb|isb)$/
}

# A direct call or tail branch to an aarch64 outline-atomics helper. The set
# is exactly LLVM's OUTLINE_ATOMIC libcall table, the one compiler_builtins
# ships: cas{1,2,4,8,16} and swp/ldadd/ldclr/ldeor/ldset{1,2,4,8}, each
# _relax/_acq/_rel/_acq_rel — so `__aarch64_have_lse_atomics` and
# `__aarch64_sync_cache_range` are not atomics. Mach-O adds one `_`.
function is_outline_atomic(t) {
    if (flavor == "macho") sub(/^_/, "", t)
    return t ~ /^__aarch64_(cas(1|2|4|8|16)|(swp|ldadd|ldclr|ldeor|ldset)(1|2|4|8))_(relax|acq|rel|acq_rel)$/
}

function is_branch(m) {
    return m ~ /^(call|callq|calll|jmp|jmpq|bl|br|blr|b)$/
}

# A call returns to the body; a jump (a jump table, or a tail branch) does not.
function is_call(m) {
    return m ~ /^(call|callq|calll|bl|blr)$/
}

# A compiler-local label: never a body owner, never a call target.
function is_local(lbl) {
    if (flavor == "macho") return lbl ~ /^L/
    return lbl ~ /^\.L/
}

BEGIN { flavor = "unknown"; inbody = 0 }

# ---- flavor -------------------------------------------------------------
/,[[:space:]]*@function/ { if (flavor == "unknown") flavor = "elf" }
/^[[:space:]]*\.section[[:space:]]+__TEXT/ { flavor = "macho" }
/^[[:space:]]*\.build_version/ { flavor = "macho" }
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
        # AT&T `*` marks an indirect operand (`callq *8(%rax)`, `jmpq
        # *.LJTI0_0(,%rax,8)`); `*sym@GOTPCREL(%rip)` is a direct call
        # through the GOT, the spelling Rust's no-PLT default gives externs.
        star = (tgt ~ /^\*/)
        sub(/^\*/, "", tgt)
        if (tgt ~ /^[%]/ || tgt ~ /^[xw][0-9]+$/ || tgt == "" || (star && tgt !~ /@GOTPCREL\(%rip\)$/)) {
            print "INDIRECT " cur " " NR " " (is_call($1) ? "call" : "jump")
        }
        else if (!is_local(tgt)) {
            sub(/@.*$/, "", tgt)
            sub(/\(%rip\)$/, "", tgt)
            if (is_outline_atomic(tgt)) { print "ATOMIC " cur " " NR " " tgt }
            print "CALL " cur " " NR " " tgt
        }
    }
    next
}

# A bare symbol label at column 0 arms the next `.cfi_startproc`.
/^[A-Za-z_$.][A-Za-z0-9_$.]*:[[:space:]]*$/ {
    lbl = $0
    sub(/:[[:space:]]*$/, "", lbl)
    if (!is_local(lbl)) { pending = lbl; pline = NR }
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
