# Blanks Rust comment text, preserving line numbers (ADR-0106 D10).
#
# `check-fsync-fail-stop.sh` must not count a doc line that *names* an
# fsync error type as a site that handles one: 11 of the 41 raw grep hits
# in this tree are prose. Line comments (`//`, `///`, `//!`) are cut from
# the `//` to end of line when the `//` is not inside a string literal;
# block comments are cut across lines. String literals are left intact —
# the caller's patterns are type paths, not text.
#
# `-v mode=files` (ADR-0203 D3) reads many files in one pass, for a gate
# whose scope is the whole tree. Each output line is `<file>\t<line>\t<text>`;
# the block state resets at each file's first line; the first comment opener
# outside a string wins, so a `//` before a `/*` cuts the line there and opens
# no block; a block comment is read as one space; and a file that ends inside
# a block comment is reported as `<file>\topen-block\t<the line it opened on>`,
# since the blanker would otherwise drop the rest of that file unseen. With no
# mode the output is the one-file form above, unchanged.
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

# The files mode's one line: the earliest `//` or `/*` with an even count of
# quotes before it on its segment is the opener; a closed block reads as one
# space and the segment after it is read the same way.
function strip_first(line,   out, at, p, q, pre, two) {
    gsub(/\\./, "@@", line)
    out = ""
    while (1) {
        if (inblock) {
            at = index(line, "*/")
            if (at == 0) { return out }
            inblock = 0
            out = out " "
            line = substr(line, at + 2)
        }
        p = 0
        two = ""
        while ((q = index(substr(line, p + 1), "/")) > 0) {
            at = p + q
            two = substr(line, at, 2)
            if (two == "//" || two == "/*") {
                pre = substr(line, 1, at - 1)
                if (gsub(/"/, "\"", pre) % 2 == 0) { break }
            }
            two = ""
            p = at
        }
        if (two == "") { return out line }
        out = out substr(line, 1, at - 1)
        if (two == "//") { return out }
        inblock = 1
        opened = FNR
        line = substr(line, at + 2)
    }
}

BEGIN { inblock = 0; file = "" }
mode != "files" { print strip($0) }
mode == "files" && FNR == 1 {
    if (inblock && file != "") { print file "\topen-block\t" opened }
    inblock = 0
}
mode == "files" {
    file = FILENAME
    print FILENAME "\t" FNR "\t" strip_first($0)
}
END {
    if (mode == "files" && inblock && file != "") { print file "\topen-block\t" opened }
}
