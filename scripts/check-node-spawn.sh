#!/usr/bin/env bash
# Node-spawn gate (ADR-0203 D3): a test starts `infinityd` only through the
# node module, bins/infinityd/tests/support/node.rs. The module's types give
# every node it starts the box's node lock, held by the node itself from
# before its first ring to its exit, and end the node with its handle. No
# type refuses a string, so this gate holds the other half: outside the
# module no file names the binary. Four rules, read on each scanned line with
# its comments blanked, each red with its file and line:
#   1. the token BIN_EXE_infinityd, in cargo's spelling and in nextest's
#      (CARGO_BIN_EXE_infinityd, NEXTEST_BIN_EXE_infinityd);
#   2. the literal "INFINITYD_BIN", except as the first argument of `.env(`
#      or `.env_remove(`, which hand the variable to a child; a literal that
#      opens a line whose previous code line ends in `.env(` or
#      `.env_remove(` is that first argument (rustfmt's split call);
#   3. a string literal equal to `infinityd` or ending in `/infinityd`, under
#      any prefix (b, c, r, br, cr, any number of #s);
#   4. the token XDG_RUNTIME_DIR or env_clear, outside the module and its own
#      test, bins/infinityd/tests/node_lock.rs: either moves the lock's
#      directory, so a node would hold another lock than the box's.
# What no token shows is review's: a name built from pieces, the variable's
# name built at run time, an escape inside the literal, the name inside a
# longer string (`sh -c "exec infinityd"`), `env -i` in a shell string.
#
# Scope: every *.rs file git lists, tracked or untracked and not ignored, so
# a new test is read before it is added, but the campaign tools' sources
# (bins/inf-bench/src, bins/inf-compare/src), which start their own nodes.
# Each `#[path = "..."]` target of a scanned file is scanned too, resolved
# against the including file's directory, transitively and even under an
# excluded directory: a campaign source a test includes is that test's code.
# A listed path gone from the work tree is skipped and counted. Comments are
# blanked by rust-strip-comments.awk in its files mode, one pass a round of
# files, which leaves string literals intact; markers are read from the raw
# line. A scope error exits 2 (ADR-0106 D1): a root that is not its own git
# work tree, a missing module, module test or excluded directory, an empty
# scan, a path holding a tab or a newline, a `#[path]` target that does not
# exist where it resolves or resolves outside the root, and a file that ends
# inside a block comment, whose rest the blanker would drop unseen.
#
# A legitimate use carries `node-spawn-allow: <reason>` on its line or the
# line above (ADR-0106 D4) and is listed on the OK line; a bare marker is
# red, and so is a marker that marks no hit.
#
# The ring census (ADR-0203 D3): io_uring rings that belong to no node take
# no lock, and memlock_boot's budget counts them. Every scanned file that
# names UringDriver::new, the one ring constructor, outside a comment is a
# row of CENSUS below: its class (test: a test runner starts it; bench: no
# test runner selects it; node: infinityd itself, whose rings take the lock
# through its start), its count of UringDriver::new and its count of
# `.register_pool(` and `.register_tier_pool(` calls. A file with no row, a
# row whose file names neither token, and a count that moved are red, naming
# the row to write. A new or larger test row is a ring beside memlock_boot
# outside the lock: its pages are counted against that test's bound when it
# lands. `--census` prints the rows (class, file, rings, registrations) and
# exits; the test rows are the list of ring holders outside the lock.
#
# Portable bash 3.2 and POSIX awk (ADR-0106 D18): the CI matrix includes
# macOS. INF_CHECK_ROOT names another checkout (the self-test's fixtures).
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
BLANK="$SCRIPT_DIR/rust-strip-comments.awk"
MODULE=bins/infinityd/tests/support/node.rs
MODULE_TEST=bins/infinityd/tests/node_lock.rs
EXCLUDED="bins/inf-bench/src bins/inf-compare/src"
TAB=$(printf '\t')

# file|class|UringDriver::new count|.register_pool( and .register_tier_pool( count
IFS= read -r -d '' CENSUS <<'EOF' || true
bins/infinityd/src/main.rs|node|1|1
crates/inf-runtime/benches/cold_shaping/linux.rs|bench|2|2
crates/inf-runtime/benches/log_fsync.rs|bench|1|0
crates/inf-runtime/benches/log_io_tier.rs|bench|2|0
crates/inf-runtime/benches/tier_io_mode/linux.rs|bench|1|1
crates/inf-runtime/benches/uring_echo.rs|bench|1|1
crates/inf-server/benches/parallel_boot/linux.rs|bench|1|1
crates/inf-runtime/tests/accept_exhaustion.rs|test|1|0
crates/inf-runtime/tests/close_fd_reuse.rs|test|1|0
crates/inf-runtime/tests/cold_hardened.rs|test|1|1
crates/inf-runtime/tests/driver_conformance.rs|test|1|0
crates/inf-runtime/tests/file_ops.rs|test|1|0
crates/inf-runtime/tests/steel_thread.rs|test|1|0
crates/inf-server/tests/node_e2e.rs|test|1|0
tests/crash-matrix/tests/fsyncgate.rs|test|1|0
EOF

case ${1:-} in
"") ;;
--census)
    printf '%s' "$CENSUS" | awk -F '|' 'NF { print $2 "\t" $1 "\t" $3 "\t" $4 }'
    exit 0
    ;;
*) echo "usage: $0 [--census]" >&2; exit 2 ;;
esac

scope_error() {
    echo "NODE-SPAWN SCOPE ERROR: $1"
    exit 2
}

cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
top=$(git rev-parse --show-toplevel 2>/dev/null) ||
    scope_error "$(pwd -P) is not inside a git work tree; the scope is what git lists"
[ "$top" = "$(pwd -P)" ] || scope_error "$(pwd -P) is not its own git work tree (its top is $top)"
[ -f "$MODULE" ] || scope_error "the module $MODULE is missing: there is no sanctioned start path"
[ -f "$MODULE_TEST" ] ||
    scope_error "the module's test $MODULE_TEST, rule 4's one exemption, is missing"
for dir in $EXCLUDED; do
    [ -d "$dir" ] || scope_error "the excluded directory $dir is missing: a stale exclusion"
done

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "node-spawn: mktemp failed" >&2; exit 2; }
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

git ls-files -z --cached --others --exclude-standard -- '*.rs' >"$work/listed.z" ||
    scope_error "git ls-files failed in $(pwd -P)"
# One path a line: a path holding a newline or a tab cannot be one record.
: >"$work/listed"
while IFS= read -r -d '' path; do
    case $path in
    *"$TAB"* | *'
'*) scope_error "a listed path holds a tab or a newline: $(printf '%q' "$path")" ;;
    esac
    printf '%s\n' "$path" >>"$work/listed"
done <"$work/listed.z"
LC_ALL=C sort -u "$work/listed" >"$work/listed.sorted"

# Partition: excluded, gone from the work tree, scanned.
: >"$work/round"
: >"$work/excluded"
gone=0
listed=0
while IFS= read -r path; do
    skip=0
    for dir in $EXCLUDED; do
        case $path in "$dir"/*) skip=1 ;; esac
    done
    if [ "$skip" -eq 1 ]; then
        printf '%s\n' "$path" >>"$work/excluded"
    elif [ ! -f "$path" ]; then
        gone=$((gone + 1))
    else
        listed=$((listed + 1))
        printf '%s\n' "$path" >>"$work/round"
    fi
done <"$work/listed.sorted"
[ "$listed" -gt 0 ] || scope_error "no *.rs file to scan in $(pwd -P): an empty scan is no verdict"

# The `#[path]` targets a round's blanked lines name: includer, line, the
# target resolved against the includer's directory ("/" outside the root:
# a resolved path never starts with one, and an empty field would vanish
# under a tab IFS).
AWK_PATHS='
function norm(path,   n, parts, i, k, stack, out) {
    if (substr(path, 1, 1) == "/") { return "/" }
    n = split(path, parts, "/")
    k = 0
    for (i = 1; i <= n; i++) {
        if (parts[i] == "" || parts[i] == ".") { continue }
        if (parts[i] == "..") {
            if (k == 0) { return "/" }
            k--
            continue
        }
        stack[++k] = parts[i]
    }
    out = ""
    for (i = 1; i <= k; i++) { out = out (i > 1 ? "/" : "") stack[i] }
    return out
}
{
    p = index($0, "\t"); file = substr($0, 1, p - 1); rest = substr($0, p + 1)
    q = index(rest, "\t"); line = substr(rest, 1, q - 1); text = substr(rest, q + 1)
    if (line == "open-block") { next }
    while (match(text, "#[[][[:space:]]*path[[:space:]]*=[[:space:]]*\"[^\"]*\"[[:space:]]*[]]")) {
        attr = substr(text, RSTART, RLENGTH)
        text = substr(text, RSTART + RLENGTH)
        a = index(attr, "\""); target = substr(attr, a + 1)
        target = substr(target, 1, index(target, "\"") - 1)
        dir = file
        if (sub("/[^/]*$", "", dir) == 0) { dir = "" }
        print file "\t" line "\t" norm(dir == "" ? target : dir "/" target) "\t" target
    }
}'

# Rounds: blank the round's files in one pass, then queue each `#[path]`
# target not yet scanned. Each scanned file is in exactly one round.
: >"$work/blanked"
: >"$work/includes"
: >"$work/reached"
cp "$work/round" "$work/seen"
while [ -s "$work/round" ]; do
    files=()
    while IFS= read -r path; do files[${#files[@]}]=$path; done <"$work/round"
    awk -v mode=files -f "$BLANK" "${files[@]}" >"$work/round.blanked"
    cat "$work/round.blanked" >>"$work/blanked"
    awk "$AWK_PATHS" "$work/round.blanked" >"$work/round.includes"
    cat "$work/round.includes" >>"$work/includes"
    : >"$work/next"
    while IFS="$TAB" read -r includer line target written; do
        [ "$target" != / ] ||
            scope_error "$includer:$line: #[path = \"$written\"] resolves outside $(pwd -P)"
        [ -f "$target" ] ||
            scope_error "$includer:$line: #[path = \"$written\"] names $target, which does not exist"
        if ! grep -qxF -- "$target" "$work/seen" "$work/next"; then
            printf '%s\n' "$target" >>"$work/next"
        fi
    done <"$work/round.includes"
    cat "$work/next" >>"$work/seen"
    cat "$work/next" >>"$work/reached"
    mv "$work/next" "$work/round"
done

# Markers, from the raw lines of every scanned file.
files=()
while IFS= read -r path; do files[${#files[@]}]=$path; done <"$work/seen"
awk 'index($0, "node-spawn-allow") { print FILENAME "\t" FNR "\t" $0 }' "${files[@]}" \
    >"$work/markers"
printf '%s' "$CENSUS" >"$work/census"

# The verdict: one record a line, `red <text>`, `scope <text>`, `allowed
# <text>` or `count <name> <value>`.
AWK_RULES='
function trim(s) { sub("^[[:space:]]+", "", s); sub("[[:space:]]+$", "", s); return s }
function ends_env(s) {
    s = trim(s)
    return (length(s) >= 5 && substr(s, length(s) - 4) == ".env(") ||
        (length(s) >= 12 && substr(s, length(s) - 11) == ".env_remove(")
}
function count(s, re,   n) {
    n = 0
    while (match(s, re)) { n++; s = substr(s, RSTART + RLENGTH - 1) }
    return n
}
function call_for(f) {
    if (index(f, "bins/infinityd/") == 1) { return "write NodeBinary::built().command()...spawn()" }
    if (index(f, "tests/compat/") == 1) { return "start nodes through compat::harness" }
    return "write NodeBinary::from_infinityd_bin(), or NodeBinary::built_or(<var>) with a" \
        " variable other than INFINITYD_BIN that the package recipe sets"
}
function hit(f, l, rule) {
    if (!((f SUBSEP l SUBSEP rule) in seen_hit)) {
        seen_hit[f SUBSEP l SUBSEP rule] = 1
        hits++
        hit_file[hits] = f; hit_line[hits] = l; hit_rule[hits] = rule
    }
}
FILENAME == includesf {
    split($0, field, "\t")
    if (!(field[3] in includer)) { includer[field[3]] = field[1] ":" field[2] }
    next
}
FILENAME == reachedf { reached[$0] = 1; next }
FILENAME == markersf {
    p = index($0, "\t"); f = substr($0, 1, p - 1); rest = substr($0, p + 1)
    q = index(rest, "\t"); l = substr(rest, 1, q - 1); raw = substr(rest, q + 1)
    m = index(raw, "node-spawn-allow")
    after = substr(raw, m + length("node-spawn-allow"))
    reason = ""
    if (substr(after, 1, 1) == ":") { reason = trim(substr(after, 2)) }
    sub("[[:space:]]*[*]/.*$", "", reason)
    marker[f SUBSEP l] = reason
    markers++; marker_file[markers] = f; marker_line[markers] = l
    next
}
FILENAME == censusf {
    if ($0 == "") { next }
    n = split($0, field, "|")
    rows++
    if (n != 4 || field[2] !~ "^(test|bench|node)$" || field[3] !~ "^[0-9]+$" ||
        field[4] !~ "^[0-9]+$") {
        print "red ring census: row " rows " is not file|test, bench or node|count|count: " $0
        next
    }
    if (field[1] in row_class) { print "red ring census: " field[1] " has two rows"; next }
    row_class[field[1]] = field[2]; row_rings[field[1]] = field[3]; row_regs[field[1]] = field[4]
    row_order[rows] = field[1]
    class_rows[field[2]]++
    next
}
{
    p = index($0, "\t"); f = substr($0, 1, p - 1); rest = substr($0, p + 1)
    q = index(rest, "\t"); l = substr(rest, 1, q - 1); text = substr(rest, q + 1)
    if (l == "open-block") {
        print "scope " f " ends inside the block comment opened at its line " text \
            ": the blanker would drop the rest unseen"
        next
    }
    if (f != current) { current = f; prev_code = "" }
    lines++
    pad = " " text " "
    rings[f] += count(pad, "[^A-Za-z0-9_]UringDriver::new[^A-Za-z0-9_]")
    regs[f] += count(text, "[.]register_(tier_)?pool[(]")
    if (f != module) {
        if (match(pad, "BIN_EXE_infinityd[^A-Za-z0-9_]")) { hit(f, l, 1) }
        rest = text
        while ((at = index(rest, "\"INFINITYD_BIN\"")) > 0) {
            before = substr(rest, 1, at - 1)
            split_call = trim(before) == "" && rest == text && ends_env(prev_code)
            if (!(ends_env(before) || split_call)) { hit(f, l, 2) }
            rest = substr(rest, at + 1)
        }
        if (match(text, "\"([^\"]*/)?infinityd\"")) { hit(f, l, 3) }
        if (f != module_test &&
            match(pad, "[^A-Za-z0-9_](XDG_RUNTIME_DIR|env_clear)[^A-Za-z0-9_]")) {
            hit(f, l, 4)
        }
    }
    if (trim(text) != "") { prev_code = text }
}
END {
    rule_text[1] = "the binary'\''s cargo or nextest variable (BIN_EXE_infinityd)"
    rule_text[2] = "the literal \"INFINITYD_BIN\" outside the first argument of .env( or .env_remove("
    rule_text[3] = "a string literal naming the binary (infinityd, or a path ending in /infinityd)"
    rule_text[4] = "XDG_RUNTIME_DIR or env_clear, which move the node lock'\''s directory"
    marked = 0
    for (i = 1; i <= hits; i++) {
        f = hit_file[i]; l = hit_line[i]
        if (((f SUBSEP l) in marker) && marker[f SUBSEP l] != "") {
            bound[f SUBSEP l] = 1; marked++; continue
        }
        if (((f SUBSEP (l - 1)) in marker) && marker[f SUBSEP (l - 1)] != "") {
            bound[f SUBSEP (l - 1)] = 1; marked++; continue
        }
        # A #[path] target names its includer; for one only a #[path]
        # reaches, the call to write is the package of the includer.
        where = f ":" l
        owner = f
        if (f in includer) {
            where = where " (included by " includer[f] " through #[path])"
        }
        if ((f in reached) && (f in includer)) {
            owner = includer[f]
            sub(":[0-9]+$", "", owner)
        }
        fix = call_for(owner)
        if (hit_rule[i] == 4) { fix = "leave the lock directory as the test runner set it" }
        print "red " where ": rule " hit_rule[i] ", " rule_text[hit_rule[i]] \
            "; a test starts infinityd only through " module ": " fix \
            ", or mark a legitimate use node-spawn-allow: <reason> on its line or the line above"
    }
    for (i = 1; i <= markers; i++) {
        f = marker_file[i]; l = marker_line[i]
        if (marker[f SUBSEP l] == "") {
            print "red " f ":" l ": a bare node-spawn-allow marker; write node-spawn-allow: <reason>"
        } else if (!((f SUBSEP l) in bound)) {
            print "red " f ":" l ": node-spawn-allow marks no hit on its line or the next; remove it"
        } else {
            print "allowed " f ":" l ": " marker[f SUBSEP l]
        }
    }
    for (f in rings) {
        if (rings[f] > 0 && !(f in row_class)) {
            print "red ring census: " f " names UringDriver::new " rings[f] " time(s), outside" \
                " the table; add the row " f "|<test, bench or node>|" rings[f] "|" regs[f]
        }
    }
    for (i = 1; i <= rows; i++) {
        f = row_order[i]
        if (f == "") { continue }
        if (!(f in rings)) {
            print "red ring census: the row " f " names no scanned file; remove it"
        } else if (rings[f] + regs[f] == 0) {
            print "red ring census: " f " names neither UringDriver::new nor a pool" \
                " registration; remove its row"
        } else if (rings[f] != row_rings[f] || regs[f] != row_regs[f]) {
            print "red ring census: " f " names UringDriver::new " rings[f] " time(s) and " \
                regs[f] " registration(s), its row " row_rings[f] " and " row_regs[f] \
                "; write " f "|" row_class[f] "|" rings[f] "|" regs[f]
        }
    }
    print "count lines " lines + 0
    print "count hits " hits + 0
    print "count marked " marked
    print "count census " (class_rows["test"] + 0) " test, " (class_rows["bench"] + 0) \
        " bench, " (class_rows["node"] + 0) " node"
}'

awk -v module="$MODULE" -v module_test="$MODULE_TEST" \
    -v includesf="$work/includes" -v reachedf="$work/reached" -v markersf="$work/markers" \
    -v censusf="$work/census" "$AWK_RULES" \
    "$work/includes" "$work/reached" "$work/markers" "$work/census" "$work/blanked" \
    >"$work/verdict"

if grep -q '^scope ' "$work/verdict"; then
    sed -n 's/^scope /NODE-SPAWN SCOPE ERROR: /p' "$work/verdict"
    exit 2
fi
value() { sed -n "s/^count $1 //p" "$work/verdict"; }
scanned=$(wc -l <"$work/seen" | tr -d ' ')
[ "$(value lines)" -gt 0 ] || scope_error "the blanker read no line: an empty scan is no verdict"
includes=$(wc -l <"$work/includes" | tr -d ' ')
reached=$(wc -l <"$work/reached" | tr -d ' ')
excluded=$(wc -l <"$work/excluded" | tr -d ' ')
reached_excluded=0
while IFS= read -r path; do
    for dir in $EXCLUDED; do
        case $path in "$dir"/*) reached_excluded=$((reached_excluded + 1)) ;; esac
    done
done <"$work/reached"
reds=$(grep -c '^red ' "$work/verdict" || true)
if [ "$reds" -gt 0 ]; then
    sed -n 's/^red /NODE-SPAWN violation: /p' "$work/verdict"
    echo "node-spawn FAILED: $reds violation(s) in $scanned files"
    exit 1
fi
echo "node-spawn OK: $scanned files, $(value lines) lines scanned ($listed listed, $reached" \
    "reached only by #[path], $reached_excluded of them under the excluded directories;" \
    "$includes #[path] includes followed; $gone listed path(s) gone from the work tree);" \
    "excluded: $EXCLUDED ($excluded files); the module: $MODULE; $(value hits) hit(s)," \
    "$(value marked) marked; ring census: $(value census) row(s)"
sed -n 's/^allowed /  allowed: /p' "$work/verdict"
