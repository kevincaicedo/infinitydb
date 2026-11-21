# source-files.sh, sourced: the one rule for leaving a build's output out of a walk of the source
# trees. Every gate walk that leaves a directory out as a build's output reads it, the shell
# gates by calling source_files and the Python gates through scripts/source_files.py, so each
# leaves out exactly the directories the others do; a walk that leaves nothing out (one that
# reads only src/ and tests/ directories, where Cargo writes no output) has no use for it.
#
# source_files <name-pattern> <dir>...: prints every regular file whose name matches
# <name-pattern> (a find -name pattern) under each <dir>, one a line, in find's order, leaving
# out each directory that holds CACHEDIR.TAG and everything beneath it. The tag is the cache
# directory tag Cargo writes at the root of every target directory it makes, so it marks a
# build's output; a directory is never left out by its name, and a source directory named
# `target` at any depth is read. A tag file itself is never printed. One find and one awk, so
# no process runs per directory. A <dir> that does not exist fails the find, and the caller's
# pipefail sees it.
source_files() {
    local name=$1
    shift
    find "$@" -type f \( -name CACHEDIR.TAG -o -name "$name" \) -print |
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
