# Release-panic census over ONE production source file (post
# strip-test-modules.awk) — ADR-0107 D2. Emits one row per site:
#   kind <TAB> message            (default)
#   kind <TAB> line <TAB> message (-v lines=1 — the audit listing)
# `kind` ∈ assert, assert_eq, assert_ne, expect, panic, unreachable
# (`debug_*` never counts: absent from release builds). `message` is the
# first string literal of the call, or `<no message> <call text>` when the
# call has none; multi-line calls are joined until their parentheses
# balance. Full-line comments never match. The identity a site keeps is
# (file, kind, message) — line numbers are disclosure only.
function flush(   s, lit, txt) {
    s = buf
    if (match(s, /"([^"\\]|\\.)*"/)) {
        lit = substr(s, RSTART + 1, RLENGTH - 2)
    } else {
        lit = ""
    }
    txt = s
    gsub(/[[:space:]]+/, " ", txt)
    if (lit == "") { lit = "<no message> " substr(txt, 1, 120) }
    if (lines) { print kind "\t" startline "\t" lit } else { print kind "\t" lit }
    buf = ""; kind = ""; depth = 0; active = 0
}
{
    line = $0
    if (!active) {
        if (line ~ /^[[:space:]]*\/\//) next
        if (match(line, /(^|[^_a-zA-Z0-9])(assert|assert_eq|assert_ne|panic|unreachable)!\(/) || match(line, /\.expect\(/)) {
            pre = substr(line, 1, RSTART - 1)
            if (pre ~ /debug_$/) next
            seg = substr(line, RSTART)
            if (seg ~ /^\.expect\(/) { kind = "expect" }
            else { kind = seg; sub(/^[^a-z]*/, "", kind); sub(/!\(.*$/, "", kind) }
            startline = NR
            buf = seg
            depth = 0; active = 1
            n = split(seg, ch, "")
            for (i = 1; i <= n; i++) { if (ch[i] == "(") depth++; else if (ch[i] == ")") depth-- }
            if (depth <= 0) { flush() }
        }
    } else {
        buf = buf " " line
        n = split(line, ch, "")
        for (i = 1; i <= n; i++) { if (ch[i] == "(") depth++; else if (ch[i] == ")") depth-- }
        if (depth <= 0) { flush() }
    }
}
