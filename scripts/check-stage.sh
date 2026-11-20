#!/usr/bin/env bash
# A stage of the `check` recipe: one line that runs the script gates it names at
# once, each still judged and printed as its own line was.
#
#   scripts/check-stage.sh <check-*.sh>...
#
# Each name is a gate's file name beside this script, so a gate still greps to
# its recipe line. A gate starts with stdin /dev/null and writes to a file of
# its own; its exit status and seconds are kept. The stage holds its line's own
# token and draws one job pool: the pool CARGO_MAKEFLAGS names
# (`--jobserver-auth=fifo:<path>`), else a FIFO it makes with CARGO_BUILD_JOBS
# - 1 tokens and names to its gates the same way. A gate starts on the line's
# token when no gate of the stage runs, else on a token taken from the pool,
# and its token goes back when it ends. cargo, and the self-test drivers, draw
# that pool too (a cargo that inherits a pool ignores its own job bound), so a
# stage computes on at most CARGO_BUILD_JOBS processes (4 when unset), and at
# one token its gates run one at a time and it ends.
#
# Once a gate is red no further gate starts, as no further line of a recipe
# does; started gates run to their end. Then the stage prints each green gate's
# output under a header naming it, its exit status and its seconds, in the
# order of the arguments; each red gate's output, whole; then a closing block
# that fits the 40 lines and 4,000 characters a reader of the recipe keeps of a
# red line: for the first three red gates, the header and the last 11 lines of
# output, each cut at 100 bytes; then one line of at most 100 bytes naming the
# red gates and counting those not started. Exit 0 when every gate ran green, 1
# otherwise; a killed stage prints nothing and leaves its directory under
# TMPDIR for the system's cleaning.
#
# INF_CHECK_STAGE=serial runs the gates one at a time with no pool, as the
# recipe ran them a line each: the stage's control. Exit 2 with nothing run on
# a name that is no `check-*.sh` file beside this script or is not executable,
# a name twice, a CARGO_BUILD_JOBS that is not a positive integer or is over
# STAGE_JOBS_MAX, an INF_CHECK_STAGE other than `serial`, or a pool
# CARGO_MAKEFLAGS names that the stage cannot open (no `fifo:` path, or a path
# that is no FIFO).
#
# Portable bash 3.2 (ADR-0106 D18): no `wait -n`, mapfile, associative arrays
# or `{fd}`; python3 for the clock and the take (job_token.py).
set -uo pipefail

# A new pool's FIFO holds its tokens unread: 1,024 bytes fit any system's pipe
# buffer, so filling it never blocks. Over it: exit 2, nothing run.
readonly STAGE_JOBS_MAX=1024
# The closing block's lines and its naming line, in bytes.
readonly STAGE_LINE_MAX=100
readonly STAGE_TAIL_LINES=11
readonly STAGE_TAIL_GATES=3

here=$(dirname "$0")

refuse() {
    echo "check-stage: $1; nothing ran" >&2
    exit 2
}

# stage_clock: the monotonic clock in tenths of a second.
stage_clock() {
    python3 -I -S -c 'import time; print(int(time.monotonic() * 10))'
}

# stage_number <word>: true for 0 or [1-9][0-9]*, the only numbers the stage's
# arithmetic reads from text.
stage_number() {
    case $1 in '' | 0?* | *[!0-9]*) return 1 ;; esac
}

[ "$#" -gt 0 ] || refuse "usage: check-stage.sh <check-*.sh>..."
jobs=${CARGO_BUILD_JOBS:-4}
case $jobs in
'' | 0* | *[!0-9]*) refuse "CARGO_BUILD_JOBS '$jobs' is not a positive integer" ;;
esac
[ "${#jobs}" -le 4 ] && [ "$jobs" -le "$STAGE_JOBS_MAX" ] ||
    refuse "CARGO_BUILD_JOBS $jobs is over $STAGE_JOBS_MAX"
mode=${INF_CHECK_STAGE:-}
case $mode in
'' | serial) ;;
*) refuse "INF_CHECK_STAGE '$mode' is neither unset nor serial" ;;
esac
seen=' '
for name in "$@"; do
    named=''
    case $name in
    check-*.sh) case $name in *[!A-Za-z0-9._-]*) ;; *) named=yes ;; esac ;;
    esac
    [ -n "$named" ] || refuse "'$name' is no check-*.sh file name"
    [ -f "$here/$name" ] && [ -x "$here/$name" ] || refuse "'$name' is no executable file in $here"
    case $seen in *" $name "*) refuse "'$name' is named twice" ;; esac
    seen="$seen$name "
done

pool='' pool_kind=''
if [ "$mode" != serial ]; then
    auth=''
    set -f
    for word in ${CARGO_MAKEFLAGS:-}; do
        case $word in --jobserver-auth=* | --jobserver-fds=*) auth=${word#*=} ;; esac
    done
    set +f
    case $auth in
    '') ;;
    fifo:?*)
        pool=${auth#fifo:}
        [ -p "$pool" ] || refuse "CARGO_MAKEFLAGS names the pool '$pool', which is no FIFO"
        pool_kind='the pool CARGO_MAKEFLAGS names'
        ;;
    *) refuse "CARGO_MAKEFLAGS names a pool this stage cannot open: '$auth' is no fifo: path" ;;
    esac
fi

root=$(mktemp -d)
[ -n "$root" ] && [ -d "$root" ] || refuse "mktemp failed"
trap '[ -n "$root" ] && [ -d "$root" ] && rm -rf "$root"' EXIT
began_stage=$(stage_clock)
stage_number "$began_stage" || refuse "the clock read '$began_stage'"

n=$#
gates=("$@")
statuses=()
seconds=()
i=0
while [ "$i" -lt "$n" ]; do
    statuses[i]=none
    seconds[i]=''
    i=$((i + 1))
done
internal=0

# record <index> <status> <began> <ended>, each a number read from a line: a
# field that is no number, an index outside the stage, a second report, or a
# clock that ran backwards leaves the gate with no status, red.
record() {
    if stage_number "$1" && [ "$1" -lt "$n" ] && [ "${statuses[$1]}" = running ] &&
        stage_number "$2" && stage_number "$3" && stage_number "$4" && [ "$4" -ge "$3" ]; then
        statuses[$1]=$2
        seconds[$1]="$((($4 - $3) / 10)).$((($4 - $3) % 10))"
        return 0
    fi
    echo "check-stage: a gate's end line is malformed: '$*'"
    internal=$((internal + 1))
    return 1
}

if [ "$mode" = serial ]; then
    i=0
    while [ "$i" -lt "$n" ]; do
        statuses[i]=running
        began=$(stage_clock)
        "$here/${gates[i]}" </dev/null >"$root/$i.out" 2>&1
        status=$?
        record "$i" "$status" "$began" "$(stage_clock)"
        [ "${statuses[i]}" = 0 ] || break
        i=$((i + 1))
    done
else
    if [ -z "$pool" ]; then
        pool="$root/pool"
        mkfifo "$pool" || refuse "mkfifo failed"
        pool_kind="a pool of its own, $((jobs - 1)) tokens"
        export CARGO_MAKEFLAGS="-j --jobserver-auth=fifo:$pool"
    fi
    # The stage holds the pool open both ways, so a token given back never
    # blocks and a take never reads an end of file; and the FIFO its gates'
    # subshells write their end lines to.
    exec 3<>"$pool"
    mkfifo "$root/ended" || refuse "mkfifo failed"
    exec 4<>"$root/ended"
    if [ "$pool" = "$root/pool" ]; then
        i=1
        while [ "$i" -lt "$jobs" ]; do
            printf + >&3
            i=$((i + 1))
        done
    fi
    next=0 running=0 stop=0 held=''
    while :; do
        if [ "$stop" -eq 0 ] && [ "$next" -lt "$n" ]; then
            took=''
            if [ "$running" -gt 0 ]; then
                took=$(python3 -I -S "$here/job_token.py" take "$pool" --watch 4)
                case $? in
                0) ;;
                3) took=none ;;
                *)
                    echo "check-stage: the take from the pool failed: no further gate starts"
                    internal=$((internal + 1))
                    stop=1
                    took=none
                    ;;
                esac
            fi
            if [ "$took" != none ]; then
                if [ -n "$took" ]; then
                    code=${took#0 }
                    if [ "$code" = "$took" ] || ! stage_number "$code" || [ "$code" -gt 255 ]; then
                        echo "check-stage: the take printed '$took': no further gate starts"
                        internal=$((internal + 1))
                        stop=1
                        continue
                    fi
                    held="$held $code"
                fi
                statuses[next]=running
                (
                    began=$(stage_clock)
                    "$here/${gates[next]}" </dev/null >"$root/$next.out" 2>&1 4>&-
                    status=$?
                    echo "$next $status $began $(stage_clock)" >&4
                ) 3>&- &
                next=$((next + 1))
                running=$((running + 1))
                continue
            fi
        elif [ "$running" -eq 0 ]; then
            break
        fi
        if ! read -r line <&4; then
            echo "check-stage: the end lines could not be read"
            internal=$((internal + 1))
            break
        fi
        running=$((running - 1))
        # shellcheck disable=SC2086 # the line's four fields
        record $line || stop=1
        if [ -n "$held" ]; then
            code=${held##* }
            held=${held% *}
            # shellcheck disable=SC2059 # the token's own byte, in octal
            printf "\\$(printf '%03o' "$code")" >&3
        fi
        case ${line#* } in 0\ *) ;; *) stop=1 ;; esac
    done
    wait
fi
ended_stage=$(stage_clock)

header() { # <index> [<suffix>]
    local s=${seconds[$1]}
    if [ -n "$s" ]; then
        echo "--- ${gates[$1]}: exit ${statuses[$1]}, $s s${2:-}"
    else
        echo "--- ${gates[$1]}: no exit status${2:-}"
    fi
}
red=0 unstarted=0 reds=''
i=0
while [ "$i" -lt "$n" ]; do
    case ${statuses[i]} in
    0)
        header "$i"
        cat "$root/$i.out"
        ;;
    none) unstarted=$((unstarted + 1)) ;;
    *)
        red=$((red + 1))
        reds="$reds $i"
        ;;
    esac
    i=$((i + 1))
done
for i in $reds; do
    header "$i"
    cat "$root/$i.out" 2>/dev/null
done
if [ "$red" -eq 0 ] && [ "$unstarted" -eq 0 ] && [ "$internal" -eq 0 ]; then
    wall=$((ended_stage - began_stage))
    if [ "$mode" = serial ]; then
        how='one at a time (INF_CHECK_STAGE=serial)'
    else
        how="at once on $jobs jobs ($pool_kind)"
    fi
    echo "check-stage: $n of $n gates green in $((wall / 10)).$((wall % 10)) s, $how"
    exit 0
fi
shown=0
for i in $reds; do
    [ "$shown" -lt "$STAGE_TAIL_GATES" ] || break
    shown=$((shown + 1))
    header "$i" ", its last $STAGE_TAIL_LINES lines" | LC_ALL=C cut -c "1-$STAGE_LINE_MAX"
    tail -n "$STAGE_TAIL_LINES" "$root/$i.out" 2>/dev/null | LC_ALL=C cut -c "1-$STAGE_LINE_MAX"
done
# The naming line: a name joins while it fits, with room kept for "and N more"
# while names are left after it.
line="check-stage FAILED: $red of $n red, $unstarted not started"
[ "$internal" -eq 0 ] || line="$line, $internal stage error(s)"
line="$line:"
more=0 left=$red
for i in $reds; do
    left=$((left - 1))
    room=$STAGE_LINE_MAX
    [ "$left" -eq 0 ] || room=$((STAGE_LINE_MAX - 14))
    if [ "$more" -eq 0 ] && [ $((${#line} + 1 + ${#gates[i]})) -le "$room" ]; then
        line="$line ${gates[i]}"
    else
        more=$((more + 1))
    fi
done
[ "$more" -eq 0 ] || line="$line and $more more"
echo "$line"
exit 1
