# source-files.sh, sourced: the gates' walks of the source trees, each failing closed on what it
# cannot read (below, each function's own rule); tree_dirs lists a tree's crate directories, and
# source_files holds the rule for leaving a build's output out of a walk of the source trees.
# Every gate script's walk that leaves a directory out as a build's output reads it, the
# shell gates by calling source_files and the Python gates through scripts/source_files.py, so
# each leaves out exactly the directories the others do; a walk that leaves nothing out (one
# that reads only src/ and tests/ directories, where Cargo writes no output) has no use for it.
# One copy lives outside this file: the wake budget tie's walk, `rust_sources` in
# crates/inf-server/src/limits.rs, a Rust test with no shell in its path, which keeps the rule
# itself and reads the tag as find -type f does here: a regular file, a link of that name
# tagging nothing.
#
# source_files <name-pattern> <dir>...: prints every regular file whose name matches
# <name-pattern> (a find -name pattern) under each <dir>, one a line, in find's order, leaving
# out each directory that holds CACHEDIR.TAG as a regular file (find -type f: a link of that
# name tags nothing) and everything beneath it. The tag is the cache directory tag Cargo writes
# at the root of every target directory it makes, so it marks a build's output; a directory is
# never left out by its name, and a source directory named `target` at any depth is read. A
# tag file itself is never printed. One walk of find and one awk, so no process runs per
# directory, and one more find for the refusal below.
#
# It fails closed, returning non-zero with nothing printed on stdout: when find fails (a missing
# <dir>, a directory it cannot read: find's own message is on stderr), or when a matched path
# or a tag's path holds a newline, which a listing of one path a line cannot carry (it would
# read as two names that do not exist): `SOURCE-FILES SCOPE ERROR` on stderr, status 2.
# A caller sees that status only through a call form that reads it: `listing=$(source_files …)`
# or `source_files … >file` under `set -e` (or with `|| …`); a pipeline under pipefail. Inside
# `< <(…)`, or on a pipeline's left side without pipefail, the status is lost and a failed walk
# reads as a short listing.
source_files() {
    local name=$1 newline='
' found
    shift
    found=$(find "$@" -type f \( -name CACHEDIR.TAG -o -name "$name" \) -path "*$newline*" -print) ||
        return
    if [ -n "$found" ]; then
        echo "SOURCE-FILES SCOPE ERROR: a path under $* holds a newline, which a listing of" \
            "one path a line cannot carry" >&2
        return 2
    fi
    found=$(find "$@" -type f \( -name CACHEDIR.TAG -o -name "$name" \) -print) || return
    [ -n "$found" ] || return 0
    printf '%s\n' "$found" |
        awk '
            { path[NR] = $0 }
            /(^|\/)CACHEDIR\.TAG$/ {
                dir = $0
                sub(/\/?CACHEDIR\.TAG$/, "", dir)
                tagged[dir] = 1
            }
            END {
                for (i = 1; i <= NR; i++) {
                    if (path[i] ~ /(^|\/)CACHEDIR\.TAG$/) continue
                    dir = path[i]
                    skipped = 0
                    while (sub(/\/[^\/]*$/, "", dir)) {
                        if (dir in tagged) { skipped = 1; break }
                    }
                    if (!skipped) print path[i]
                }
            }'
}

# tree_dirs <sub> <top>...: prints <top>/<entry>/<sub> for each directory <entry> directly under
# each <top> whose <sub> is a directory, one a line, in glob order; <sub> `.` prints each
# <top>/<entry>. A <top> that is not a directory adds nothing. It is the one way a gate lists the
# crate directories of a tree: a glob such as `crates/*/src` probes <sub> inside each entry, and
# a probe inside a directory the walk cannot search reads the refusal as absence, so the entry
# would drop out of the walk unseen. It fails closed, `SOURCE-FILES SCOPE ERROR` on stderr,
# status 2 and nothing on stdout, on a <top> it cannot read, an entry it cannot search, or an
# entry whose name holds a newline, which a listing of one path a line cannot carry.
tree_dirs() {
    local sub=$1 top entry found='' newline='
'
    shift
    for top in "$@"; do
        [ -d "$top" ] || continue
        if [ ! -r "$top" ] || [ ! -x "$top" ]; then
            echo "SOURCE-FILES SCOPE ERROR: $top cannot be read, so its directories are unknown" >&2
            return 2
        fi
        for entry in "$top"/*/; do
            [ -d "$entry" ] || continue
            entry=${entry%/}
            case $entry in
            *"$newline"*)
                echo "SOURCE-FILES SCOPE ERROR: a directory under $top holds a newline, which a" \
                    "listing of one path a line cannot carry" >&2
                return 2
                ;;
            esac
            if [ ! -x "$entry" ]; then
                echo "SOURCE-FILES SCOPE ERROR: $entry cannot be searched, so whether it holds" \
                    "$sub is unknown" >&2
                return 2
            fi
            if [ "$sub" = . ]; then
                found="$found$entry$newline"
            elif [ -d "$entry/$sub" ]; then
                found="$found$entry/$sub$newline"
            fi
        done
    done
    printf '%s' "$found"
}
