# Blanks Rust comment text, preserving line numbers (ADR-0106 D10).
#
# `check-fsync-fail-stop.sh` must not count a doc line that *names* an
# fsync error type as a site that handles one: 11 of the 41 raw grep hits
# in this tree are prose. Line comments (`//`, `///`, `//!`) are cut from
# the `//` to end of line when the `//` is not inside a string literal;
# block comments are cut across lines. String literals are left intact —
# the caller's patterns are type paths, not text.
#
# POSIX awk only (mawk, gawk, BSD awk): the CI matrix includes macOS.

function strip(line,   at, q, p, pre, cnt) {
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
            if (q > 0) { return substr(line, 1, at - 1) strip(substr(line, at + q + 1)) }
            inblock = 1
            return substr(line, 1, at - 1)
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

BEGIN { inblock = 0 }
{ print strip($0) }
