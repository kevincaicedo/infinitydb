# source-files.sh, sourced: the gates' one walk of the source trees.
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
