# The gates' self-test machinery (ADR-0106 D1), sourced first by
# check-scripts-selftest.sh: the case helpers, the fixture git guard, the case
# log, the fixture lint and the driver that runs the self-test's groups.
#
# A group is a function `group_<name>() {` of the self-test, its cases seeing
# only what the group itself builds. The driver reads the group set from the
# file twice, its definition lines and bash's own function table once the file
# is loaded, and a name only one of them lists is red. With no argument it
# lints the groups' fixtures, runs every group in a process of its own, at most
# CARGO_BUILD_JOBS (default 4) at once and each at CARGO_BUILD_JOBS=1, prints
# their output in file order, folds their counts and names the longest group
# with its seconds on the OK line. Under a job pool CARGO_MAKEFLAGS names
# (`--jobserver-auth=fifo:<path>`, scripts/check-stage.sh's), a group beyond
# the first running holds one of its tokens: each group waits on the line's own
# token and the pool's at once (job_token.py) and gives the token back when it
# ends, so the self-test computes on no more than its pool, and at one token
# its groups run one at a time and end. A pool it cannot open exits 2.
# `--group <name>` runs one group in a work directory of its own and prints
# `group <name>: <pass> <fail>`, then `group <name> clock: <began> <ended>`,
# the monotonic clock in tenths of a second around the group; `--list-groups`
# prints bash's table; `--lint` runs the fixture lint alone. A count or stamp
# the driver reads is 0 or [1-9][0-9]*, or the group is red: its arithmetic
# never meets a leading zero or a non-number.
#
# Portable bash 3.2 (ADR-0106 D18), as the macOS leg runs it: no mapfile, no
# `wait -n`, no associative arrays, no `{fd}` redirection; BSD xargs, sed and
# awk. Every rm on a variable is guarded: an empty one would turn
# `rm -rf "$x/…"` into a delete at the filesystem root.

pass=0
fail=0
work=''
SELFTEST_GROUP=''
SELFTEST_ROOT=''
case_seq=0

# A case outside a group would run in every process the driver starts, the
# listing and the lint included: it turns each of them red with its label.
selftest_outside() {
    echo "selftest: the case '$1' runs outside any group; every case belongs to a group_<name> function" >&2
    exit 2
}

# INF_SELFTEST_CASE_LOG=<an empty directory>: each case also writes one record,
# under the directory of its group, in a file named by its place in the group:
# the group, its label, the verdict it wants, the exit status, its own verdict,
# the work root, the cases it stands for and the captured output; a suite that
# counts its own cases (the inventory and dep-DAG fixtures) is one record of
# that many. Two runs compare case by case on these records; a group whose
# records and counted cases differ in number is red.
# case_log <label> <want> <status> <pass|fail> [<output file> [<cases>]]
case_log() {
    [ -n "${INF_SELFTEST_CASE_LOG:-}" ] || return 0
    case_seq=$((case_seq + 1))
    {
        printf 'section\t%s\nlabel\t%s\nwant\t%s\nstatus\t%s\nverdict\t%s\nwork\t%s\ncases\t%s\noutput\n' \
            "$SELFTEST_GROUP" "$1" "$2" "$3" "$4" "$work" "${6:-1}"
        [ -z "${5:-}" ] || [ ! -f "$5" ] || cat "$5"
    } >"$INF_SELFTEST_CASE_LOG/$SELFTEST_GROUP/$(printf '%05d' "$case_seq")"
}

# A gate that never ran gives no verdict (ADR-0106 D1): counted as red, a
# tool the runner lacked passed every planted red of the gate that needed it.
# Each expect* refuses two shapes, failing the case with the cause.
# not_runnable <label> <command> [<want>]: a path that is no executable file,
# or a name the shell cannot find, is refused before it runs — bash 3.2 under
# `set -e` reports a failed exec of a path as exit 1, which reads as red.
not_runnable() {
    case $2 in
    */*) [ -f "$2" ] && [ -x "$2" ] && return 1 ;;
    *) command -v -- "$2" >/dev/null && return 1 ;;
    esac
    fail=$((fail + 1))
    echo "SELFTEST FAIL: '$2' is not a runnable command, so it gives no verdict — $1"
    case_log "$1" "${3:-run}" - fail
}
# no_verdict <status> <label> <log> [<want>]: a command that ran and exited 126
# or 127 — env refusing a gate it cannot execute or find, or a gate that died
# on a tool the runner lacks (both shells keep those codes) — is not a verdict.
no_verdict() {
    case $1 in 126 | 127) ;; *) return 1 ;; esac
    fail=$((fail + 1))
    echo "SELFTEST FAIL: exit $1 means a command could not be executed or found, not a gate verdict — $2"
    sed 's/^/    | /' "$3"
    case_log "$2" "${4:-run}" "$1" fail "$3"
}

# expect <red|green> <label> <command…>: runs the command with stdout+stderr
# captured; a mismatch prints the captured output.
expect() {
    local want=$1 label=$2
    shift 2
    [ -n "$SELFTEST_GROUP" ] || selftest_outside "$label"
    local log="$work/log" status=0
    not_runnable "$label" "$1" "$want" && return 0
    "$@" >"$log" 2>&1 || status=$?
    no_verdict "$status" "$label" "$log" "$want" && return 0
    if { [ "$want" = red ] && [ "$status" -ne 0 ]; } || { [ "$want" = green ] && [ "$status" -eq 0 ]; }; then
        pass=$((pass + 1))
        case_log "$label" "$want" "$status" pass "$log"
    else
        fail=$((fail + 1))
        echo "SELFTEST FAIL: expected $want, got exit $status — $label"
        sed 's/^/    | /' "$log"
        case_log "$label" "$want" "$status" fail "$log"
    fi
}

# expect_output <label> <pattern> <command…>: the command's output must
# contain the pattern (a scope disclosure, an allowed-site line).
expect_output() {
    local label=$1 pattern=$2
    shift 2
    [ -n "$SELFTEST_GROUP" ] || selftest_outside "$label"
    local log="$work/log" status=0
    not_runnable "$label" "$1" "output $pattern" && return 0
    "$@" >"$log" 2>&1 || status=$?
    no_verdict "$status" "$label" "$log" "output $pattern" && return 0
    if grep -q -- "$pattern" "$log"; then
        pass=$((pass + 1))
        case_log "$label" "output $pattern" "$status" pass "$log"
    else
        fail=$((fail + 1))
        echo "SELFTEST FAIL: output lacks '$pattern' — $label"
        sed 's/^/    | /' "$log"
        case_log "$label" "output $pattern" "$status" fail "$log"
    fi
}

# expect_red_because <label> <pattern> <command…>: red, and for its own
# reason — a plant that is red for another reason is not evidence.
expect_red_because() {
    local label=$1 pattern=$2 status=0
    shift 2
    [ -n "$SELFTEST_GROUP" ] || selftest_outside "$label"
    local log="$work/log"
    not_runnable "$label" "$1" "red $pattern" && return 0
    "$@" >"$log" 2>&1 || status=$?
    no_verdict "$status" "$label" "$log" "red $pattern" && return 0
    if [ "$status" -ne 0 ] && grep -q -- "$pattern" "$log"; then
        pass=$((pass + 1))
        case_log "$label" "red $pattern" "$status" pass "$log"
    else
        fail=$((fail + 1))
        echo "SELFTEST FAIL: expected red naming '$pattern', got exit $status — $label"
        sed 's/^/    | /' "$log"
        case_log "$label" "red $pattern" "$status" fail "$log"
    fi
}

# fx_git <root> <git args…>: git in a fixture repository only. `git -C ""`
# runs in the current directory, and a root that is not its own repository
# lets git walk up to an enclosing one, the developer's engine branch: a root
# that is empty, outside $work or without its own `.git` is refused before
# git runs (the rule every destructive command on a variable keeps). `init`
# creates that `.git`, so it alone runs bare, on a root its guarded
# constructor made.
fx_git() {
    local root=$1
    shift
    case $root in
    "$work"/?*) ;;
    *) echo "fx_git: '$root' is not a fixture directory under $work" >&2; exit 2 ;;
    esac
    [ -d "$root/.git" ] || { echo "fx_git: no repository of its own at '$root'" >&2; exit 2; }
    git -C "$root" "$@"
}
# fx_only <file>: every `git -C` on a variable in <file> is fx_git's own or an
# `init`; the canary is the self-test with a raw reset appended.
fx_only() {
    ! grep -n 'git -C "\$' "$1" | grep -v -e ' init -q$' -e '^[0-9]*:    git -C "\$root" "\$@"$'
}

# ---------------------------------------------------------------- the driver
# selftest_defined <file>: the groups its definition lines name, in file order.
selftest_defined() {
    sed -n 's/^group_\([a-z0-9_][a-z0-9_]*\)() {$/\1/p' "$1"
}
# selftest_declared: the groups bash's function table holds.
selftest_declared() {
    declare -F | sed -n 's/^declare -f[a-z]* group_//p'
}
# selftest_clock: the monotonic clock in tenths of a second; bash 3.2 has no
# clock finer than a second, and python3 is already the lint's.
selftest_clock() {
    python3 -I -S -c 'import time; print(int(time.monotonic() * 10))'
}
# selftest_number <word>: true for 0 or [1-9][0-9]*, the only numbers the
# driver's arithmetic reads.
selftest_number() {
    case $1 in '' | 0?* | *[!0-9]*) return 1 ;; esac
}
# selftest_tenths <group> <log>: the group's tenths of a second, from its one
# clock line of exactly two stamps, one space apart and in order; else that
# line, or `none`, and status 1. The driver reads it for a group no earlier
# check counted red.
selftest_tenths() {
    local line stamps began ended
    line=$(sed -n "/^group $1 clock: /p" "$2")
    stamps=${line#"group $1 clock: "}
    began=${stamps%% *}
    ended=${stamps#"$began "}
    if [ "$began" = "$stamps" ] || ! selftest_number "$began" || ! selftest_number "$ended" ||
        [ "$ended" -lt "$began" ]; then
        echo "${line:-none}"
        return 1
    fi
    echo $((ended - began))
}

# The fixture lint: an absent fixture passes a bare `expect red`, so a function
# the self-test defines is red when it reads a `$work/<name>` before it, or a
# call it makes (an `fx_build_<name>` for a fixture several groups read), has
# created it. selftest_lint.py reads bash's own text of each function
# (`declare -f`, nested functions included) and states what creates a name: a
# word inside quotes is never a call and quoted text never a creation, and a
# helper creates a name its argument completes only for the word each call
# passes there.
selftest_lint() {
    local fn
    for fn in $(declare -F | sed -n 's/^declare -f[a-z]* //p'); do
        case " $SELFTEST_LIB_FUNCS " in *" $fn "*) continue ;; esac
        printf '\036%s\n' "$fn"
        declare -f "$fn"
    done | python3 -B -I "$SELFTEST_LIB_DIR/selftest_lint.py"
}

# selftest_pool: the FIFO path of the job pool CARGO_MAKEFLAGS names, or
# nothing when it names none; status 2, naming it, for a pool the driver cannot
# open (a `--jobserver-auth` that is no `fifo:` path, or a path that is no FIFO).
selftest_pool() {
    local word auth=''
    set -f
    for word in ${CARGO_MAKEFLAGS:-}; do
        case $word in --jobserver-auth=* | --jobserver-fds=*) auth=${word#*=} ;; esac
    done
    set +f
    case $auth in
    '') return 0 ;;
    fifo:?*) [ -p "${auth#fifo:}" ] && echo "${auth#fifo:}" && return 0 ;;
    esac
    echo "selftest: CARGO_MAKEFLAGS names a pool the driver cannot open: '$auth'" >&2
    return 2
}

# The worker xargs runs a group in: sh -c <it> <self> <root> <pool> <lib dir>
# <group>. Under a pool it first takes a token, the line's own or the pool's,
# and gives that byte back to that FIFO once the group has ended.
SELFTEST_WORKER='taken=
if [ -n "$2" ]; then
    taken=$(python3 -I -S "$3/job_token.py" take "$1/own" "$2") || exit 0
fi
CARGO_BUILD_JOBS=1 "$0" --group "$4" >"$1/$4.log" 2>&1
echo $? >"$1/$4.status"
case $taken in
"0 "[0-9]*) fifo=$1/own ;;
"1 "[0-9]*) fifo=$2 ;;
*) exit 0 ;;
esac
printf "\\$(printf %03o "${taken#* }")" >"$fifo"'

# selftest_run_group <self> <name>: one group in its own work directory.
selftest_run_group() {
    local self=$1 name=$2 logged began
    case " $(selftest_defined "$self" | tr '\n' ' ') " in
    *" $name "*) ;;
    *) echo "selftest: no group '$name' in $self" >&2; exit 2 ;;
    esac
    work=$(mktemp -d)
    [ -n "$work" ] && [ -d "$work" ] || { echo "selftest: mktemp failed" >&2; exit 2; }
    trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT
    SELFTEST_GROUP=$name
    began=$(selftest_clock)
    if [ -n "${INF_SELFTEST_CASE_LOG:-}" ]; then
        mkdir "$INF_SELFTEST_CASE_LOG/$name" || exit 2
    fi
    "group_$name"
    if [ -n "${INF_SELFTEST_CASE_LOG:-}" ]; then
        logged=$(cat "$INF_SELFTEST_CASE_LOG/$name"/* 2>/dev/null |
            awk -F '\t' '$1 == "cases" { n += $2 } END { print n + 0 }')
        if [ "$logged" -ne $((pass + fail)) ]; then
            echo "SELFTEST FAIL: group $name's records count $logged cases, the group $((pass + fail))"
            fail=$((fail + 1))
        fi
    fi
    echo "group $name: $pass $fail"
    echo "group $name clock: $began $(selftest_clock)"
    [ "$fail" -eq 0 ]
}

# selftest_drive <self>: every group, folded into one verdict.
selftest_drive() {
    local self=$1 jobs=${CARGO_BUILD_JOBS:-4} defined g n=0 red=0 groups_red=0 line
    local status count names='' gp gf tenths longest=-1 longest_group='' pool
    case $jobs in
    '' | 0* | *[!0-9]*) echo "selftest: CARGO_BUILD_JOBS '$jobs' is not a positive integer" >&2; exit 2 ;;
    esac
    pool=$(selftest_pool) || exit 2
    if [ -n "${INF_SELFTEST_CASE_LOG:-}" ]; then
        [ -d "$INF_SELFTEST_CASE_LOG" ] && [ -z "$(ls -A "$INF_SELFTEST_CASE_LOG")" ] || {
            echo "selftest: INF_SELFTEST_CASE_LOG '$INF_SELFTEST_CASE_LOG' is not an empty directory" >&2
            exit 2
        }
    fi
    # The groups' logs and statuses; a global, since the EXIT trap runs after
    # this function has returned.
    SELFTEST_ROOT=$(mktemp -d)
    local root=$SELFTEST_ROOT
    [ -n "$root" ] && [ -d "$root" ] || { echo "selftest: mktemp failed" >&2; exit 2; }
    trap '[ -n "$SELFTEST_ROOT" ] && [ -d "$SELFTEST_ROOT" ] && rm -rf "$SELFTEST_ROOT"' EXIT
    selftest_defined "$self" >"$root/defined"
    if ! "$self" --list-groups >"$root/declared" 2>"$root/declared.err"; then
        echo "SELFTEST DRIVER: '$self --list-groups' failed:"
        sed 's/^/    | /' "$root/declared.err"
        red=$((red + 1))
    fi
    # One collation, bytes (LC_ALL=C), for sort, uniq and comm: comm and uniq
    # read their input in the order they collate, which must be the order sort
    # wrote whatever the caller's locale and whichever system's tools these are.
    LC_ALL=C sort "$root/defined" >"$root/defined.sorted"
    LC_ALL=C sort "$root/declared" >"$root/declared.sorted"
    for g in $(LC_ALL=C uniq -d "$root/defined.sorted"); do
        echo "SELFTEST DRIVER: group $g is defined twice"
        red=$((red + 1))
    done
    for g in $(LC_ALL=C sort -u "$root/defined" | LC_ALL=C comm -23 - "$root/declared.sorted"); do
        echo "SELFTEST DRIVER: group $g is in the definition lines only (bash's table lacks it)"
        red=$((red + 1))
    done
    for g in $(LC_ALL=C comm -13 "$root/defined.sorted" "$root/declared.sorted"); do
        echo "SELFTEST DRIVER: group $g is in bash's function table only (its definition is not \`group_$g() {\`)"
        red=$((red + 1))
    done
    defined=$(cat "$root/defined")
    if [ -z "$defined" ]; then
        echo "SELFTEST DRIVER: no group is defined in $self"
        red=$((red + 1))
    fi
    if ! "$self" --lint >"$root/lint" 2>&1; then
        cat "$root/lint"
        red=$((red + 1))
    fi
    if [ "$red" -eq 0 ]; then
        # The line's own token is one byte in a FIFO of the driver's; the driver
        # holds it and the pool open both ways, so a token given back never
        # blocks and a take never reads an end of file. The groups do not hold them.
        if [ -n "$pool" ]; then
            mkfifo "$root/own" || exit 2
            exec 7<>"$root/own" 8<>"$pool"
            printf + >&7
        fi
        printf '%s\n' $defined | xargs -n 1 -P "$jobs" sh -c "$SELFTEST_WORKER" \
            "$self" "$root" "$pool" "$SELFTEST_LIB_DIR" 7>&- 8>&-
    fi
    for g in $defined; do
        [ "$red" -eq 0 ] || break
        n=$((n + 1))
        names="$names${names:+, }$g"
        status=$(cat "$root/$g.status" 2>/dev/null || echo none)
        sed -e "/^group $g: /d" -e "/^group $g clock: /d" "$root/$g.log" 2>/dev/null || true
        count=$(grep -c "^group $g: " "$root/$g.log" 2>/dev/null || true)
        if [ "${count:-0}" -ne 1 ]; then
            echo "SELFTEST DRIVER: group $g printed ${count:-0} count lines (exit $status), not one"
            groups_red=$((groups_red + 1))
            continue
        fi
        line=$(grep "^group $g: " "$root/$g.log")
        gp=$(echo "$line" | awk '{ print $3 }')
        gf=$(echo "$line" | awk '{ print $4 }')
        case $gp:$gf in
        [1-9]*:[1-9]* | [1-9]*:0 | 0:[1-9]* | 0:0) ;;
        *) gp=x ;;
        esac
        case $gp$gf in *[!0-9]*) gp=x ;; esac
        if [ "$gp" = x ]; then
            echo "SELFTEST DRIVER: group $g's count line is malformed: $line"
            groups_red=$((groups_red + 1))
            continue
        fi
        pass=$((pass + gp))
        fail=$((fail + gf))
        if [ $((gp + gf)) -eq 0 ]; then
            echo "SELFTEST DRIVER: group $g ran no case"
            groups_red=$((groups_red + 1))
        elif { [ "$gf" -eq 0 ] && [ "$status" != 0 ]; } || { [ "$gf" -ne 0 ] && [ "$status" = 0 ]; }; then
            echo "SELFTEST DRIVER: group $g counted $gf failed case(s) but exited $status"
            groups_red=$((groups_red + 1))
        elif [ "$gf" -ne 0 ]; then
            groups_red=$((groups_red + 1))
        elif ! tenths=$(selftest_tenths "$g" "$root/$g.log"); then
            echo "SELFTEST DRIVER: group $g's clock line is not one line of two stamps" \
                "in order: $tenths"
            groups_red=$((groups_red + 1))
        elif [ "$tenths" -gt "$longest" ]; then
            longest=$tenths
            longest_group=$g
        fi
    done
    if [ "$red" -ne 0 ] || [ "$groups_red" -ne 0 ] || [ "$fail" -ne 0 ] || [ "$pass" -eq 0 ]; then
        echo "check-scripts self-test FAILED: $fail of $((pass + fail)) cases failed;" \
            "$groups_red of $n groups red; $red driver error(s)"
        exit 1
    fi
    echo "check-scripts self-test OK ($pass cases in $n groups; the longest group:" \
        "$longest_group, $((longest / 10)).$((longest % 10)) s; each exiting non-zero on its" \
        "planted case: $names)"
}

# selftest_main <self> [--group <name> | --list-groups | --lint]
selftest_main() {
    local self=$1
    shift
    case ${1:-} in
    '') selftest_drive "$self" ;;
    --group) [ "$#" -eq 2 ] || { echo "usage: $self [--group <name> | --list-groups | --lint]" >&2; exit 2; }
        selftest_run_group "$self" "$2" ;;
    --list-groups) selftest_declared ;;
    --lint) selftest_lint ;;
    *) echo "usage: $self [--group <name> | --list-groups | --lint]" >&2; exit 2 ;;
    esac
}

SELFTEST_LIB_FUNCS=$(declare -F | sed -n 's/^declare -f[a-z]* //p' | tr '\n' ' ')
SELFTEST_LIB_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
