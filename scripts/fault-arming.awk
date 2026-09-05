# Fault-point reference classifier for check-fault-points.sh (ADR-0106 D9).
#
# The old gate's "exercised by a test" arm was `grep -e fault::CONST -e
# "point"` over the tests trees — satisfied by a `//!` doc line and by an
# assert's *message string*. `shadow_twin_read_fail` passed on exactly
# that: zero arming call sites, two matches, both prose (review 2026-08-30,
# F-L20-04). Deleting the whole `#[test]` left the gate green.
#
# This pass reports only references that sit in an *arming* or a *firing*
# statement, after removing comments and after collapsing string literals
# that are not themselves a bare point name:
#
#   ARM  <line> <point>   the reference is an argument of `fault::arm(`
#                         or shares a statement with a `FaultSpec::` row
#                         (the `vec![(CONST, FaultSpec::Nth(n))]` plan the
#                         node harnesses hand to `start_node`)
#   FIRE <line> <point>   the reference is an argument of `fault::fire(`
#
# A reference is a `fault::SCREAMING_CONST` path or a `"bare_literal"`.
# Statements are approximated by a ±2-line window: rustfmt splits
# `fault::arm(` / const / `FaultSpec::` across at most three lines in this
# tree, and the window is disclosed in the gate's scope line.
#
# POSIX awk only (mawk, gawk, BSD awk): the CI matrix includes macOS.

function strip_comments(line,   at, q, p, pre, cnt) {
    gsub(/\\./, "@@", line)
    if (inblock) {
        at = index(line, "*/")
        if (at == 0) { return "" }
        inblock = 0
        line = substr(line, at + 2)
    }
    at = index(line, "/*")
    if (at > 0) {
        pre = substr(line, 1, at - 1)
        cnt = gsub(/"/, "\"", pre)
        if (cnt % 2 == 0) {
            q = index(substr(line, at), "*/")
            if (q > 0) { line = substr(line, 1, at - 1) substr(line, at + q + 1) }
            else { inblock = 1; line = substr(line, 1, at - 1) }
        }
    }
    p = 0
    while ((q = index(substr(line, p + 1), "//")) > 0) {
        at = p + q
        pre = substr(line, 1, at - 1)
        cnt = gsub(/"/, "\"", pre)
        if (cnt % 2 == 0) { return substr(line, 1, at - 1) }
        p = at + 1
    }
    return line
}

# Collapse every string literal to "" except a bare point name, which the
# harnesses do arm by literal (`fault::arm("blob_short_write", …)`).
function collapse(line,   out, rest, body) {
    out = ""
    rest = line
    while (match(rest, /"[^"]*"/)) {
        out = out substr(rest, 1, RSTART - 1)
        body = substr(rest, RSTART + 1, RLENGTH - 2)
        if (body ~ /^[a-z][a-z0-9_]*$/) { out = out "\"" body "\"" } else { out = out "\"\"" }
        rest = substr(rest, RSTART + RLENGTH)
    }
    return out rest
}

BEGIN { inblock = 0; n = 0 }

{ n++; norm[n] = collapse(strip_comments($0)) }

END {
    for (i = 1; i <= n; i++) {
        win = ""
        for (j = (i - 2 < 1 ? 1 : i - 2); j <= (i + 2 > n ? n : i + 2); j++) { win = win " " norm[j] }
        arming = (win ~ /fault::arm\(/ || win ~ /FaultSpec::/)
        firing = (win ~ /fault::fire\(/)
        if (!arming && !firing) { continue }
        rest = norm[i]
        while (match(rest, /fault::[A-Z][A-Z0-9_]*/)) {
            tok = substr(rest, RSTART + 7, RLENGTH - 7)
            rest = substr(rest, RSTART + RLENGTH)
            if (tok == "ALL" || tok == "COMPILED_IN") { continue }
            lower = tolower(tok)
            if (firing) { print "FIRE " i " " lower }
            if (arming) { print "ARM " i " " lower }
        }
        rest = norm[i]
        while (match(rest, /"[a-z][a-z0-9_]*"/)) {
            tok = substr(rest, RSTART + 1, RLENGTH - 2)
            rest = substr(rest, RSTART + RLENGTH)
            if (firing) { print "FIRE " i " " tok }
            if (arming) { print "ARM " i " " tok }
        }
    }
}
