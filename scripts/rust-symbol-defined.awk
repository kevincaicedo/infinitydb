# Does ONE rustfmt-shaped Rust source file define `sym`? — ADR-0107 D2
# (first amendment): a `C` row's proof pointer `file.rs:Symbol` resolves
# to a definition in production code, or the gate is red. Usage:
#
#   awk -v sym='Type::method' -f rust-symbol-defined.awk file.rs
#   awk -v sym='free_fn'      -f rust-symbol-defined.awk file.rs
#
# Prints `<line>\t<container-chain>\t<item>` for the first definition and
# exits 0; exits 1 (printing nothing) when the file defines no such
# symbol. Feed it a strip-test-modules.awk'd file so a symbol that exists
# only under `#[cfg(test)]` does not count as a proof.
#
# Grammar resolved:
#   `name`            a free item — fn, const, static, struct, enum,
#                     union, trait, type, mod, macro_rules! — at any
#                     nesting
#   `Type::name`      an item (fn, const, associated type/const) whose
#                     innermost enclosing container is an `impl` block
#                     whose self type is `Type` (`impl Type`, `impl<T>
#                     Type<T>`, `impl Trait for Type<..>` — the type after
#                     `for` wins), a `trait Type` block (required or
#                     default methods), or a `mod Type` block
#   `A::B::name`      the same, with every earlier segment matching the
#                     enclosing containers outward (suffix match)
#
# Containers are tracked by rustfmt's shape: a header (`impl` / `trait` /
# `mod`, possibly `pub`/`unsafe`-prefixed, its generics and `where`
# clause joined until the opening `{`) opens at indent N and closes at
# the first `}` at exactly indent N. Full-line comments and doc comments
# never match. Line numbers are the input's (a stripped file keeps its
# numbering).

function indent_of(s,    m) { m = s; sub(/[^ ].*$/, "", m); return length(m) }

# Self type of an impl/trait/mod header (joined, whitespace-normalized).
function header_self(h,    s, depth, i, c, n, rest) {
    s = h
    sub(/^[[:space:]]*/, "", s)
    sub(/^pub(\([^)]*\))?[[:space:]]+/, "", s)
    sub(/^unsafe[[:space:]]+/, "", s)
    if (s ~ /^impl([[:space:]]|<)/) {
        sub(/^impl/, "", s)
        # skip the generic parameter list, balanced
        if (s ~ /^</) {
            depth = 0; n = length(s)
            for (i = 1; i <= n; i++) {
                c = substr(s, i, 1)
                if (c == "<") depth++
                else if (c == ">") { depth--; if (depth == 0) break }
            }
            s = substr(s, i + 1)
        }
        sub(/^[[:space:]]*/, "", s)
        # `Trait for Type` — the type after `for`; else the type itself.
        # Match ` for ` at the top level only (generic args never contain
        # a bare ` for `).
        if (match(s, /[[:space:]]for[[:space:]]/)) {
            rest = substr(s, RSTART + RLENGTH)
            s = rest
        }
        sub(/^[[:space:]]*/, "", s)
        sub(/^&(mut[[:space:]]+)?/, "", s)   # impl Trait for &T
        sub(/^\(?/, "", s)                   # tuple/paren self types: first segment
        # last path segment before generics: `crate::a::Type<..>` -> Type
        sub(/[[:space:]<({].*$/, "", s)
        sub(/^.*::/, "", s)
        return s
    }
    if (s ~ /^(trait|mod)[[:space:]]/) {
        sub(/^(trait|mod)[[:space:]]+/, "", s)
        sub(/[^A-Za-z0-9_].*$/, "", s)
        return s
    }
    return ""
}

# Name of an item definition on a (single) line, or "".
function item_name(line,    s) {
    s = line
    sub(/^[[:space:]]*/, "", s)
    sub(/^pub(\([^)]*\))?[[:space:]]+/, "", s)
    while (s ~ /^(const|unsafe|async|extern[[:space:]]+"[^"]*"|default)[[:space:]]+fn[[:space:]]/ ||
           s ~ /^(const|unsafe|async|default)[[:space:]]+(const|unsafe|async|extern|default)[[:space:]]/) {
        sub(/^(const|unsafe|async|default)[[:space:]]+/, "", s)
        sub(/^extern[[:space:]]+"[^"]*"[[:space:]]+/, "", s)
    }
    if (s ~ /^fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/) {
        sub(/^fn[[:space:]]+/, "", s); sub(/[^A-Za-z0-9_].*$/, "", s); return s
    }
    if (s ~ /^(const|static)[[:space:]]+(mut[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:/) {
        sub(/^(const|static)[[:space:]]+(mut[[:space:]]+)?/, "", s); sub(/[^A-Za-z0-9_].*$/, "", s); return s
    }
    if (s ~ /^(struct|enum|union|trait|type)[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/) {
        sub(/^(struct|enum|union|trait|type)[[:space:]]+/, "", s); sub(/[^A-Za-z0-9_].*$/, "", s); return s
    }
    if (s ~ /^mod[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/) {
        sub(/^mod[[:space:]]+/, "", s); sub(/[^A-Za-z0-9_].*$/, "", s); return s
    }
    if (s ~ /^macro_rules![[:space:]]+[A-Za-z_][A-Za-z0-9_]*/) {
        sub(/^macro_rules![[:space:]]+/, "", s); sub(/[^A-Za-z0-9_].*$/, "", s); return s
    }
    return ""
}

# Does the container stack (outermost..innermost) end with `want[1..nw]`?
function chain_matches(nw,    i) {
    if (nw > sp) return 0
    for (i = 0; i < nw; i++) {
        if (stack[sp - i] != want[nw - i]) return 0
    }
    return 1
}

function chain_text(    i, t) {
    t = ""
    for (i = 1; i <= sp; i++) t = t (i > 1 ? "::" : "") stack[i]
    return t
}

BEGIN {
    if (sym == "") { print "rust-symbol-defined: -v sym= is required" > "/dev/stderr"; exit 2 }
    nseg = split(sym, seg, "::")
    item = seg[nseg]
    nwant = nseg - 1
    for (i = 1; i <= nwant; i++) want[i] = seg[i]
    sp = 0
    inheader = 0
    found = 0
}

{
    line = $0
    if (found) next
    if (inheader) {
        header = header " " line
        if (line ~ /\{[[:space:]]*$/ || line ~ /\{[[:space:]]*\}[[:space:]]*$/) {
            inheader = 0
            if (header !~ /\{[[:space:]]*\}[[:space:]]*$/) {
                sp++; stack[sp] = header_self(header); ind[sp] = hindent
            }
        }
        next
    }
    if (line ~ /^[[:space:]]*\/\//) next
    # close the innermost container at its own indent
    if (sp > 0 && line ~ /^[[:space:]]*\}/ && indent_of(line) == ind[sp]) {
        sp--
        next
    }
    # a container header?
    if (line ~ /^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?(unsafe[[:space:]]+)?(impl([[:space:]]|<)|trait[[:space:]]|mod[[:space:]])/ &&
        line !~ /;[[:space:]]*$/) {
        hindent = indent_of(line)
        if (line ~ /\{[[:space:]]*\}[[:space:]]*$/) {
            # `impl X for Y {}` — empty, nothing to enter
            name = header_self(line)
            if (name == item && chain_matches(nwant)) { found = 1; print NR "\t" chain_text() "\t" name; exit 0 }
            next
        }
        if (line ~ /\{.*\}[[:space:]]*$/) {
            # a one-line container with a body (`trait T { fn a(&self); }`):
            # enter it for this line only and scan the body's items
            head = line; sub(/\{.*$/, "", head)
            name = header_self(head)
            if (line ~ /^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?(trait|mod)[[:space:]]/ &&
                name == item && chain_matches(nwant)) { found = 1; print NR "\t" chain_text() "\t" name; exit 0 }
            body = line; sub(/^[^{]*\{/, "", body)
            sp++; stack[sp] = name; ind[sp] = hindent
            nparts = split(body, parts, /[;{}]/)
            for (pi = 1; pi <= nparts; pi++) {
                nm = item_name(parts[pi])
                if (nm != "" && nm == item && chain_matches(nwant)) { found = 1; print NR "\t" chain_text() "\t" nm; exit 0 }
            }
            sp--
            next
        }
        if (line ~ /\{[[:space:]]*$/) {
            name = header_self(line)
            # `mod Name {` / `trait Name {` are themselves items
            if (line ~ /^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?(trait|mod)[[:space:]]/ &&
                name == item && chain_matches(nwant)) { found = 1; print NR "\t" chain_text() "\t" name; exit 0 }
            sp++; stack[sp] = name; ind[sp] = hindent
        } else {
            name = header_self(line)
            if (line ~ /^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?(trait|mod)[[:space:]]/ &&
                name == item && chain_matches(nwant)) { found = 1; print NR "\t" chain_text() "\t" name; exit 0 }
            header = line; inheader = 1
        }
        next
    }
    name = item_name(line)
    if (name != "" && name == item && chain_matches(nwant)) {
        found = 1
        print NR "\t" chain_text() "\t" name
        exit 0
    }
}

END {
    if (!found) exit 1
}
