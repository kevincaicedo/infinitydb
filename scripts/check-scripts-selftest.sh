#!/usr/bin/env bash
# Self-test for the mechanical gates (ADR-0106 D1): every `check-*.sh`
# rewritten after the 2026-08-30 review must go RED on a planted violation
# and stay GREEN on the sanctioned shapes — otherwise "OK" is a claim, not a
# measurement. The review found two gates that had been inert for months
# (P1: a directory that did not exist, skipped silently; P1c: a file cut at
# its first `#[cfg(test)]`), and a `just` recipe whose bare `wait` could not
# fail. Each of those shapes is a case below; a regression in any gate turns
# this script red inside `just check`.
#
# Fixture trees are built under mktemp and handed to the gates through
# INF_CHECK_ROOT (the gates) and INF_SIM_BIN / INF_SWEEP_SHARDS (the sweep
# runner, with a stub simulator). No cargo, except in the lint-scope probe
# cases, which run `cargo clippy` on the pinned toolchain, and the target-root
# config and nextest-config cases, which run `cargo metadata` there.
#
# The cases live in group functions, `group_<name>() {`, one a gate's
# section or a part of one, each building every fixture it reads: the driver
# in scripts/selftest-lib.sh runs every group in a process and a work
# directory of its own, several at once, and folds their counts (`--group
# <name>` runs one). A group's body is not indented: its heredocs and
# multi-line strings are fixture bytes. INF_SELFTEST_CASE_LOG=<an empty
# directory> writes one record a case (the lib's header says what it holds).
# A group the driver runs for more than 20 s is cut, each part building its
# own fixtures: the longest group is the line's floor. The OK line names the
# longest group with its seconds on every run, which is where a cut shows.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "$SCRIPT_DIR/.."
# The cases build fixture checkouts of their own, so none inherits the root the
# justfile exports for this checkout, which every other checkout's helper
# refuses (ADR-0204 D1); a case that needs CARGO_TARGET_DIR sets it itself.
unset CARGO_TARGET_DIR
# shellcheck source=selftest-lib.sh
. "$SCRIPT_DIR/selftest-lib.sh"

group_self_test() {
expect green "self-test: every git command on a fixture root goes through fx_git" \
    fx_only "$SCRIPT_DIR/check-scripts-selftest.sh"
expect green "self-test: every git command in the lib goes through fx_git" \
    fx_only "$SCRIPT_DIR/selftest-lib.sh"
cp "$SCRIPT_DIR/check-scripts-selftest.sh" "$work/selftest-raw-git"
printf 'git -C "%sroot" reset -q --hard HEAD~1\n' '$' >>"$work/selftest-raw-git"
expect red "self-test: a raw git reset on a fixture root is refused (canary)" fx_only "$work/selftest-raw-git"
fx_empty_canary() (
    mkdir -p "$work/fx-guard" && cd "$work/fx-guard" && git init -q &&
        git -c user.name=t -c user.email=t@t commit -q --allow-empty -m base && : >untracked &&
        fx_git "" clean -fdq
)
expect_red_because "self-test: fx_git refuses an empty root" "fx_git: '' is not a fixture directory under" \
    fx_empty_canary
expect green "self-test: the refused clean removed nothing" test -f "$work/fx-guard/untracked"
fx_outside_canary() (fx_git "$SCRIPT_DIR/.." status)
expect_red_because "self-test: fx_git refuses a root outside the fixture directory" \
    "is not a fixture directory under" fx_outside_canary
fx_norepo_canary() (mkdir -p "$work/fx-norepo" && fx_git "$work/fx-norepo" status)
expect_red_because "self-test: fx_git refuses a fixture directory with no repository of its own" \
    "no repository of its own" fx_norepo_canary
# The canary of both refusals: every expect* fails, red expectations included,
# on a gate name the shell cannot find, a gate path that does not exist or is
# not executable (each by not_runnable, on every shell), a gate that dies on a
# missing tool (127) and env refusing a gate it cannot execute (126) (each by
# no_verdict). Its own log directory keeps the inner cases off the outer
# case's log.
nv_canary() (
    unset INF_SELFTEST_CASE_LOG
    work="$work/nv" && mkdir -p "$work" && : >"$work/noexec" && pass=0 && fail=0
    absent="$work/no-such-gate"
    printf '#!/usr/bin/env bash\nset -e\ninf-selftest-no-such-tool\n' >"$work/tool-missing"
    chmod +x "$work/tool-missing"
    expect red "a gate name the shell cannot find" inf-selftest-no-such-gate >"$work/o1"
    expect_red_because "a gate path that does not exist" "" "$absent" >"$work/o2"
    expect_output "a gate path that is not executable" "" "$work/noexec" >"$work/o3"
    expect red "a gate that dies on a missing tool" "$work/tool-missing" >"$work/o4"
    expect red "env refusing a gate it cannot execute" env "$work/noexec" >"$work/o5"
    cat "$work"/o[1-5]
    grep -qF "'inf-selftest-no-such-gate' is not a runnable command" "$work/o1" &&
        grep -qF "'$absent' is not a runnable command" "$work/o2" &&
        grep -qF "'$work/noexec' is not a runnable command" "$work/o3" &&
        grep -qF "exit 127 means a command could not be executed or found" "$work/o4" &&
        grep -qF "exit 126 means a command could not be executed or found" "$work/o5" &&
        [ "$pass" -eq 0 ] && [ "$fail" -eq 5 ]
)
expect green "self-test: a gate that never ran is never counted as a verdict (canary)" nv_canary
}

group_driver() {
# The driver itself, on fixture scripts that source the lib: each shape it must
# refuse is red naming its cause, and its control is green. A fixture's groups
# run under the real driver, so a fixture is a small self-test of its own.
# drv_fixture <name> <line…>: a script holding the body's lines, run as the
# self-test runs, from its own path.
drv_fixture() {
    local file="$work/drv-$1.sh"
    shift
    {
        printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
        printf '. "%s"\n' "$SCRIPT_DIR/selftest-lib.sh"
        printf '%s\n' "$@"
        printf '%s\n' 'selftest_main "$0" "$@"'
    } >"$file"
    chmod +x "$file"
    echo "$file"
}
drv_run() { env -u INF_SELFTEST_CASE_LOG CARGO_BUILD_JOBS=2 "$@"; }
f=$(drv_fixture control 'group_one() {' 'expect green "one passes" true' '}' \
    'group_two() {' 'expect red "two passes" false' '}')
expect_output "driver: the control's OK line names its groups and counts" \
    "check-scripts self-test OK (2 cases in 2 groups, each exiting non-zero on its planted case: one, two)" \
    drv_run "$f"
expect_output "driver: --group runs one group and prints its count line" "group two: 1 0" \
    drv_run "$f" --group two
expect_red_because "driver: --group refuses a name no definition line holds" "no group 'three'" \
    drv_run "$f" --group three
f=$(drv_fixture keyword 'group_one() {' 'expect green "one passes" true' '}' \
    'function group_two {' 'expect green "two passes" true' '}')
expect_red_because "driver: a group defined as \`function group_x {\` is red, named" \
    "group two is in bash's function table only" drv_run "$f"
f=$(drv_fixture twice 'group_one() {' 'expect green "one passes" true' '}' \
    'group_one() {' 'expect green "one again" true' '}')
expect_red_because "driver: a group defined twice is red, named" "group one is defined twice" drv_run "$f"
f=$(drv_fixture toplevel 'group_one() {' 'expect green "one passes" true' '}' \
    'expect green "a top-level case" true')
expect_red_because "driver: a case outside any group is red, with its label" \
    "the case 'a top-level case' runs outside any group" drv_run "$f"
f=$(drv_fixture nocount 'group_one() {' 'expect green "one passes" true' 'exit 0' '}' \
    'group_two() {' 'expect green "two passes" true' '}')
expect_red_because "driver: a group that prints no count line is red, named" \
    "group one printed 0 count lines" drv_run "$f"
f=$(drv_fixture twocounts 'group_one() {' 'expect green "one passes" true' 'echo "group one: 1 0"' '}')
expect_red_because "driver: a group that prints two count lines is red, named" \
    "group one printed 2 count lines" drv_run "$f"
f=$(drv_fixture contradicts 'group_one() {' 'expect green "one passes" true' 'echo "group one: 1 0"' \
    'exit 1' '}')
expect_red_because "driver: a count its exit status contradicts is red, named" \
    "group one counted 0 failed case(s) but exited 1" drv_run "$f"
f=$(drv_fixture malformed 'group_one() {' 'expect green "one passes" true' 'echo "group one: 1 x"' \
    'exit 1' '}')
expect_red_because "driver: a count line that is no pair of counts is red, named" \
    "group one's count line is malformed: group one: 1 x" drv_run "$f"
f=$(drv_fixture none 'group_one() {' ': no case' '}')
expect_red_because "driver: a group of zero cases is red, named" "group one ran no case" drv_run "$f"
f=$(drv_fixture failing 'group_one() {' 'expect green "one passes" true' '}' \
    'group_two() {' 'expect green "a planted failing case" false' '}')
expect_red_because "driver: one failing case is red, naming it" \
    "expected green, got exit 1 — a planted failing case" drv_run "$f"
f=$(drv_fixture unbuilt 'fx_build_empty() { mkdir -p "$work/empty/crates"; }' \
    'group_one() {' 'fx_build_empty' 'expect red "one reads its own" test -e "$work/empty/nothing"' '}' \
    'group_two() {' 'expect red "two reads another'"'"'s" test -e "$work/empty/nothing"' '}')
expect_red_because "driver: a fixture a group reads but never builds is red, naming both" \
    "group_two: reads \$work/empty, which it neither creates nor builds with fx_build_empty" drv_run "$f"
f=$(drv_fixture built 'fx_build_empty() { mkdir -p "$work/empty/crates"; }' \
    'group_one() {' 'fx_build_empty' 'expect red "one reads its own" test -e "$work/empty/nothing"' '}' \
    'group_two() {' 'fx_build_empty' 'expect red "two reads its own" test -e "$work/empty/nothing"' '}')
expect green "driver: groups that each build what they read pass the lint (control)" drv_run "$f"
# A helper making "$work/$name" makes the names its calls pass, never a word of a
# label or a message: here the helper's own message holds "empty".
f=$(drv_fixture named 'group_one() {' 'ro_fixture() {' 'local name=$1 root' \
    '[ -n "$name" ] && [ -n "$work" ] ||' \
    '{ echo "ro_fixture: empty name or work dir" >&2; exit 2; }' \
    'root="$work/$name"' 'mkdir -p "$root/src"' 'echo "$root"' '}' \
    'root=$(ro_fixture roclean)' 'expect green "one builds roclean" test -d "$root/src"' \
    'expect red "one: a missing directory, empty, is red" test -e "$work/empty/src"' '}')
expect_red_because "driver: a helper making \"\$work/\$name\" makes only what its calls pass" \
    "group_one: reads \$work/empty, which it neither creates nor builds with fx_build_empty" \
    drv_run "$f"
# Every way the lint counts a name as made: each r_ group reads a name its form
# does not make and is refused, each g_ group reads what its form made and runs
# green. Quoted text and heredoc bodies make nothing; a read before the making
# is red; a helper hands a name it was passed on to the helper it calls.
f=$(drv_fixture forms 'mk_whole() { local name=$1; mkdir -p "$work/$name"; }' \
    'mk_pre() { mkdir -p "$work/p-$1"; }' 'mk_suf() { : >"$work/$1.s"; }' \
    'mk_via() { mk_pre "$1"; }' 'rd_pre() { test -e "$work/p-$1"; }' \
    'fx_build_y() { mkdir -p "$work/y"; }' \
    'group_r_whole() {' 'mk_whole other' \
    'expect red "x, which mk_whole x would make" test -e "$work/x"' '}' \
    'group_r_prefix() {' 'mk_pre x' 'expect red "mk_pre x made p-x" test -e "$work/x"' '}' \
    'group_r_suffix() {' 'mk_suf x' 'expect red "mk_suf x made x.s" test -e "$work/x"' '}' \
    'group_r_hand() {' 'mk_via other' 'expect red "mk_via z makes p-z" test -e "$work/p-z"' '}' \
    'group_r_redirect() {' 'printf a >"$work/x.log"' 'expect red "x" test -e "$work/x"' '}' \
    'group_r_mkdir() {' 'mkdir -p "$work/y" && expect red "x" test -e "$work/x"' '}' \
    'group_r_touch() {' 'touch "$work/y" && expect red "x" test -e "$work/x"' '}' \
    'group_r_mktemp() {' \
    'mktemp "$work/x.XXXXXX" >/dev/null && expect red "x" test -e "$work/x"' '}' \
    'group_r_git() {' 'git init -q "$work/y" && expect red "x" test -e "$work/x"' '}' \
    'group_r_copy() {' 'cp -R "$work/x" "$work/y" 2>/dev/null || :' '}' \
    'group_r_assign() {' 'INF_X="$work/x" true' '}' \
    'group_r_builder() {' 'fx_build_y' 'expect red "fx_build_x" test -e "$work/x"' '}' \
    'group_r_late() {' 'expect red "x" test -e "$work/x"' 'mkdir -p "$work/x"' '}' \
    'group_r_reader() {' 'expect red "p-x" rd_pre x' '}' \
    'group_r_glob() {' ': >"$work/o1"' 'expect red "q" ls "$work"/q[12]' '}' \
    'group_r_loop() {' 'for v in a b; do : >"$work/d-$v"; done' \
    'expect red "d-x" test -e "$work/d-x"' '}' \
    'group_r_quoted() {' 'echo '"'"'mkdir -p "$work/x"'"'"' >/dev/null' \
    'expect red "x" test -e "$work/x"' '}' \
    'group_r_heredoc() {' 'cat >/dev/null <<'"'"'H'"'"'' 'mkdir -p "$work/x"' 'H' \
    'expect red "x" test -e "$work/x"' '}' \
    'group_g_whole() {' 'mk_whole x' 'expect green "mk_whole x made x" test -d "$work/x"' '}' \
    'group_g_prefix() {' 'mk_pre x' 'expect green "mk_pre x made p-x" test -d "$work/p-x"' '}' \
    'group_g_suffix() {' 'mk_suf x' 'expect green "mk_suf x made x.s" test -f "$work/x.s"' '}' \
    'group_g_hand() {' 'mk_via z' 'expect green "mk_via z made p-z" test -d "$work/p-z"' '}' \
    'group_g_nested() {' 'mk_in() { mkdir -p "$work/n-$1"; }' 'mk_in x' \
    'expect green "mk_in x made n-x" test -d "$work/n-x"' '}' \
    'group_g_redirect() {' 'printf a >"$work/x"' 'expect green "x" test -f "$work/x"' '}' \
    'group_g_mkdir() {' 'mkdir -p "$work/x" && expect green "x" test -d "$work/x"' '}' \
    'group_g_touch() {' 'touch "$work/x" && expect green "x" test -f "$work/x"' '}' \
    'group_g_mktemp() {' 'mktemp -d "$work/m.XXXXXX" >/dev/null' \
    'expect green "m.*" ls -d "$work"/m.*' '}' \
    'group_g_git() {' 'git init -q "$work/x"' 'expect green "x" test -d "$work/x/.git"' '}' \
    'group_g_copy() {' 'mkdir -p "$work/x"' 'cp -R "$work/x" "$work/y"' \
    'expect green "y" test -d "$work/y"' '}' \
    'group_g_assign() {' 'local d="$work/x"' 'mkdir -p "$d"' \
    'expect green "x" test -d "$work/x"' '}' \
    'group_g_builder() {' 'fx_build_y' 'expect green "fx_build_y made y" test -d "$work/y"' '}' \
    'group_g_reader() {' 'mk_pre x' 'expect green "p-x" rd_pre x' '}' \
    'group_g_glob() {' ': >"$work/o1"' ': >"$work/o2"' \
    'expect green "o1, o2" ls "$work"/o[12]' '}' \
    'group_g_loop() {' \
    'for v in a b; do : >"$work/d-$v"; expect green "d-$v" test -f "$work/d-$v"; done' '}' \
    'group_g_log() {' 'expect green "a case" true' 'expect green "its log" test -f "$work/log"' '}')
drv_forms_red() { # the lint is red, naming each r_ group once and no g_ group
    local out g n=0
    out=$(drv_run "$1" --lint 2>&1) && { printf '%s\n' "$out"; return 1; }
    printf '%s\n' "$out"
    for g in $(sed -n 's/^group_\(r_[a-z_]*\)() {$/\1/p' "$1"); do
        n=$((n + 1))
        [ "$(grep -c "^SELFTEST LINT: group_$g: " <<<"$out")" -eq 1 ] || {
            echo "group_$g is not named once"
            return 1
        }
    done
    [ "$n" -eq 18 ] && ! grep -q '^SELFTEST LINT: group_g_' <<<"$out" &&
        grep -qx "selftest lint: .* calls followed, $n unbuilt fixture(s)" <<<"$out"
}
expect green "driver: the lint refuses the unbuilt read of each of 18 forms, naming its group" \
    drv_forms_red "$f"
drv_forms_green() { # each g_ group runs green: what the lint counts as made exists
    local g n=0
    for g in $(sed -n 's/^group_\(g_[a-z_]*\)() {$/\1/p' "$1"); do
        n=$((n + 1))
        drv_run "$1" --group "$g" >"$work/drv-forms-$g.log" 2>&1 &&
            grep -q "^group $g: [1-9][0-9]* 0$" "$work/drv-forms-$g.log" || {
            cat "$work/drv-forms-$g.log"
            return 1
        }
    done
    [ "$n" -eq 17 ]
}
expect green "driver: each of 17 forms the lint counts as making makes what it reads (controls)" \
    drv_forms_green "$f"
# Two groups at once: each waits until the other has started (so they overlap),
# then finds its own fixture and not the other's, and its own case log only.
f=$(drv_fixture overlap \
    'drv_meet() { : >"$DRV_SYNC/$1"; n=0; while [ ! -e "$DRV_SYNC/$2" ] && [ "$n" -lt 100 ]; do sleep 0.1; n=$((n + 1)); done; [ -e "$DRV_SYNC/$2" ]; }' \
    'group_one() {' 'printf one >"$work/mine"' 'expect green "one overlaps two" drv_meet one two' \
    'expect green "one sees its own fixture" grep -qx one "$work/mine"' '}' \
    'group_two() {' 'printf two >"$work/mine"' 'expect green "two overlaps one" drv_meet two one' \
    'expect green "two sees its own fixture" grep -qx two "$work/mine"' '}')
mkdir -p "$work/drv-sync" "$work/drv-log"
expect_output "driver: two groups at once each see only their own fixture" \
    "check-scripts self-test OK (4 cases in 2 groups" \
    env DRV_SYNC="$work/drv-sync" INF_SELFTEST_CASE_LOG="$work/drv-log" CARGO_BUILD_JOBS=2 "$f"
drv_own_log() { # each group's records hold its own labels only
    [ "$(cat "$work/drv-log/one/"* | grep -c '^label	one ')" -eq 2 ] &&
        [ "$(cat "$work/drv-log/two/"* | grep -c '^label	two ')" -eq 2 ] &&
        ! cat "$work/drv-log/one/"* | grep -q '^label	two ' &&
        ! cat "$work/drv-log/two/"* | grep -q '^label	one '
}
expect green "driver: two groups at once each write only their own case log" drv_own_log
# A group's seconds come from the clock line its process prints; the OK line
# names the longest. A fixture clock that answers by the group, and by whether
# a case has run yet, makes the seconds exact (tenths of a second).
drv_only() { # <fixture> <line>: the driver exits 0 and prints that line alone
    local out
    out=$(drv_run "$1" 2>&1) || { printf '%s\n' "$out"; return 1; }
    printf '%s\n' "$out"
    [ "$out" = "$2" ]
}
drv_clocked() { # <name> <begin> <end>: one green group; its clock reads <begin>, then <end>
    drv_fixture "$1" 'group_one() {' 'expect green "one passes" true' '}' \
        "selftest_clock() { if [ \$((pass + fail)) -eq 0 ]; then echo $2; else echo $3; fi; }"
}
drv_pair() { # <name> <one's end> <two's end>: two green groups, each clock 100, then its end
    drv_fixture "$1" 'group_one() {' 'expect green "one passes" true' '}' \
        'group_two() {' 'expect green "two passes" true' '}' \
        'selftest_clock() { if [ $((pass + fail)) -eq 0 ]; then echo 100;' \
        "else case \$SELFTEST_GROUP in one) echo $2 ;; *) echo $3 ;; esac; fi; }"
}
drv_ok='check-scripts self-test OK (2 cases in 2 groups, each exiting non-zero on its planted case:'
# The longest group is the last read in one fixture and the first in the other,
# so a driver that names the last group, or the first, is red on one of them.
f=$(drv_pair clocked 105 130)
expect green "driver: the OK line, alone, names the longest group with its seconds" drv_only "$f" \
    "$drv_ok one, two); the longest group: two, 3.0 s"
f=$(drv_pair clocked-first 130 105)
expect green "driver: the longest group is named when it is the first read" drv_only "$f" \
    "$drv_ok one, two); the longest group: one, 3.0 s"
f=$(drv_clocked instant 100 100)
expect_output "driver: a group of no measurable time is still named, at 0.0 s" \
    "; the longest group: one, 0\.0 s\$" drv_run "$f"
drv_tenths() { # the lib's clock: tenths of the monotonic seconds read between two of its reads
    local before seconds after
    before=$(selftest_clock)
    seconds=$(python3 -I -S -c 'import time; print(time.monotonic())')
    after=$(selftest_clock)
    echo "clock $before, monotonic seconds $seconds, clock $after"
    awk -v b="$before" -v s="$seconds" -v a="$after" \
        'BEGIN { t = int(s * 10); exit !(b <= t && t <= a) }'
}
expect green "driver: the group clock reads the monotonic clock in tenths of a second" drv_tenths
drv_clock_red="group one's clock line is not one line of two stamps in order:"
f=$(drv_fixture noclock 'group_one() {' 'expect green "one passes" true' 'echo "group one: 1 0"' \
    'exit 0' '}')
expect_red_because "driver: a group that prints no clock line is red, named" "$drv_clock_red none" \
    drv_run "$f"
f=$(drv_fixture redonce 'group_one() {' 'expect green "one passes" true' 'echo "group one: 1 0"' \
    'exit 1' '}')
expect_red_because "driver: a group red by its count is counted once, its clock line unread" \
    "check-scripts self-test FAILED: 0 of 1 cases failed; 1 of 1 groups red; 0 driver error(s)" \
    drv_run "$f"
f=$(drv_fixture twoclocks 'group_one() {' 'expect green "one passes" true' \
    'echo "group one clock: 100 105"' '}')
expect_red_because "driver: a group that prints two clock lines is red, named" \
    "$drv_clock_red group one clock: 100 105" drv_run "$f"
f=$(drv_clocked octal 08 100)
expect_red_because "driver: a begin stamp with a leading zero is red, named" \
    "$drv_clock_red group one clock: 08 100" drv_run "$f"
f=$(drv_clocked notnumber 100 1e3)
expect_red_because "driver: an end stamp that is no whole number is red, named" \
    "$drv_clock_red group one clock: 100 1e3" drv_run "$f"
f=$(drv_clocked backwards 105 100)
expect_red_because "driver: a clock that runs backwards is red, named" \
    "$drv_clock_red group one clock: 105 100" drv_run "$f"
f=$(drv_clocked signed 100 +105)
expect_red_because "driver: a signed end stamp is red, named" \
    "$drv_clock_red group one clock: 100 +105" drv_run "$f"
# A clock line of one stamp: the group's count is green, so its clock line
# alone makes it red, and the tally counts that group once.
f=$(drv_fixture onestamp 'group_one() {' 'expect green "one passes" true' 'echo "group one: 1 0"' \
    'echo "group one clock: 100"' 'exit 0' '}')
expect_red_because "driver: a clock line of one stamp is red, named" \
    "$drv_clock_red group one clock: 100\$" drv_run "$f"
expect_red_because "driver: a group red by its clock line alone is one red group, no failed case" \
    "check-scripts self-test FAILED: 0 of 1 cases failed; 1 of 1 groups red; 0 driver error(s)" \
    drv_run "$f"
# The driver's sort, uniq and comm run under LC_ALL=C: stand-ins that answer
# `collation_differs` under any other locale setting would turn its control red.
mkdir -p "$work/drv-collate"
for drv_tool in sort uniq comm; do
    printf '#!/bin/sh\n[ "${LC_ALL-}" = C ] && exec %s "$@"\necho collation_differs\n' \
        "$(command -v "$drv_tool")" >"$work/drv-collate/$drv_tool"
    chmod +x "$work/drv-collate/$drv_tool"
done
drv_stand_ins() { # each stand-in answers otherwise outside LC_ALL=C, so the plant is live
    local tool
    for tool in sort uniq comm; do
        [ "$(LC_ALL=POSIX "$work/drv-collate/$tool" /dev/null /dev/null)" = collation_differs ] ||
            return 1
    done
}
expect green "driver: the collation stand-ins answer otherwise outside LC_ALL=C (control)" \
    drv_stand_ins
drv_collated() ( # <fixture>: the driver beside the stand-ins, its caller's locale POSIX
    PATH="$work/drv-collate:$PATH" LC_ALL=POSIX
    export PATH LC_ALL
    drv_run "$1"
)
f=$(drv_fixture collate 'group_one() {' 'expect green "one passes" true' '}' \
    'group_two() {' 'expect green "two passes" true' '}')
expect_output "driver: sort, uniq and comm read one collation whatever the caller's locale" \
    "check-scripts self-test OK (2 cases in 2 groups" drv_collated "$f"
}

# The gates' crate set (scripts/cell-crates.sh) names exclusions that must
# exist; a fixture root carries each of them as an empty `src/` so the
# self-test is independent of which crates are excluded today.
# shellcheck source=cell-crates.sh
. "$SCRIPT_DIR/cell-crates.sh"

# fixture <name> <rust-source…>: a fresh root with one crate, `src/lib.rs`
# from stdin, plus the excluded directories.
fixture() {
    local name=$1 root entry
    [ -n "$name" ] && [ -n "$work" ] || { echo "fixture: empty name or work dir" >&2; exit 2; }
    root="$work/$name"
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/fake/src"
    for entry in "${CELL_CRATE_EXCLUDE[@]}"; do
        [ -n "${entry%%|*}" ] && mkdir -p "$root/${entry%%|*}"
    done
    cat >"$root/crates/fake/src/lib.rs"
    echo "$root"
}

# fx_build_empty: an empty root with an empty crates/, the shape every gate's
# scope assertion refuses; each group that reads it builds it.
fx_build_empty() { mkdir -p "$work/empty/crates"; }

# fx_root_key <dir>: the target-root key of a checkout at <dir>, the first 16
# hex digits of SHA-256 of its physical path, read by coreutils' sha256sum or
# perl's shasum: an oracle that shares no code with scripts/target-root.sh.
fx_root_key() {
    local phys
    phys=$(cd "$1" && pwd -P) || return 1
    if command -v sha256sum >/dev/null 2>&1; then
        printf '%s' "$phys" | sha256sum | cut -c1-16
    else
        printf '%s' "$phys" | shasum -a 256 | cut -c1-16
    fi
}

# ---------------------------------------------------------------- deny-list
group_deny_list() {
DENY=./scripts/check-cell-denylist.sh

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() -> u64 { 1 }
EOF
root=$(printf '%s' "$body" | fixture clean)
expect green "deny-list: clean crate" env INF_CHECK_ROOT="$root" $DENY
expect_output "deny-list: scope line discloses the scan" "1 crates, 1 files, 1 lines scanned" env INF_CHECK_ROOT="$root" $DENY

# The P1 shape: the configured set resolves to nothing.
fx_build_empty
expect red "deny-list: no crates at all is a failure, not OK" env INF_CHECK_ROOT="$work/empty" $DENY

# A stale exclusion (a path that evaporated) is a failure.
IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}
EOF
root=$(printf '%s' "$body" | fixture stale)
first=${CELL_CRATE_EXCLUDE[0]%%|*}
[ -n "$first" ] && [ -n "$root" ] && [ -d "$root/$first" ] && rm -rf "$root/$first"
expect red "deny-list: exclusion naming a missing directory fails" env INF_CHECK_ROOT="$root" $DENY

# Each banned family, planted in production code.
for snippet in \
    'pub fn t() -> std::time::Instant { std::time::Instant::now() }' \
    'pub fn t() -> u64 { std::time::SystemTime::now(); 0 }' \
    'pub fn t() { std::thread::spawn(|| {}); }' \
    'pub fn t() { let _ = std::thread::Builder::new(); }' \
    'pub fn t() { std::thread::park(); }' \
    'pub fn t() { std::thread::sleep(std::time::Duration::from_millis(1)); }' \
    'use std::sync::mpsc; pub fn t() { let _ = mpsc::channel::<u8>(); }' \
    'pub fn t() { let _ = std::sync::Mutex::new(0); }' \
    'pub fn t() { let _ = std::sync::Condvar::new(); }' \
    'pub fn t() { tokio::spawn(async {}); }' \
    'pub fn t() -> u8 { rand::random() }'
do
    root=$(fixture planted <<<"$snippet")
    expect red "deny-list: planted '$snippet'" env INF_CHECK_ROOT="$root" $DENY
done

# The same hit inside a test-only module is not cell code.
IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}

#[cfg(test)]
mod tests {
    fn scratch() -> u128 {
        std::time::SystemTime::now().elapsed().unwrap().as_nanos()
    }
}
EOF
root=$(printf '%s' "$body" | fixture testmod)
expect green "deny-list: wall clock inside #[cfg(test)] mod tests is stripped" env INF_CHECK_ROOT="$root" $DENY

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}

#[cfg(all(test, not(loom)))]
mod tests {
    fn t() { std::thread::spawn(|| {}); }
}
EOF
root=$(printf '%s' "$body" | fixture loommod)
expect green "deny-list: #[cfg(all(test, …))] module is stripped" env INF_CHECK_ROOT="$root" $DENY

# `any(test, feature)` is NOT test-only: it compiles under the feature.
IFS= read -r -d '' body <<'EOF' || true
#[cfg(any(test, feature = "probe"))]
mod probe {
    pub fn t() { std::thread::spawn(|| {}); }
}
EOF
root=$(printf '%s' "$body" | fixture anymod)
expect red "deny-list: #[cfg(any(test, feature))] module is scanned" env INF_CHECK_ROOT="$root" $DENY

# The P1c shape, applied here: an inline #[cfg(test)] item must not swallow
# the rest of the file.
IFS= read -r -d '' body <<'EOF' || true
pub struct S;
impl S {
    #[cfg(test)]
    pub fn peek(&self) -> u8 { 0 }
}
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
root=$(printf '%s' "$body" | fixture inline)
expect red "deny-list: a violation after an inline #[cfg(test)] item is still seen" env INF_CHECK_ROOT="$root" $DENY

# Sanctioned sites: the marker with a reason, on the line or the one above.
IFS= read -r -d '' body <<'EOF' || true
pub fn t() -> std::time::Instant { std::time::Instant::now() } // denylist-allow: fixture reason
EOF
root=$(printf '%s' "$body" | fixture allowsame)
expect green "deny-list: marker with a reason on the same line" env INF_CHECK_ROOT="$root" $DENY
expect_output "deny-list: allowed sites are listed" "allowed crates/fake/src/lib.rs:1: fixture reason" env INF_CHECK_ROOT="$root" $DENY

IFS= read -r -d '' body <<'EOF' || true
// denylist-allow: fixture reason on the line above
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
root=$(printf '%s' "$body" | fixture allowabove)
expect green "deny-list: marker with a reason on the line above" env INF_CHECK_ROOT="$root" $DENY

IFS= read -r -d '' body <<'EOF' || true
pub fn t() -> std::time::Instant { std::time::Instant::now() } // denylist-allow
EOF
root=$(printf '%s' "$body" | fixture allowbare)
expect red "deny-list: a bare marker without a reason fails" env INF_CHECK_ROOT="$root" $DENY

IFS= read -r -d '' body <<'EOF' || true
// denylist-allow: two lines up does not count
//
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
root=$(printf '%s' "$body" | fixture allowfar)
expect red "deny-list: a marker two lines above does not apply" env INF_CHECK_ROOT="$root" $DENY

# A `mod name;` under #[cfg(test)] makes the named file test-only.
IFS= read -r -d '' body <<'EOF' || true
#[cfg(test)]
mod scratch;
pub fn ok() {}
EOF
root=$(printf '%s' "$body" | fixture modfile)
echo 'pub fn t() -> std::time::Instant { std::time::Instant::now() }' >"$root/crates/fake/src/scratch.rs"
expect green "deny-list: a #[cfg(test)] mod file is test-only" env INF_CHECK_ROOT="$root" $DENY

# A test module whose closing brace never comes back to its indent would
# blank the rest of the file: that is a scope error, not a pass.
IFS= read -r -d '' body <<'EOF' || true
#[cfg(test)]
mod tests {
    fn t() {}
  }
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
root=$(printf '%s' "$body" | fixture unterminated)
expect red "deny-list: an unterminated test module is a scope error" env INF_CHECK_ROOT="$root" $DENY
}

# --------------------------------------------------------------- panic policy
group_panic_policy() {
PANIC=./scripts/check-panic-policy.sh

IFS= read -r -d '' body <<'EOF' || true
pub fn ok(v: Option<u8>) -> u8 { v.unwrap_or(0) }
pub fn ok2(v: Option<u8>) -> u8 { v.unwrap_or_default() }
pub fn ok3(v: Option<u8>) -> u8 { v.expect("invariant: caller checked") }
/// Docs may say `.unwrap()` without being code.
pub fn ok4() {}
EOF
root=$(printf '%s' "$body" | fixture pclean)
expect green "panic-policy: unwrap_or / expect / doc-comment unwrap are fine" env INF_CHECK_ROOT="$root" $PANIC

for snippet in \
    'pub fn t(v: Option<u8>) -> u8 { v.unwrap() }' \
    'pub fn t() { todo!() }' \
    'pub fn t() { unimplemented!() }' \
    'pub fn t(a: Option<u8>, b: Option<u8>) -> u8 { a.unwrap_or(0) + b.unwrap() }'
do
    root=$(fixture pplanted <<<"$snippet")
    expect red "panic-policy: planted '$snippet'" env INF_CHECK_ROOT="$root" $PANIC
done

# The P1c shape exactly: an inline #[cfg(test)] accessor, then a naked
# unwrap further down the same file.
IFS= read -r -d '' body <<'EOF' || true
pub struct S { mode: u8 }
impl S {
    #[cfg(test)]
    pub fn mode(&self) -> u8 { self.mode }
}
pub fn t(v: Option<u8>) -> u8 { v.unwrap() }
EOF
root=$(printf '%s' "$body" | fixture p1c)
expect red "panic-policy: the ckpt.rs shape (inline cfg(test) then unwrap) is caught" env INF_CHECK_ROOT="$root" $PANIC
expect_output "panic-policy: inline items are disclosed" "1 inline cfg(test) items scanned as production" env INF_CHECK_ROOT="$root" $PANIC

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}

#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _ = Some(1u8).unwrap(); }
}

#[cfg(all(test, not(loom)))]
mod more {
    fn t() { let _ = Some(1u8).unwrap(); }
}
EOF
root=$(printf '%s' "$body" | fixture ptest)
expect green "panic-policy: unwrap inside test-only modules is stripped" env INF_CHECK_ROOT="$root" $PANIC
expect_output "panic-policy: stripped lines are disclosed" "9 test-only lines stripped" env INF_CHECK_ROOT="$root" $PANIC

IFS= read -r -d '' body <<'EOF' || true
// panic-policy-allow: fixture reason
pub fn t(v: Option<u8>) -> u8 { v.unwrap() }
EOF
root=$(printf '%s' "$body" | fixture pallow)
expect green "panic-policy: marker with a reason on the line above" env INF_CHECK_ROOT="$root" $PANIC

IFS= read -r -d '' body <<'EOF' || true
pub fn t(v: Option<u8>) -> u8 { v.unwrap() } // panic-policy-allow
EOF
root=$(printf '%s' "$body" | fixture pbare)
expect red "panic-policy: a bare marker without a reason fails" env INF_CHECK_ROOT="$root" $PANIC

fx_build_empty
expect red "panic-policy: no crates at all is a failure, not OK" env INF_CHECK_ROOT="$work/empty" $PANIC
}

# ------------------------------------------------------------ reply outcome
group_reply_outcome() {
# ADR-0190 D4, A2: no production site of crates/inf-server/src reads a
# reply's outcome from its bytes, or names a fabric reply tag outside the
# functions that set and read it. Each fixture is one inf-server lib.rs, with
# a test-only modfile or a row's file beside it where the case needs one.
REPLY=./scripts/check-reply-outcome.sh
ro_fixture() {
    local name=$1 root
    [ -n "$name" ] && [ -n "$work" ] || { echo "ro_fixture: empty name or work dir" >&2; exit 2; }
    root="$work/$name"
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/inf-server/src"
    cat >"$root/crates/inf-server/src/lib.rs"
    echo "$root"
}
# ro_put <root> <path>: one more file of a fixture ro_fixture made, at <path>
# under its crates/inf-server/src, from stdin; it removes nothing.
ro_put() {
    local root=$1 path=$2
    case $root in
    "$work"/?*) ;;
    *) echo "ro_put: '$root' is not a fixture directory under $work" >&2; exit 2 ;;
    esac
    [ -n "$path" ] && [ -d "$root/crates/inf-server/src" ] ||
        { echo "ro_put: no path or no fixture at '$root'" >&2; exit 2; }
    mkdir -p "$(dirname "$root/crates/inf-server/src/$path")"
    cat >"$root/crates/inf-server/src/$path"
}

IFS= read -r -d '' body <<'EOF' || true
pub fn ok(bytes: &[u8], digits: &[u8], pattern: &[u8], c: u8) -> bool {
    let sign = match c {
        b'-' => -1,
        b'*' => 0,
        _ => 1,
    };
    sign == 0 && bytes == b"-" && digits[0] == b'0' && pattern.first() == Some(&b'^')
}
pub const N: &[u8] = b"-1";
EOF
root=$(printf '%s' "$body" | ro_fixture roclean)
expect green "reply-outcome: the sign arm, an argument compare, a digit, a non-tag byte and b\"-1\" are clean" \
    env INF_CHECK_ROOT="$root" $REPLY
expect_output "reply-outcome: the scope line discloses the scan" "1 files, 9 lines scanned" \
    env INF_CHECK_ROOT="$root" $REPLY

for snippet in \
    "pub fn t(r: &[u8]) -> bool { r.first() == Some(&b'-') }" \
    "pub fn t(r: &[u8]) -> bool { r.first() == Some(&b'!') }" \
    "pub fn t(r: &[u8]) -> bool { r.first() == Some(&b'+') }" \
    "pub fn t(r: &[u8]) -> bool { matches!(r.first().copied(), Some(b'-')) }" \
    "pub fn t(r: &[u8]) -> bool { matches!(r, [b'-', ..]) }" \
    "pub fn t(r: &[u8]) -> bool { matches!(r.first().copied(), Some(b'-' | b'!')) }" \
    "pub fn t(r: &[u8]) -> bool { matches!(r, [b'-' | b'!', ..]) }" \
    "pub fn t(r: &[u8]) -> bool { b'-' == r[0] }" \
    "pub fn t(r: &[u8]) -> bool { b'-' != r[0] }" \
    "pub fn t(r: &[u8]) -> bool { r[0] == b'*' }" \
    "pub fn t(r: &[u8]) -> bool { r[0] != b'-' }" \
    "pub fn t(r: &[u8]) -> bool { r.starts_with(b\"-ERR\") }" \
    "pub fn t(r: &[u8]) -> Option<&[u8]> { r.strip_prefix(b\"*1\") }" \
    "pub const E: &[u8] = b\"-ERR planted\\r\\n\";" \
    "pub fn t(out: &mut Vec<u8>) { out.push(b'-') }" \
    "pub fn t(b: Vec<u8>) { let _ = inf_wire::OwnedReply::received(b, inf_wire::ReplyOutcome::Error); }"
do
    root=$(ro_fixture roplanted <<<"$snippet")
    expect_red_because "reply-outcome: planted '$snippet'" "REPLY-OUTCOME violation (an outcome" \
        env INF_CHECK_ROOT="$root" $REPLY
done

IFS= read -r -d '' body <<'EOF' || true
pub fn t(r: &[u8]) -> bool {
    // reply-outcome-allow: a fixture's sanctioned value decoder
    r.first() == Some(&b'-')
}
EOF
root=$(printf '%s' "$body" | ro_fixture roallow)
expect green "reply-outcome: a marked site is green" env INF_CHECK_ROOT="$root" $REPLY
expect_output "reply-outcome: a marked site is listed with its reason" \
    "a fixture's sanctioned value decoder" env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub fn t(r: &[u8]) -> bool {
    r.first() == Some(&b'-') // reply-outcome-allow
}
EOF
root=$(printf '%s' "$body" | ro_fixture robare)
expect_red_because "reply-outcome: a bare marker without a reason fails" "marker without a reason" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub fn t(r: &[u8]) -> bool {
    // reply-outcome-allow: guards nothing below
    r.is_empty()
}
EOF
root=$(printf '%s' "$body" | ro_fixture rostale)
expect_red_because "reply-outcome: a marker guarding no hit fails" "a marker guarding no hit" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}

#[cfg(test)]
mod tests {
    fn t(r: &[u8]) -> bool { r.first() == Some(&b'-') }
}

#[cfg(test)]
#[path = "fixture_tests.rs"]
mod fixture_tests;
EOF
root=$(printf '%s' "$body" | ro_fixture rotest)
printf '%s\n' "pub fn t(r: &[u8]) -> bool { r.first() == Some(&b'-') }" \
    >"$root/crates/inf-server/src/fixture_tests.rs"
expect green "reply-outcome: a test module and a test-only modfile are out of scope" \
    env INF_CHECK_ROOT="$root" $REPLY
expect_output "reply-outcome: the excluded modfile is disclosed" "1 test-only files excluded" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
/// A doc line that says `r.first() == Some(&b'-')` is not code.
pub fn ok() {}
EOF
root=$(printf '%s' "$body" | ro_fixture rodoc)
expect green "reply-outcome: a comment naming a shape is not a site" env INF_CHECK_ROOT="$root" $REPLY

mkdir -p "$work/roempty/crates/inf-server/src"
expect_red_because "reply-outcome: an empty file set is a scope error" "REPLY-OUTCOME SCOPE ERROR" \
    env INF_CHECK_ROOT="$work/roempty" $REPLY
fx_build_empty
expect red "reply-outcome: a missing directory is a scope error" env INF_CHECK_ROOT="$work/empty" $REPLY

# ADR-0190 A2: the fabric's reply tag meets a reply's bytes only in the
# functions that set and read it. R1 is a path through `Outcome` to anything
# but a non-reply variant, R2 a path through `ReplyOutcome`, R3 another name
# for either enum; no marker sanctions one. Each red case names its message.
RO_TAG="REPLY-OUTCOME violation (a reply tag outside its functions)"
RO_NAME="REPLY-OUTCOME violation (another name for a reply tag's enum)"
for snippet in \
    "pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> { inf_fabric::Outcome::Bytes(b) }" \
    "pub fn t(b: Option<&[u8]>) -> Option<Outcome<'_>> { b.map(Outcome::ErrorReply) }" \
    "pub fn t(o: &Outcome<'_>) -> bool { matches!(o, Outcome::ErrorReply(_)) }" \
    "pub fn t(b: &[u8]) -> Outcome<'_> { Outcome :: Bytes (b) }" \
    "pub fn t(b: &[u8]) -> Outcome<'_> { Outcome::r#Bytes(b) }" \
    "pub fn t(b: &[u8]) -> Outcome<'_> { Outcome::<'_>::Bytes(b) }" \
    "use inf_fabric::Outcome::*;" \
    "use inf_fabric::Outcome::{Bytes};" \
    "use inf_fabric::Outcome::Bytes as Tag;" \
    "pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> { inf_fabric::Outcome::from_reply(b) }" \
    "pub fn t(b: &[u8]) -> Outcome<'_> { wire_reply(b, ReplyOutcome::Success) }" \
    "pub fn t(n: usize) -> StagedReply { StagedReply::Bytes(0, n, ReplyOutcome::Error) }" \
    "use inf_wire::ReplyOutcome::*;" \
    "pub fn t(b: &[u8]) -> inf_wire::ReplyOutcome { inf_wire::ReplyOutcome::from_byte(b[0]) }" \
    "pub fn t(ok: bool) { debug_assert!(ok, \"an Outcome::Bytes leg\"); }"
do
    root=$(ro_fixture rotag <<<"$snippet")
    expect_red_because "reply-outcome: a reply tag, planted '$snippet'" "$RO_TAG" \
        env INF_CHECK_ROOT="$root" $REPLY
done
for snippet in \
    "pub fn t(b: &'static [u8]) -> Outcome<'static> { <inf_fabric::Outcome<'_> as From<&[u8]>>::from(b) }" \
    "type Wire<'a> = inf_fabric::Outcome<'a>;" \
    "pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> { <inf_fabric::Outcome<'_>>::Bytes(b) }" \
    "use inf_wire::ReplyOutcome as Tag;" \
    "type Tag = inf_wire::ReplyOutcome;"
do
    root=$(ro_fixture roname <<<"$snippet")
    expect_red_because "reply-outcome: another name, planted '$snippet'" "$RO_NAME" \
        env INF_CHECK_ROOT="$root" $REPLY
done

IFS= read -r -d '' body <<'EOF' || true
use inf_fabric::Outcome;
pub fn t(b: &[u8]) -> Outcome<'_> {
    Outcome::ErrorReply(b)
}
EOF
root=$(printf '%s' "$body" | ro_fixture roimported)
expect_red_because "reply-outcome: a hand-built ErrorReply after the enum's import" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
macro_rules! tag {
    ($v:ident, $b:expr) => {
        inf_fabric::Outcome::$v($b)
    };
}
EOF
root=$(printf '%s' "$body" | ro_fixture ropaste)
expect_red_because "reply-outcome: a macro pasting a variant after the enum's path" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> {
    inf_fabric::Outcome::
        Bytes(b)
}
EOF
root=$(printf '%s' "$body" | ro_fixture rosplit)
expect_red_because "reply-outcome: a path split after the enum's ::" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}

#[cfg(test)]
fn t(b: &[u8]) -> inf_fabric::Outcome<'_> {
    inf_fabric::Outcome::Bytes(b)
}
EOF
root=$(printf '%s' "$body" | ro_fixture roinline)
expect_red_because "reply-outcome: an inline #[cfg(test)] function is production" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
use inf_fabric::{Op, Outcome as Wire};
pub fn t(b: &[u8]) -> Wire<'_> {
    Wire::Bytes(b)
}
EOF
root=$(printf '%s' "$body" | ro_fixture rorename)
expect_red_because "reply-outcome: a rename of the enum" "$RO_NAME" env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
trait Tag {
    fn tag(b: &'static [u8]) -> Self;
}
impl Tag for inf_fabric::Outcome<'static> {
    fn tag(b: &'static [u8]) -> Self {
        Self::Bytes(b)
    }
}
EOF
root=$(printf '%s' "$body" | ro_fixture roimpl)
expect_red_because "reply-outcome: an impl of a local trait for the enum" "$RO_NAME" \
    env INF_CHECK_ROOT="$root" $REPLY

# rustfmt splits an impl header past 100 columns: `for E` on its own line.
IFS= read -r -d '' body <<'EOF' || true
trait AVeryLongTraitNameThatTagsAFabricReplyFromItsOwnBytesByHand<'a> {
    fn tag(b: &'a [u8]) -> Self;
}
impl<'a> AVeryLongTraitNameThatTagsAFabricReplyFromItsOwnBytesByHand<'a>
    for inf_fabric::Outcome<'a>
{
    fn tag(b: &'a [u8]) -> Self {
        Self::Bytes(b)
    }
}
EOF
root=$(printf '%s' "$body" | ro_fixture roimplsplit)
expect_red_because "reply-outcome: an impl for the enum, its header split by rustfmt" "$RO_NAME" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
macro_rules! tag {
    ($e:ident, $b:expr) => {
        $e::Bytes($b)
    };
}
pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> {
    use inf_fabric::Outcome;
    tag!(Outcome, b)
}
EOF
root=$(printf '%s' "$body" | ro_fixture rofragment)
expect_red_because "reply-outcome: a fragment pasted before a reply variant's name" "$RO_NAME" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
macro_rules! tag {
    ($e:ident, $v:ident, $b:expr) => {
        $e::$v($b)
    };
}
pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> {
    use inf_fabric::Outcome;
    tag!(Outcome, Bytes, b)
}
EOF
root=$(printf '%s' "$body" | ro_fixture rofragments)
expect_red_because "reply-outcome: a fragment pasted before another fragment" "$RO_NAME" \
    env INF_CHECK_ROOT="$root" $REPLY

# ro_a2_only <root>: red on reply-tag lines alone: A2's two trailer lines, and
# neither D4's marker sentence nor a stale-marker line.
ro_a2_only() {
    local status=0
    env INF_CHECK_ROOT="$1" $REPLY >"$work/ro-a2.log" 2>&1 || status=$?
    if [ "$status" -ne 0 ] &&
        grep -qF "Tag a reply with wire_reply(bytes, reply.outcome()); read an outcome with is_error()." \
            "$work/ro-a2.log" &&
        grep -qF "A new site that names a reply tag changes ADR-0190 D3 and A2; no marker sanctions it." \
            "$work/ro-a2.log" &&
        ! grep -qF "a sanctioned site carries" "$work/ro-a2.log" &&
        ! grep -qF "a marker guarding no hit" "$work/ro-a2.log"; then
        return 0
    fi
    echo "exit $status"
    cat "$work/ro-a2.log"
    return 1
}
IFS= read -r -d '' body <<'EOF' || true
pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> {
    // reply-outcome-allow: a fixture's reason
    inf_fabric::Outcome::Bytes(b)
}
EOF
root=$(printf '%s' "$body" | ro_fixture romarked)
expect_red_because "reply-outcome: a marker above a reply tag leaves it red" \
    "a marker does not sanction a reply tag" env INF_CHECK_ROOT="$root" $REPLY
expect green "reply-outcome: a marker above a reply tag is not stale and sends no one to a marker" \
    ro_a2_only "$root"
root=$(ro_fixture rotrailer <<<"pub fn t(b: &[u8]) -> inf_fabric::Outcome<'_> { inf_fabric::Outcome::Bytes(b) }")
expect green "reply-outcome: a reply tag alone prints A2's trailer and not D4's marker sentence" \
    ro_a2_only "$root"

# ro_both <root>: a line that is both a byte shape and a reply tag prints both
# violations and both trailers.
ro_both() {
    local status=0
    env INF_CHECK_ROOT="$1" $REPLY >"$work/ro-both.log" 2>&1 || status=$?
    if [ "$status" -ne 0 ] &&
        grep -qF "REPLY-OUTCOME violation (an outcome read from a reply's bytes)" "$work/ro-both.log" &&
        grep -qF "$RO_TAG" "$work/ro-both.log" &&
        grep -qF "a sanctioned site carries" "$work/ro-both.log" &&
        grep -qF "no marker sanctions it" "$work/ro-both.log"; then
        return 0
    fi
    echo "exit $status"
    cat "$work/ro-both.log"
    return 1
}
root=$(ro_fixture roboth <<<"pub fn t(b: Vec<u8>) { let _ = inf_wire::OwnedReply::received(b, inf_wire::ReplyOutcome::Error); }")
expect green "reply-outcome: a received( line with a chosen outcome prints both rules' messages" \
    ro_both "$root"

# The rows: plane/fabric.rs's wire_reply and owned_outcome, plane/scatter.rs's
# stages, each from its declaration at column 0 to the next line that begins
# with `}`. The bodies below are the tree's.
IFS= read -r -d '' RO_WIRE <<'EOF' || true
pub(super) fn wire_reply(bytes: &[u8], outcome: ReplyOutcome) -> Outcome<'_> {
    match outcome {
        ReplyOutcome::Success => Outcome::Bytes(bytes),
        ReplyOutcome::Error if cfg!(inf_canary_fabric_error_as_bytes) => Outcome::Bytes(bytes),
        ReplyOutcome::Error => Outcome::ErrorReply(bytes),
    }
}
EOF
IFS= read -r -d '' RO_STAGED <<'EOF' || true
pub(super) fn staged_outcome<'a>(scratch: &'a [u8], reply: &StagedReply) -> Outcome<'a> {
    match *reply {
        StagedReply::Bytes(start, end, outcome) => wire_reply(&scratch[start..end], outcome),
        StagedReply::Int(n) => Outcome::Int(n),
        StagedReply::Nil => Outcome::Nil,
        StagedReply::Ok => Outcome::Ok,
        StagedReply::Refused => Outcome::Err(ErrCode::Unknown(0)),
    }
}
EOF
IFS= read -r -d '' RO_OWNED <<'EOF' || true
fn owned_outcome<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    outcome: Outcome<'_>,
) -> OwnedOutcome {
    match outcome {
        Outcome::Bytes(bytes) => received_reply(shared, bytes, ReplyOutcome::Success),
        Outcome::ErrorReply(bytes) => received_reply(shared, bytes, ReplyOutcome::Error),
        Outcome::Ok => OwnedOutcome::Ok,
        Outcome::Int(n) => OwnedOutcome::Int(n),
        Outcome::Nil => OwnedOutcome::Nil,
        Outcome::Bool(b) => OwnedOutcome::Bool(b),
        Outcome::Err(code) => OwnedOutcome::Err(code),
    }
}
EOF
IFS= read -r -d '' RO_STAGES <<'EOF' || true
pub(super) fn stages_despite_error(id: CommandId) -> bool {
    matches!(id, CommandId::Mset | CommandId::Msetnx)
}

pub(super) fn stages(outcome: ReplyOutcome, id: CommandId) -> bool {
    outcome == ReplyOutcome::Success || stages_despite_error(id)
}
EOF
# The staged drain tagging its span by hand: the function after wire_reply.
IFS= read -r -d '' RO_LIE <<'EOF' || true
pub(super) fn staged_outcome<'a>(scratch: &'a [u8], reply: &StagedReply) -> Outcome<'a> {
    match *reply {
        StagedReply::Bytes(start, end, _) => Outcome::Bytes(&scratch[start..end]),
        StagedReply::Ok => Outcome::Ok,
    }
}
EOF

root=$(printf '%s\n' 'mod plane;' | ro_fixture roelsewhere)
printf '%s' "$RO_WIRE" | ro_put "$root" plane/dispatch.rs
expect_red_because "reply-outcome: a row's name in another file allows nothing" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

root=$(printf '%s\n' 'mod plane;' | ro_fixture ronext)
printf '%s\n' "$RO_WIRE" "$RO_LIE" "$RO_OWNED" | ro_put "$root" plane/fabric.rs
expect_red_because "reply-outcome: the function after wire_reply is outside its row" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

# A row's close with a trailing comment ends its range as rustfmt's `}` does.
root=$(printf '%s\n' 'mod plane;' | ro_fixture roclosenote)
{
    printf '%s' "$RO_WIRE" | sed 's|^}$|} // the owner side sets the tag here|'
    printf '\n%s\n%s' "$RO_LIE" "$RO_OWNED"
} | ro_put "$root" plane/fabric.rs
expect_red_because "reply-outcome: a commented close of wire_reply allows nothing after it" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
impl Fabric {
    pub(super) fn wire_reply(bytes: &[u8], outcome: ReplyOutcome) -> Outcome<'_> {
        match outcome {
            ReplyOutcome::Success => Outcome::Bytes(bytes),
            ReplyOutcome::Error => Outcome::ErrorReply(bytes),
        }
    }
}
EOF
root=$(printf '%s\n' 'mod plane;' | ro_fixture roindented)
printf '%s\n' "$body" "$RO_OWNED" | ro_put "$root" plane/fabric.rs
expect_red_because "reply-outcome: an indented wire_reply gives no allowance" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub(super) fn stages_despite_error(id: CommandId, outcome: ReplyOutcome) -> bool {
    outcome == ReplyOutcome::Error && matches!(id, CommandId::Mset | CommandId::Msetnx)
}

pub(super) fn stages(outcome: ReplyOutcome, id: CommandId) -> bool {
    outcome == ReplyOutcome::Success || stages_despite_error(id)
}
EOF
root=$(printf '%s\n' 'mod plane;' | ro_fixture roneighbour)
printf '%s' "$body" | ro_put "$root" plane/scatter.rs
expect_red_because "reply-outcome: stages_despite_error is not the row stages" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub(super) fn stages(outcome: ReplyOutcome, id: CommandId) -> bool {
    let _ = Outcome::Bytes(b"");
    outcome == ReplyOutcome::Success || stages_despite_error(id)
}
EOF
root=$(printf '%s\n' 'mod plane;' | ro_fixture rostagesr1)
printf '%s' "$body" | ro_put "$root" plane/scatter.rs
expect_red_because "reply-outcome: the row stages allows R2 only" "$RO_TAG" \
    env INF_CHECK_ROOT="$root" $REPLY

root=$(printf '%s\n' 'mod plane;' | ro_fixture rorowgone)
printf '%s\n' "$RO_WIRE" "$RO_STAGED" | ro_put "$root" plane/fabric.rs
expect_red_because "reply-outcome: a scanned row whose function is gone is a scope error" \
    "REPLY-OUTCOME SCOPE ERROR: crates/inf-server/src/plane/fabric.rs:owned_outcome is not found" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
pub(super) fn stages(outcome: ReplyOutcome, id: CommandId) -> bool {
    outcome.is_success() || stages_despite_error(id)
}
EOF
root=$(printf '%s\n' 'mod plane;' | ro_fixture rorowstale)
printf '%s' "$body" | ro_put "$root" plane/scatter.rs
expect_red_because "reply-outcome: a row whose range names none of its tokens is stale" \
    "REPLY-OUTCOME violation (a row with no reply tag): crates/inf-server/src/plane/scatter.rs:stages" \
    env INF_CHECK_ROOT="$root" $REPLY

# ro_rows_listed <root>: green, each row listed with the lines it allows and
# its reason, and none left unjudged.
ro_rows_listed() {
    local status=0 src=crates/inf-server/src/plane
    env INF_CHECK_ROOT="$1" $REPLY >"$work/ro-rows.log" 2>&1 || status=$?
    if [ "$status" -eq 0 ] &&
        grep -qF "  row $src/fabric.rs:wire_reply (lines 3, 4, 5): the owner side's one tag, set from the reply's outcome; the canary's arm" \
            "$work/ro-rows.log" &&
        grep -qF "  row $src/fabric.rs:owned_outcome (lines 24, 25): the origin's one decode: the variant read, the outcome set from it" \
            "$work/ro-rows.log" &&
        grep -qF "  row $src/scatter.rs:stages (line 6): the staging gate compares the reply's outcome" \
            "$work/ro-rows.log" &&
        grep -qF "3 rows judged allowing 6 lines; rows not judged, file not in the scan: none)" \
            "$work/ro-rows.log"; then
        return 0
    fi
    echo "exit $status"
    cat "$work/ro-rows.log"
    return 1
}
root=$(printf '%s\n' 'mod plane;' | ro_fixture rorows)
printf '%s\n' "$RO_WIRE" "$RO_STAGED" "$RO_OWNED" | ro_put "$root" plane/fabric.rs
printf '%s' "$RO_STAGES" | ro_put "$root" plane/scatter.rs
expect green "reply-outcome: the three rows as the tree holds them are green" \
    env INF_CHECK_ROOT="$root" $REPLY
expect green "reply-outcome: the OK line lists each row with its lines and reason" ro_rows_listed "$root"

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}

#[cfg(test)]
mod tests {
    fn t(b: &[u8]) -> inf_fabric::Outcome<'_> {
        let _ = wire_reply(b, inf_wire::ReplyOutcome::Error);
        inf_fabric::Outcome::Bytes(b)
    }
}

#[cfg(test)]
#[path = "fixture_tests.rs"]
mod fixture_tests;
EOF
root=$(printf '%s' "$body" | ro_fixture rotesttag)
printf '%s\n' "pub fn t(b: &[u8]) -> Outcome<'_> { wire_reply(b, ReplyOutcome::Error); Outcome::Bytes(b) }" |
    ro_put "$root" fixture_tests.rs
expect green "reply-outcome: a lying peer in a test module or a test-only modfile is out of scope" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
/// The tag ships as `Outcome::Bytes` from `wire_reply`, never by hand.
pub fn ok() {}
EOF
root=$(printf '%s' "$body" | ro_fixture rotagdoc)
expect green "reply-outcome: a doc comment naming a reply tag is not a site" \
    env INF_CHECK_ROOT="$root" $REPLY

IFS= read -r -d '' body <<'EOF' || true
use inf_fabric::{Op, Outcome};

type R<'a> = Option<Outcome<'a>>;

pub fn values(n: i64, b: bool, code: ErrCode) -> [Outcome<'static>; 5] {
    [Outcome::Int(n), Outcome::Ok, Outcome::Nil, Outcome::Bool(b), Outcome::Err(code)]
}

pub fn others(v: Vec<u8>, start: usize, end: usize, outcome: ReplyOutcome) -> bool {
    let _ = OwnedOutcome::Bytes(v);
    let _ = StagedReply::Bytes(start, end, outcome);
    let _ = NsApplyOutcome::Reply;
    let _ = <Vec<u8>>::new();
    let _ = Vec::<ReplyOutcome>::new();
    let _ = Option::<Outcome<'_>>::None;
    reply.is_error()
}

pub fn ship(op: &Op) -> Outcome<'_> {
    op.outcome()
}

impl From<u8> for OwnedOutcome {
    fn from(_: u8) -> Self {
        Self::Nil
    }
}

impl TryFrom<i64> for StagedReply {
    type Error = i64;
    fn try_from(v: i64) -> Result<Self, Self::Error> {
        Err(v)
    }
}

macro_rules! make {
    ($t:ty) => {
        $t::new()
    };
}
EOF
root=$(printf '%s' "$body" | ro_fixture rotagclean)
expect green "reply-outcome: non-reply variants, other enums' paths and the enum held, not renamed, are clean" \
    env INF_CHECK_ROOT="$root" $REPLY

root=$(ro_fixture rolibonly <<<"pub fn ok() {}")
expect_output "reply-outcome: the OK line names each row whose file is not in the scan" \
    "rows not judged, file not in the scan: crates/inf-server/src/plane/fabric.rs:wire_reply, crates/inf-server/src/plane/fabric.rs:owned_outcome, crates/inf-server/src/plane/scatter.rs:stages)" \
    env INF_CHECK_ROOT="$root" $REPLY
}

# -------------------------------------------------------------- node spawn
group_node_spawn() {
# ADR-0203 D3: scripts/check-node-spawn.sh on a fixture checkout of its own
# `git init`, its base committed and green as built: every rule's token in the
# module, rule 4's in the module's test, each sanctioned shape, an ignored
# file, an excluded file a test includes, and the ring census's files written
# from the gate's own table. A case writes its plant, runs the gate, and the
# checkout returns to its base. A plant is red for its own rule at its own
# file:line; a scope error names its cause.
NS="$SCRIPT_DIR/check-node-spawn.sh"
NSW="$work/ns"
mkdir -p "$NSW"
ns() { env INF_CHECK_ROOT="$1" "$NS"; }
NS_ROOT="$NSW/base"
# ns_put <path>: a file of the fixture at <path>, from stdin.
ns_put() {
    [ -n "$1" ] && [ -d "$NS_ROOT/.git" ] || { echo "ns_put: no path or no fixture" >&2; exit 2; }
    mkdir -p "$(dirname "$NS_ROOT/$1")"
    cat >"$NS_ROOT/$1"
}
# ns_reset: the checkout back at its base; an ignored file stays.
ns_reset() {
    fx_git "$NS_ROOT" checkout -q -- .
    fx_git "$NS_ROOT" clean -fdq
}
mkdir -p "$NS_ROOT"
git -C "$NS_ROOT" init -q
ns_put bins/infinityd/tests/support/node.rs <<'EOF'
//! The module: every rule's token is its own.
pub fn built() -> Option<&'static str> {
    option_env!("CARGO_BIN_EXE_infinityd")
}
pub fn from_infinityd_bin() -> Option<String> {
    std::env::var("INFINITYD_BIN").ok()
}
pub fn dir() -> Option<std::ffi::OsString> {
    std::env::var_os("XDG_RUNTIME_DIR")
}
pub const NAME: &str = "infinityd";
EOF
ns_put bins/infinityd/tests/node_lock.rs <<'EOF'
#[path = "support/node.rs"]
mod node;

fn private(command: &mut std::process::Command, dir: &std::path::Path) {
    command.env("XDG_RUNTIME_DIR", dir).env_clear();
}
EOF
ns_put bins/inf-bench/src/lib.rs <<'EOF'
pub const RELEASE: &str = "target/release/infinityd";
EOF
ns_put bins/inf-bench/src/shared.rs <<'EOF'
pub fn shared() {}
EOF
ns_put bins/inf-compare/src/main.rs <<'EOF'
fn main() {}
EOF
ns_put tests/compat/tests/required_binary.rs <<'EOF'
// CARGO_BIN_EXE_infinityd and "infinityd" in a comment name nothing.
/* XDG_RUNTIME_DIR, env_clear and "INFINITYD_BIN" in a block comment */
/// A doc line: NEXTEST_BIN_EXE_infinityd, UringDriver::new(config).
fn shapes(child: &mut std::process::Command, binary: &str) {
    child.env("INFINITYD_BIN", binary);
    child.env_remove("INFINITYD_BIN");
    child.env(
        "INFINITYD_BIN",
        binary,
    );
    eprintln!("SKIPPED: INFINITYD_BIN is unset; just compat sets it");
    let _ = std::process::Command::new("true").spawn().expect("spawn infinityd");
    let _ = env!("CARGO_TARGET_TMPDIR");
}
struct Fake;
impl Fake {
    fn register_pool(&mut self) {}
}
EOF
ns_put crates/fx/tests/uses_bench.rs <<'EOF'
#[path = "../../../bins/inf-bench/src/shared.rs"]
mod shared;
EOF
printf 'ignored/\n' | ns_put .gitignore
ns_put ignored/x.rs <<<'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
"$NS" --census >"$NSW/census"
while IFS="$(printf '\t')" read -r class path rings regs; do
    {
        printf '// A %s row of the ring census.\nfn census(config: u32) {\n' "$class"
        n=0
        while [ "$n" -lt "$rings" ]; do printf '    let _ = UringDriver::new(config);\n'; n=$((n + 1)); done
        n=0
        while [ "$n" -lt "$regs" ]; do printf '    driver.register_pool(&mut pool);\n'; n=$((n + 1)); done
        printf '}\n'
    } | ns_put "$path"
done <"$NSW/census"
fx_git "$NS_ROOT" add -A
fx_git "$NS_ROOT" -c user.name=t -c user.email=t@t commit -q -m base

ns_rows=$(wc -l <"$NSW/census" | tr -d ' ')
expect green "node-spawn: --census prints the table's rows" [ "$ns_rows" -gt 0 ]
expect green "node-spawn: the base, every sanctioned shape and the module's own tokens, is green" \
    ns "$NS_ROOT"
expect_output "node-spawn: the OK line counts the scan, the #[path] reach and the exclusions" \
    "node-spawn OK: $((ns_rows + 5)) files, .* ($((ns_rows + 4)) listed, 1 reached only by #.path., 1 of them under the excluded directories; 2 #.path. includes followed; 0 listed path(s) gone" \
    ns "$NS_ROOT"
expect_output "node-spawn: the OK line counts the census rows by class" \
    "ring census: $(grep -c '^test' "$NSW/census") test, $(grep -c '^bench' "$NSW/census") bench, $(grep -c '^node' "$NSW/census") node row(s)" \
    ns "$NS_ROOT"

# Each rule red at its plant's file:line, in a file git does not track yet.
P=crates/fx/tests/plant.rs
ns_case() {
    local label=$1 pattern=$2
    ns_put "$P"
    expect_red_because "node-spawn: $label" "$pattern" ns "$NS_ROOT"
    ns_reset
}
ns_case 'env!("CARGO_BIN_EXE_infinityd") is rule 1' "$P:1: rule 1," \
    <<<'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
ns_case 'std::env::var("NEXTEST_BIN_EXE_infinityd") is rule 1' "$P:2: rule 1," <<'EOF'
fn f() {
    let _ = std::env::var("NEXTEST_BIN_EXE_infinityd");
}
EOF
ns_case 'a const holding "INFINITYD_BIN" read by std::env::var is rule 2' "$P:1: rule 2," <<'EOF'
const VAR: &str = "INFINITYD_BIN";
fn f() {
    let _ = std::env::var(VAR);
}
EOF
ns_case 'a std::env::vars() filter on "INFINITYD_BIN" is rule 2' "$P:2: rule 2," <<'EOF'
fn f() {
    let _ = std::env::vars().filter(|(key, _)| key == "INFINITYD_BIN").count();
}
EOF
ns_case '"INFINITYD_BIN" as .env'\''s second argument is rule 2' "$P:2: rule 2," <<'EOF'
fn f(command: &mut std::process::Command) {
    command.env("PATH", "INFINITYD_BIN");
}
EOF
ns_case '"INFINITYD_BIN" as .env'\''s second argument, split across lines, is rule 2' \
    "$P:4: rule 2," <<'EOF'
fn f(command: &mut std::process::Command) {
    command.env(
        "PATH",
        "INFINITYD_BIN",
    );
}
EOF
ns_case 'Command::new("infinityd") is rule 3' "$P:2: rule 3," <<'EOF'
fn f() {
    let _ = std::process::Command::new("infinityd").spawn();
}
EOF
ns_case 'a CARGO_MANIFEST_DIR target path is rule 3' "$P:2: rule 3," <<'EOF'
fn f() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/infinityd")
}
EOF
ns_case 'a current_exe() path'\''s .join("infinityd") is rule 3' "$P:3: rule 3," <<'EOF'
fn f() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().join("infinityd")
}
EOF
for literal in 'r"infinityd"' 'r#"../../target/debug/infinityd"#' 'b"infinityd"' 'c"infinityd"' \
    'br"infinityd"' 'cr"../infinityd"' 'r##"../../target/debug/infinityd"##'; do
    ns_case "$literal is rule 3" "$P:1: rule 3," <<<"pub fn f() { let _ = $literal; }"
done
ns_case '.env_remove("XDG_RUNTIME_DIR") is rule 4' "$P:2: rule 4," <<'EOF'
fn f(command: &mut std::process::Command) {
    command.env_remove("XDG_RUNTIME_DIR");
}
EOF
ns_case '.env_clear() is rule 4' "$P:2: rule 4," <<'EOF'
fn f(command: &mut std::process::Command) {
    command.env_clear();
}
EOF
ns_case 'a bare marker is red' "$P:2: a bare node-spawn-allow marker" <<'EOF'
fn f() {
    // node-spawn-allow:
    let _ = std::process::Command::new("infinityd");
}
EOF
ns_case 'a marker with no colon is bare' "$P:2: a bare node-spawn-allow marker" <<'EOF'
fn f() {
    // node-spawn-allow a stand-in
    let _ = std::process::Command::new("infinityd");
}
EOF
ns_case 'a marker that marks no hit is red' "$P:2: node-spawn-allow marks no hit" <<'EOF'
fn f() {
    // node-spawn-allow: a stand-in, two lines above its hit

    let _ = std::process::Command::new("infinityd");
}
EOF
ns_case 'a marker two lines above leaves its hit red' "$P:4: rule 3," <<'EOF'
fn f() {
    // node-spawn-allow: a stand-in, two lines above its hit

    let _ = std::process::Command::new("infinityd");
}
EOF
ns_case 'a line comment holding /* then a token, no */ after, is read' "$P:2: rule 1," <<'EOF'
// the lock's files are /* never removed
pub const B: &str = env!("CARGO_BIN_EXE_infinityd");
EOF
ns_case 'a line comment holding /* then a token, a */ after, is read' "$P:2: rule 1," <<'EOF'
// the lock's files are /* never removed
pub const B: &str = env!("CARGO_BIN_EXE_infinityd");
// */
EOF
ns_case 'a block comment holding // then a token on its line is read' "$P:1: rule 1," \
    <<<'/* a // b */ pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
ns_case 'a // inside a string literal opens no comment' "$P:1: rule 1," \
    <<<'pub const B: (&str, &str) = ("http://x", env!("CARGO_BIN_EXE_infinityd"));'
ns_case 'a /* inside a string literal opens no comment' "$P:2: rule 1," <<'EOF'
pub const GLOB: &str = "src/*.rs";
pub const B: &str = env!("CARGO_BIN_EXE_infinityd");
EOF
ns_case 'rule 2 reads every literal on its line, not only the first' "$P:2: rule 2," <<'EOF'
fn f(command: &mut std::process::Command) {
    command.env("INFINITYD_BIN", std::env::var("INFINITYD_BIN").unwrap());
}
EOF
ns_case 'a call ending env( with no dot is no .env( exception' "$P:5: rule 2," <<'EOF'
fn get_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
fn f() {
    let _ = get_env("INFINITYD_BIN");
}
EOF
ns_case 'a marker on the line below its hit sanctions nothing' "$P:2: rule 3," <<'EOF'
fn f() {
    let _ = std::process::Command::new("infinityd");
    // node-spawn-allow: a marker on the line below its hit
}
EOF
ns_case 'a marker in a block comment with no reason is bare' "$P:2: a bare node-spawn-allow marker" \
    <<'EOF'
fn f() {
    /* node-spawn-allow: */
    let _ = std::process::Command::new("infinityd");
}
EOF
printf 'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");\n' | ns_put crates/fx/tests/support/node.rs
expect_red_because "node-spawn: another file named node.rs is not the module" \
    "crates/fx/tests/support/node.rs:1: rule 1," ns "$NS_ROOT"
ns_reset
printf 'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");\n' | ns_put bins/inf-bench/srcx/plant.rs
expect_red_because "node-spawn: a sibling of an excluded directory sharing its prefix is read" \
    "bins/inf-bench/srcx/plant.rs:1: rule 1," ns "$NS_ROOT"
ns_reset

# Sanctioned shapes, each green; each shape moved out of its exemption, red.
ns_put "$P" <<'EOF'
fn f() {
    // node-spawn-allow: a fixture stand-in named for the engine
    let _ = std::process::Command::new("infinityd");
    let _ = std::process::Command::new("./infinityd"); // node-spawn-allow: the same stand-in
}
EOF
expect green "node-spawn: a marker on its line or the line above sanctions its hit" ns "$NS_ROOT"
expect_output "node-spawn: a marked site is listed with its reason" \
    "allowed: $P:2: a fixture stand-in named for the engine" ns "$NS_ROOT"
ns_reset
ns_put bins/infinityd/tests/support/other.rs <"$NS_ROOT/bins/infinityd/tests/support/node.rs"
expect_red_because "node-spawn: the module's own tokens in another file are red" \
    "other.rs:3: rule 1," ns "$NS_ROOT"
expect_output "node-spawn: ... rule 2 there" "other.rs:6: rule 2," ns "$NS_ROOT"
expect_output "node-spawn: ... rule 3 there" "other.rs:11: rule 3," ns "$NS_ROOT"
expect_output "node-spawn: ... rule 4 there" "other.rs:9: rule 4," ns "$NS_ROOT"
ns_reset
sed -n '4,6p' "$NS_ROOT/bins/infinityd/tests/node_lock.rs" | ns_put bins/infinityd/tests/lock_copy.rs
expect_red_because "node-spawn: the module test's rule-4 tokens in another test are red" \
    "lock_copy.rs:2: rule 4," ns "$NS_ROOT"
ns_reset
ns_put "$P" <<<'pub fn f() -> Option<String> { std::env::var("CARGO_BIN_EXE_infinityd_probe").ok() }'
expect green "node-spawn: rule 1's token is bounded on its right (another binary's variable)" \
    ns "$NS_ROOT"
ns_reset
ns_put "$P" <<'EOF'
fn f(command: &mut std::process::Command, binary: &str) {
    command.env(
        // the variable compat reads
        "INFINITYD_BIN",
        binary,
    );
}
EOF
expect green "node-spawn: rule 2's split call reads the previous code line, past a comment" \
    ns "$NS_ROOT"
ns_reset
ns_put "$P" <<'EOF'
pub fn f() -> Option<String> {
    std::env::var("OLD_XDG_RUNTIME_DIR").ok()
}
pub fn env_clearance() {}
EOF
expect green "node-spawn: rule 4's tokens are bounded on both sides" ns "$NS_ROOT"
ns_reset

# The red's text: the module, and the call each package writes instead.
ns_put bins/infinityd/tests/plant.rs <<<'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
expect_output "node-spawn: a red in bins/infinityd names the module and its call" \
    "only through bins/infinityd/tests/support/node.rs: write NodeBinary::built().command()...spawn()" \
    ns "$NS_ROOT"
ns_reset
ns_put tests/compat/tests/plant.rs <<<'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
expect_output "node-spawn: a red in tests/compat names the harness" \
    "start nodes through compat::harness" ns "$NS_ROOT"
ns_reset
ns_put "$P" <<<'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
expect_output "node-spawn: a red elsewhere names from_infinityd_bin and built_or" \
    "write NodeBinary::from_infinityd_bin(), or NodeBinary::built_or(<var>)" ns "$NS_ROOT"
ns_reset

# Scope: tracked and untracked files are read, a gone one is counted, and a
# #[path] target is read even under an excluded directory.
printf 'fn more() {\n    let _ = env!("CARGO_BIN_EXE_infinityd");\n}\n' \
    >>"$NS_ROOT/tests/compat/tests/required_binary.rs"
expect_red_because "node-spawn: a token added to a tracked file is red" \
    "tests/compat/tests/required_binary.rs:20: rule 1," ns "$NS_ROOT"
ns_reset
rm -f "$NS_ROOT/crates/fx/tests/uses_bench.rs"
expect_output "node-spawn: a listed path gone from the work tree is skipped and counted" \
    "1 listed path(s) gone from the work tree" ns "$NS_ROOT"
ns_reset
ns_put bins/inf-bench/src/planted.rs <<<'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
expect green "node-spawn: an excluded file no test includes is not read" ns "$NS_ROOT"
printf '#[path = "../../../bins/inf-bench/src/planted.rs"]\nmod planted;\n' | ns_put "$P"
expect_red_because "node-spawn: a #[path] target under an excluded directory is read" \
    "bins/inf-bench/src/planted.rs:1 (included by $P:1 through #.path.): rule 1," ns "$NS_ROOT"
ns_reset
printf '#[path = "../../../bins/inf-bench/src/first.rs"]\nmod first;\n' | ns_put "$P"
printf '#[path = "second.rs"]\nmod second;\n' | ns_put bins/inf-bench/src/first.rs
printf 'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");\n' | ns_put bins/inf-bench/src/second.rs
expect_red_because "node-spawn: a #[path] target of a #[path] target is read" \
    "bins/inf-bench/src/second.rs:1 (included by bins/inf-bench/src/first.rs:1 through #.path.): rule 1," \
    ns "$NS_ROOT"
ns_reset

# The ring census.
ns_put "$P" <<<'fn f() { let _ = inf_runtime::UringDriver::new(config); }'
expect_red_because "node-spawn: a new file naming UringDriver::new is red, naming its row" \
    "ring census: $P names UringDriver::new 1 time(s), outside the table; add the row $P|" \
    ns "$NS_ROOT"
ns_reset
printf 'fn more() {\n    driver.register_tier_pool(&mut pool);\n}\n' \
    >>"$NS_ROOT/tests/crash-matrix/tests/fsyncgate.rs"
expect_red_because "node-spawn: a census row whose count moved is red" \
    "ring census: tests/crash-matrix/tests/fsyncgate.rs names UringDriver::new 1 time(s) and 1 registration(s), its row 1 and 0" \
    ns "$NS_ROOT"
ns_reset
printf 'fn more(config: u32) {\n    let _ = UringDriver::new(config);\n}\n' \
    >>"$NS_ROOT/tests/crash-matrix/tests/fsyncgate.rs"
expect_red_because "node-spawn: a census row whose ring count moved is red" \
    "ring census: tests/crash-matrix/tests/fsyncgate.rs names UringDriver::new 2 time(s) and 0 registration(s), its row 1 and 0" \
    ns "$NS_ROOT"
ns_reset
printf 'fn census() {}\n' >"$NS_ROOT/tests/crash-matrix/tests/fsyncgate.rs"
expect_red_because "node-spawn: a census row whose file names neither token is red" \
    "ring census: tests/crash-matrix/tests/fsyncgate.rs names neither" ns "$NS_ROOT"
ns_reset
rm -f "$NS_ROOT/tests/crash-matrix/tests/fsyncgate.rs"
expect_red_because "node-spawn: a census row whose file is gone is red" \
    "ring census: the row tests/crash-matrix/tests/fsyncgate.rs names no scanned file" ns "$NS_ROOT"
ns_reset
ns_put "$P" <<<'fn f() { let _ = FakeUringDriver::new(config); }'
expect green "node-spawn: the census's constructor is bounded on its left (another type's new)" \
    ns "$NS_ROOT"
ns_reset
ns_put "$P" <<<'fn f() { let _ = UringDriver::new_probe(config); }'
expect green "node-spawn: the census's constructor is bounded on its right (another function)" \
    ns "$NS_ROOT"
ns_reset

# Rule 5 (ADR-0203 D3 as A2 amends it): a plant appended to the module, red at
# its own line of the module (the plant's first line is 1) or green.
NSM=bins/infinityd/tests/support/node.rs
nsm_base=$(wc -l <"$NS_ROOT/$NSM" | tr -d ' ')
nf_case() {
    local label=$1 line=$2 why=$3
    cat >>"$NS_ROOT/$NSM"
    expect_red_because "node-spawn: $label" "$NSM:$((nsm_base + line)): $why" ns "$NS_ROOT"
    ns_reset
}
nf_green() {
    cat >>"$NS_ROOT/$NSM"
    expect green "node-spawn: $1" ns "$NS_ROOT"
    ns_reset
}
for vis in 'pub' 'pub(crate)' 'pub(super)' 'pub(in crate::harness)'; do
    printf 'pub struct Named {\n    field: u8,\n    %s planted: u8,\n}\n' "$vis" |
        nf_case "rule 5: $vis on a named field" 3 "rule 5, a field of Named carries a visibility"
    printf 'pub struct Positional(u8, %s std::path::PathBuf);\n' "$vis" |
        nf_case "rule 5: $vis on a positional field" 1 "rule 5, a field of Positional carries"
done
nf_case 'rule 5: a bare node-field-public marker is red' 1 "a bare node-field-public marker" <<'EOF'
// node-field-public:
pub struct Bare {
    pub field: u8,
}
EOF
nf_case 'rule 5: a marker with no reason is bare' 1 "a bare node-field-public marker" <<'EOF'
// node-field-public: 1 fields
pub struct Bare {
    pub field: u8,
}
EOF
nf_case 'rule 5: a marker with no count is bare' 1 "a bare node-field-public marker" <<'EOF'
// node-field-public: some fields, a count in words
pub struct Bare {
    pub field: u8,
}
EOF
nf_case 'rule 5: a marker on a struct with no public field is red' 1 \
    "node-field-public marks Closed, which has no field with a visibility" <<'EOF'
// node-field-public: 1 fields, nothing public
pub struct Closed {
    field: u8,
}
EOF
nf_case 'rule 5: a marker two lines above its struct marks no struct' 1 \
    "node-field-public marks no struct on its line or the next" <<'EOF'
// node-field-public: 1 fields, two lines above

pub struct Far {
    pub field: u8,
}
EOF
nf_case 'rule 5: a marker on the line below its struct marks no struct' 2 \
    "node-field-public marks no struct on its line or the next" <<'EOF'
pub struct Below {
    // node-field-public: 1 fields, inside the body
    pub field: u8,
}
EOF
cat >>"$NS_ROOT/$NSM" <<'EOF'
// node-field-public: 1 fields, one of two
pub struct Counted {
    pub first: u8,
    pub second: u8,
}
EOF
expect_red_because "node-spawn: rule 5: a marker whose count is not its struct's is red" \
    "$NSM:$((nsm_base + 1)): node-field-public counts 1 field(s) with a visibility and Counted has 2" \
    ns "$NS_ROOT"
expect_output "node-spawn: rule 5: ... and leaves each of the struct's fields red" \
    "$NSM:$((nsm_base + 4)): rule 5, a field of Counted" ns "$NS_ROOT"
ns_reset
nf_case 'rule 5: a second marker on one struct is red' 2 \
    "a second node-field-public marker on Twice" <<'EOF'
// node-field-public: 1 fields, the line above
pub struct Twice { // node-field-public: 1 fields, its own line
    pub field: u8,
}
EOF
nf_case 'rule 5: a bound whose return holds parens opens no body' 2 "rule 5, a field of Generic" <<'EOF'
pub struct Generic<F: Fn(u32) -> (u32, u32)> {
    pub planted: F,
}
EOF
cat >>"$NS_ROOT/$NSM" <<'EOF'
pub struct Bounded<F>
where
    F: Fn(u32) -> u32,
{
    pub planted: F,
}
pub struct After(pub u8);
EOF
expect_red_because "node-spawn: rule 5: a where clause's parens open no body" \
    "$NSM:$((nsm_base + 5)): rule 5, a field of Bounded" ns "$NS_ROOT"
expect_output "node-spawn: rule 5: ... and the next struct's body opens at its own paren" \
    "$NSM:$((nsm_base + 7)): rule 5, a field of After" ns "$NS_ROOT"
ns_reset
nf_case 'rule 5: braces nested in a named body do not close it' 3 "rule 5, a field of Nested" <<'EOF'
pub struct Nested {
    buffer: [u8; { 4 }],
    pub planted: u8,
}
EOF
nf_case 'rule 5: parens nested in a positional body do not close it' 1 "rule 5, a field of Pair" \
    <<<'pub struct Pair((u8, u8), pub u8);'
nf_case 'rule 5: a brace inside a string literal closes no body' 3 "rule 5, a field of Quoted" <<'EOF'
pub struct Quoted {
    #[doc = "a } in a string"]
    pub planted: u8,
}
EOF
printf 'pub struct Open {\n    field: u8,\n' >>"$NS_ROOT/$NSM"
expect_red_because "node-spawn: rule 5: a struct whose body does not close is a scope error" \
    "SCOPE ERROR: $NSM: rule 5 cannot bound the body of Open (its line $((nsm_base + 1)))" \
    ns "$NS_ROOT"
ns_reset
: >"$NS_ROOT/$NSM"
expect_red_because "node-spawn: rule 5: an empty module is a scope error" \
    "SCOPE ERROR: rule 5 read no line of the module" ns "$NS_ROOT"
ns_reset
cat >>"$NS_ROOT/$NSM" <<'EOF'
// node-field-public: 2 fields, a test sets both
pub struct Marked {
    pub first: u8,
    pub(crate) second: u8,
}
/* node-field-public: 1 fields, in a block comment */
pub struct Blocked(pub u8);
EOF
expect green "node-spawn: rule 5: a marked struct with its counted fields is green" ns "$NS_ROOT"
expect_output "node-spawn: rule 5: the OK line lists each marker with its struct and count" \
    "public by design: $NSM:$((nsm_base + 1)): Marked, 2 field(s): a test sets both" ns "$NS_ROOT"
expect_output "node-spawn: rule 5: ... a block comment's marker without its close" \
    "public by design: $NSM:$((nsm_base + 6)): Blocked, 1 field(s): in a block comment$" ns "$NS_ROOT"
expect_output "node-spawn: rule 5: the OK line counts the structs and their public fields" \
    "rule 5: 2 struct(s) in the module, 3 field(s) with a visibility, all in 2 marked struct(s)" \
    ns "$NS_ROOT"
ns_reset
nf_green "rule 5: a marker on its struct's own line marks it" <<'EOF'
pub struct OnItsLine { // node-field-public: 1 fields, its own line
    pub only: u8,
}
EOF
nf_green 'rule 5: a pub fn in an impl, after a unit struct, is no field' <<'EOF'
pub struct Unit;
impl Unit {
    pub fn method(&self) {}
}
EOF
nf_green 'rule 5: pub inside a longer name or a raw identifier is no visibility' <<'EOF'
pub struct Private {
    field: u8,
    publish: u8,
    epub: u8,
    r#pub: u8,
}
pub struct Positional(u8, std::path::PathBuf);
EOF
nf_green 'rule 5: struct inside a longer name, a raw identifier or a string opens no body' <<'EOF'
mod destruct {
    pub fn f() {}
}
mod structure {
    pub fn f() {}
}
mod r#struct {
    pub fn f() {}
}
pub const TEXT: &str = "struct S { pub x: u8 }";
EOF
printf 'pub struct Other {\n    pub field: u8,\n}\n' | ns_put crates/fx/tests/support/node.rs
expect green "node-spawn: rule 5 reads the module alone: a pub field in another node.rs" ns "$NS_ROOT"
ns_reset
# Rule 5's parser: a header's const block and brackets, char literals, arrows,
# and an item met before a body opens.
nf_case 'rule 5: a const block in a generic default is read whole' 2 "rule 5, a field of Shift" <<'EOF'
pub struct Shift<const N: usize = { 1 << 2 }> {
    pub planted: [u8; N],
}
EOF
nf_case "rule 5: a const block's > closes no generics" 2 "rule 5, a field of Compare" <<'EOF'
pub struct Compare<const N: usize = { (3 > 2) as usize }> {
    pub planted: [u8; N],
}
EOF
nf_case "rule 5: a where clause's brackets hold their ; and const block" 5 \
    "rule 5, a field of WhereBlock" <<'EOF'
pub struct WhereBlock
where
    [u8; { 1 << 2 }]: Sized,
{
    pub planted: u8,
}
EOF
nf_case "rule 5: a '}' char literal closes no body" 3 "rule 5, a field of CharBrace" <<'EOF'
pub struct CharBrace {
    width: [u8; { '}' as usize }],
    pub planted: u8,
}
EOF
nf_case "rule 5: a '\"' char literal opens no string" 1 "rule 5, a field of OddQuote" <<'EOF'
pub struct OddQuote(Wrap<{ b'"' }>, pub u8, Wrap<{ "x".len() as u8 }>);
EOF
cat >>"$NS_ROOT/$NSM" <<'EOF'
macro_rules! make {
    (struct $name:ident) => {
        pub struct $name {
            pub planted: u8,
        }
    };
}
EOF
expect_red_because "node-spawn: rule 5: a matcher's => closes no generics" \
    "$NSM:$((nsm_base + 3)): rule 5, a field of a struct" ns "$NS_ROOT"
expect_output "node-spawn: rule 5: ... and the arm's struct is read" \
    "$NSM:$((nsm_base + 4)): rule 5, a field of a struct" ns "$NS_ROOT"
ns_reset
for item in 'pub struct Next {' 'impl Next {' 'fn helper() {'; do
    printf 'pub struct Truncated<T>\n%s\n    pub planted: u8,\n}\n' "$item" >>"$NS_ROOT/$NSM"
    expect_red_because "node-spawn: rule 5: '$item' before a header's body opens is a scope error" \
        "SCOPE ERROR: $NSM: rule 5 cannot bound the header of Truncated (its line $((nsm_base + 1))): an item opens at line $((nsm_base + 2))" \
        ns "$NS_ROOT"
    ns_reset
done
nf_green 'rule 5: a fn pointer type in a header is no item' \
    <<<'pub struct Callback<F = fn(u8) -> u8>(F);'
nf_green 'rule 5: a lifetime is no char literal' <<'EOF'
pub struct Borrowed<'a>(&'a u8);
pub struct After(u8);
EOF
printf 'pub struct Unfinished<T>\n' >>"$NS_ROOT/$NSM"
expect_red_because "node-spawn: rule 5: a header still open at the file's end is a scope error" \
    "SCOPE ERROR: $NSM: rule 5 cannot bound the body of Unfinished (its line $((nsm_base + 1)))" \
    ns "$NS_ROOT"
ns_reset
nf_case 'rule 5: a marker that counts more than its struct has is red' 1 \
    "node-field-public counts 3 field(s) with a visibility and Over has 2" <<'EOF'
// node-field-public: 3 fields, one more than there is
pub struct Over {
    pub first: u8,
    pub second: u8,
}
EOF
nf_case 'rule 5: a marker in the singular is bare' 1 "a bare node-field-public marker" <<'EOF'
// node-field-public: 1 field, the singular
pub struct One {
    pub only: u8,
}
EOF
nf_case "rule 5: a where inside a name rule 5 cannot read opens no where clause" 3 \
    "rule 5, a field of a struct" <<'EOF'
macro_rules! named {
    ($nowhere:ident) => {
        pub struct $nowhere(pub u8);
    };
}
EOF
cat >>"$NS_ROOT/$NSM" <<'EOF'
// node-field-public: 2 fields, both on one line
pub struct Two(pub u8, pub u8);
EOF
expect green "node-spawn: rule 5: two pub on one line are two fields" ns "$NS_ROOT"
expect_output "node-spawn: rule 5: ... listed with their count" \
    "public by design: $NSM:$((nsm_base + 1)): Two, 2 field(s): both on one line" ns "$NS_ROOT"
ns_reset
nf_green 'rule 5: two structs on one line: the marker marks the last' <<'EOF'
// node-field-public: 1 fields, the last struct of its line
pub struct Empty; pub struct Last(pub u8);
EOF
nf_green "rule 5: a marker on its struct's line marks it before the next line's" <<'EOF'
pub struct First(pub u8); // node-field-public: 1 fields, its own line
pub struct Second(pub u8); // node-field-public: 1 fields, its own line too
EOF

# Scope errors.
printf 'fn f() {}\n/* never closed\nfn g() {}\n' | ns_put "$P"
expect_red_because "node-spawn: a file that ends inside a block comment is a scope error" \
    "SCOPE ERROR: $P ends inside the block comment opened at its line 2" ns "$NS_ROOT"
ns_reset
printf '#[path = "missing.rs"]\nmod missing;\n' | ns_put "$P"
expect_red_because "node-spawn: a #[path] target that does not exist is a scope error" \
    "SCOPE ERROR: $P:1: #.path = \"missing.rs\". names crates/fx/tests/missing.rs, which does not exist" \
    ns "$NS_ROOT"
ns_reset
printf '#[path = "../../../../outside.rs"]\nmod outside;\n' | ns_put "$P"
expect_red_because "node-spawn: a #[path] target outside the root is a scope error" \
    "resolves outside" ns "$NS_ROOT"
ns_reset
rm -f "$NS_ROOT/bins/infinityd/tests/support/node.rs"
expect_red_because "node-spawn: a missing module is a scope error" \
    "SCOPE ERROR: the module bins/infinityd/tests/support/node.rs is missing" ns "$NS_ROOT"
ns_reset
rm -f "$NS_ROOT/bins/infinityd/tests/node_lock.rs"
expect_red_because "node-spawn: a missing module test is a scope error" \
    "SCOPE ERROR: the module's test bins/infinityd/tests/node_lock.rs" ns "$NS_ROOT"
ns_reset
for dir in bins/inf-bench/src bins/inf-compare/src; do
    rm -rf "${NS_ROOT:?}/$dir"
    expect_red_because "node-spawn: a missing excluded directory $dir is a scope error" \
        "SCOPE ERROR: the excluded directory $dir is missing" ns "$NS_ROOT"
    ns_reset
done
printf 'x' | ns_put "$(printf 'crates/fx/tests/a\tb.rs')"
expect_red_because "node-spawn: a path holding a tab is a scope error" \
    "SCOPE ERROR: a listed path holds a tab or a newline" ns "$NS_ROOT"
ns_reset
printf 'x' | ns_put "$(printf 'crates/fx/tests/a\nb.rs')"
expect_red_because "node-spawn: a path holding a newline is a scope error" \
    "SCOPE ERROR: a listed path holds a tab or a newline" ns "$NS_ROOT"
ns_reset
ns_put 'a=b.rs' <<<'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");'
printf 'pub const B: &str = env!("CARGO_BIN_EXE_infinityd");\n' | ns_put 'x=1/plant.rs'
expect_red_because "node-spawn: a path awk would take for an assignment is read (a=b.rs)" \
    "a=b.rs:1: rule 1," ns "$NS_ROOT"
expect_output "node-spawn: ... and a file under a directory named x=1" "x=1/plant.rs:1: rule 1," \
    ns "$NS_ROOT"
ns_reset
expect_red_because "node-spawn: a root that is not its own git work tree is a scope error" \
    "SCOPE ERROR: .* is not its own git work tree" ns "$NS_ROOT/crates"
mkdir -p "$NSW/nogit/bins/infinityd/tests/support"
cp "$NS_ROOT/bins/infinityd/tests/support/node.rs" "$NSW/nogit/bins/infinityd/tests/support/"
expect_red_because "node-spawn: a root outside a git work tree is a scope error" \
    "SCOPE ERROR: .* is not inside a git work tree" ns "$NSW/nogit"
NS_ROOT="$NSW/empty"
mkdir -p "$NS_ROOT"
git -C "$NS_ROOT" init -q
cp -R "$NSW/base/bins" "$NS_ROOT/"
printf '*.rs\n' | ns_put .gitignore
expect_red_because "node-spawn: an empty scan is a scope error" \
    "SCOPE ERROR: no \*.rs file to scan" ns "$NS_ROOT"
NS_ROOT="$NSW/blank"
mkdir -p "$NS_ROOT"
git -C "$NS_ROOT" init -q
: | ns_put bins/infinityd/tests/support/node.rs
: | ns_put bins/infinityd/tests/node_lock.rs
mkdir -p "$NS_ROOT/bins/inf-bench/src" "$NS_ROOT/bins/inf-compare/src"
expect_red_because "node-spawn: every listed file empty, the blanker reading no line, is a scope error" \
    "SCOPE ERROR: the blanker read no line" ns "$NS_ROOT"
NS_ROOT="$NSW/base"
}

# ------------------------------------------------------------ refusal text
group_refusal_text() {
# ADR-0172 D2: no production line of crates/inf-server/src spells a refusal
# class suffix; inf_wire::Refusal renders every one. One plant a suffix, each
# fixture one inf-server lib.rs (and a test-only modfile where the case needs one).
REFUSAL=./scripts/check-refusal-text.sh
rt_fixture() {
    local name=$1 root
    [ -n "$name" ] && [ -n "$work" ] || { echo "rt_fixture: empty name or work dir" >&2; exit 2; }
    root="$work/$name"
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/inf-server/src"
    cat >"$root/crates/inf-server/src/lib.rs"
    echo "$root"
}

IFS= read -r -d '' body <<'EOF' || true
pub const BUSY: &str = "BUSY a script is running";
pub const OOM: &str = "OOM command not allowed when used memory > 'maxmemory'.";
pub fn reply(refusal: inf_wire::Refusal) -> &'static str { refusal.render() }
/// A doc line naming the `limit reached` suffix is not a site.
pub fn ok() {}
EOF
root=$(printf '%s' "$body" | rt_fixture rtclean)
expect green "refusal-text: prefixes, Redis's OOM reply, a render call and a doc line are clean" \
    env INF_CHECK_ROOT="$root" $REFUSAL
expect_output "refusal-text: the scope line discloses the scan" "1 files, 5 lines scanned" \
    env INF_CHECK_ROOT="$root" $REFUSAL

for suffix in "capacity in use" "limit reached" "reservation refused" "identity space exhausted" \
    "Identity Space Exhausted"; do
    root=$(rt_fixture rtplanted <<<"pub const E: &str = \"ERR planted $suffix\";")
    expect_red_because "refusal-text: planted '$suffix'" "REFUSAL-TEXT violation" \
        env INF_CHECK_ROOT="$root" $REFUSAL
done

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() {}

#[cfg(test)]
mod tests {
    const E: &str = "ERR namespace identity space exhausted";
}

#[cfg(test)]
#[path = "fixture_tests.rs"]
mod fixture_tests;
EOF
root=$(printf '%s' "$body" | rt_fixture rttest)
printf '%s\n' 'pub const E: &str = "ERR tiered namespace limit reached";' \
    >"$root/crates/inf-server/src/fixture_tests.rs"
expect green "refusal-text: a test module and a test-only modfile are out of scope" \
    env INF_CHECK_ROOT="$root" $REFUSAL
expect_output "refusal-text: the excluded modfile is disclosed" "1 test-only files excluded" \
    env INF_CHECK_ROOT="$root" $REFUSAL

mkdir -p "$work/rtempty/crates/inf-server/src"
expect_red_because "refusal-text: an empty file set is a scope error" "REFUSAL-TEXT SCOPE ERROR" \
    env INF_CHECK_ROOT="$work/rtempty" $REFUSAL
fx_build_empty
expect red "refusal-text: a missing directory is a scope error" env INF_CHECK_ROOT="$work/empty" $REFUSAL
}

# ---------------------------------------------------------------- run-sweep
group_run_sweep() {
# A stub simulator driven by env: which shard exits non-zero, which shard
# reports violations, which shard writes no manifest.
stub="$work/inf-sim-stub"
cat >"$stub" <<'EOF'
#!/usr/bin/env bash
# args: --scenario S --sweep N --seed B --shard I/K --out DIR
shard=""; out=""
while [ $# -gt 0 ]; do
    case "$1" in
        --shard) shard=${2%%/*}; shift 2 ;;
        --out) out=$2; shift 2 ;;
        *) shift ;;
    esac
done
mkdir -p "$out"
[ "$shard" = "${STUB_NO_MANIFEST:-none}" ] && exit 0
v=0
[ "$shard" = "${STUB_VIOLATING:-none}" ] && v=1
printf 'scenario=stub base_seed=0x1 sweep=8 shard=%s/4 seeds_run=2 violations=%s refused=0\n' "$shard" "$v" >"$out/manifest-shard-$shard.txt"
printf '0x1 %s\n' "$([ "$v" -eq 1 ] && echo 'VIOLATION planted' || echo ok)" >"$out/results-shard-$shard.txt"
[ "$shard" = "${STUB_EXIT_ONE:-none}" ] && exit 1
exit "$v"
EOF
chmod +x "$stub"
SWEEP=./scripts/run-sweep.sh

expect green "run-sweep: all shards clean" env INF_SIM_BIN="$stub" INF_SWEEP_SHARDS=4 $SWEEP stub 8 0x1
expect red "run-sweep: one shard exits non-zero (the bare-wait gap)" env INF_SIM_BIN="$stub" INF_SWEEP_SHARDS=4 STUB_EXIT_ONE=2 $SWEEP stub 8 0x1
expect red "run-sweep: one shard reports violations" env INF_SIM_BIN="$stub" INF_SWEEP_SHARDS=4 STUB_VIOLATING=3 $SWEEP stub 8 0x1
expect red "run-sweep: one shard writes no manifest" env INF_SIM_BIN="$stub" INF_SWEEP_SHARDS=4 STUB_NO_MANIFEST=0 $SWEEP stub 8 0x1
}

# ------------------------------------------------------ shipping features
group_shipping_features() {
# ADR-0107 (F-L16-01): the manifest scan runs on fixture roots (the
# resolver half needs a real workspace and is skipped under INF_CHECK_ROOT).
SHIP=./scripts/check-shipping-features.sh

# manifest <name> <toml…>: a fresh root with one crate manifest from stdin.
manifest() {
    local name=$1 root
    [ -n "$name" ] && [ -n "$work" ] || { echo "manifest: empty name or work dir" >&2; exit 2; }
    root="$work/$name"
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/fake"
    cat >"$root/crates/fake/Cargo.toml"
    echo "$root"
}

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[dependencies]
inf-foundation = { workspace = true }

[dev-dependencies]
inf-foundation = { workspace = true, features = ["fault-points", "collision-oracle"] }
EOF
root=$(printf '%s' "$body" | manifest ship-clean)
expect green "shipping: dev-dependency edge may request the features" env INF_CHECK_ROOT="$root" $SHIP
expect_output "shipping: scope line discloses the scan" "1 manifests scanned" env INF_CHECK_ROOT="$root" $SHIP

mkdir -p "$work/ship-empty/crates"
expect red "shipping: no manifests at all is a failure, not OK" env INF_CHECK_ROOT="$work/ship-empty" $SHIP

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[dependencies]
inf-foundation = { workspace = true, features = ["collision-oracle", "fault-points"] }
EOF
root=$(printf '%s' "$body" | manifest ship-normal)
expect red "shipping: the F-L16-01 shape — a normal edge requests the features" env INF_CHECK_ROOT="$root" $SHIP

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[dependencies.inf-foundation]
workspace = true
features = ["fault-points"]
EOF
root=$(printf '%s' "$body" | manifest ship-table)
expect red "shipping: a [dependencies.NAME] table requesting the feature" env INF_CHECK_ROOT="$root" $SHIP

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[target.'cfg(unix)'.dependencies]
inf-foundation = { workspace = true, features = ["fault-points"] }
EOF
root=$(printf '%s' "$body" | manifest ship-target)
expect red "shipping: a target-cfg dependency edge is a normal edge" env INF_CHECK_ROOT="$root" $SHIP

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[features]
default = ["sim"]
sim = ["dst"]
dst = ["inf-foundation/fault-points"]

[dependencies]
inf-foundation = { workspace = true }
EOF
root=$(printf '%s' "$body" | manifest ship-default)
expect red "shipping: default reaching a forwarder (transitively)" env INF_CHECK_ROOT="$root" $SHIP

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[features]
dst = [
    "inf-foundation/collision-oracle",
    "inf-foundation/fault-points",
]

[dependencies]
inf-foundation = { workspace = true }
EOF
root=$(printf '%s' "$body" | manifest ship-forwarder)
expect green "shipping: a non-default forwarder feature (inf-sim's dst shape)" env INF_CHECK_ROOT="$root" $SHIP
expect_output "shipping: forwarders are counted" "1 forwarder feature(s)" env INF_CHECK_ROOT="$root" $SHIP

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[dependencies]
inf-store = { workspace = true, features = ["test-support"] }
EOF
root=$(printf '%s' "$body" | manifest ship-test-support)
expect red "shipping: a normal edge requests inf-store's test-support (ADR-0139 D2)" env INF_CHECK_ROOT="$root" $SHIP

IFS= read -r -d '' body <<'EOF' || true
[package]
name = "fake"

[features]
test-support = []

[dev-dependencies]
inf-store = { workspace = true, features = ["test-support"] }
EOF
root=$(printf '%s' "$body" | manifest ship-test-support-dev)
expect green "shipping: test-support declared, and requested on a dev edge only" env INF_CHECK_ROOT="$root" $SHIP
}

# ------------------------------------------------ planted-bug canary driver
group_sim_canaries() {
# ADR-0139: `sim-canaries.sh` judges a crate-test row by the named test's
# own verdict line. A stub cargo prints a canned test log: with RUSTFLAGS
# set it is the planted build, without it the plain one.
CANARY=./scripts/sim-canaries.sh
canary_fixture() {
    local name=$1 planted=$2 plain=$3 dir
    [ -n "$name" ] && [ -n "$work" ] || { echo "canary_fixture: empty name or work dir" >&2; exit 2; }
    dir="$work/$name"
    mkdir -p "$dir"
    echo "inf_canary_fixture crate-test fake test:suite the_row" >"$dir/rows"
    {
        echo '#!/usr/bin/env bash'
        echo 'if [ -n "${RUSTFLAGS:-}" ]; then'
        echo "$planted"
        echo 'else'
        echo "$plain"
        echo 'fi'
    } >"$dir/cargo"
    chmod +x "$dir/cargo"
    echo "$dir"
}
ok_line='echo "test the_row ... ok"; exit 0'
failed_line='echo "test the_row ... FAILED"; exit 101'
dir=$(canary_fixture canary-caught "$failed_line" "$ok_line")
expect green "canaries: the named test FAILED on the planted build, ok on the plain one" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(canary_fixture canary-toothless "$ok_line" "$ok_line")
expect red "canaries: a planted build that stays green is NOT CAUGHT" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: the toothless oracle is named" "NOT CAUGHT" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(canary_fixture canary-other-red 'echo "error[E0425]: cannot find value"; echo "test another_row ... FAILED"; exit 101' "$ok_line")
expect red "canaries: red for another reason (compile error, another test) is not a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: the wrong-reason red is named" "red for another reason" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(canary_fixture canary-no-control "$failed_line" "$failed_line")
expect red "canaries: a plain build that is red too is no control leg" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(canary_fixture canary-filtered-out "$failed_line" 'echo "running 0 tests"; exit 0')
expect red "canaries: a plain run that never ran the named test is not green" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
: >"$work/canary-empty-rows"
expect red "canaries: an empty row table is a scope failure" env INF_CANARY_ROWS_FILE="$work/canary-empty-rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
echo "inf_canary_fixture crate-test fake test:suite" >"$work/canary-short-row"
expect red "canaries: a malformed crate-test row is a scope failure" env INF_CANARY_ROWS_FILE="$work/canary-short-row" INF_CANARY_CARGO="$dir/cargo" $CANARY
# The nightly matrix shards the lane by cfg (INF_CANARY_SHARD=I/K): a
# cfg's rows stay in one shard, and the shards together run every row.
dir=$(canary_fixture canary-shard "$failed_line" "$ok_line")
printf 'inf_canary_%s crate-test fake test:suite the_row\n' a b a c >"$dir/rows"
expect_output "canaries: shard 0/2 runs the first and third cfg, a cfg's rows together" "shard 0/2: 3 of 4 rows (3 cfgs)" env INF_CANARY_SHARD=0/2 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: shard 1/2 runs the second cfg alone" "shard 1/2: 1 of 4 rows (3 cfgs)" env INF_CANARY_SHARD=1/2 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: the shard's rows are the ones judged" "== canary inf_canary_b:" env INF_CANARY_SHARD=1/2 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect green "canaries: a shard's caught rows are green" env INF_CANARY_SHARD=1/2 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect red "canaries: a shard index at its count is a scope failure" env INF_CANARY_SHARD=2/2 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect red "canaries: a malformed shard is a scope failure" env INF_CANARY_SHARD=1/02 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect red "canaries: a shard past the last cfg holds no row" env INF_CANARY_SHARD=3/4 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
# ADR-0107 D1: the simulator's tests compile only with its `dst` feature,
# so a crate-test row on `inf-sim` passes it and no other package's row
# does — the stub answers only when it sees the feature.
dir=$(canary_fixture canary-sim-dst "$failed_line" "$ok_line")
{
    echo '#!/usr/bin/env bash'
    echo 'case " $* " in'
    echo '  *" --features dst "*) ;;'
    echo '  *) echo "error[E0432]: unresolved import"; exit 101 ;;'
    echo 'esac'
    echo 'if [ -n "${RUSTFLAGS:-}" ]; then'
    echo "$failed_line"
    echo 'else'
    echo "$ok_line"
    echo 'fi'
} >"$dir/cargo"
expect red "canaries: a row on another package does not carry the dst feature" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
echo "inf_canary_fixture crate-test inf-sim lib the_row" >"$dir/rows"
expect green "canaries: a row on inf-sim builds with the dst feature" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
# A test with more than one way to fail: a crate-test row's witness is a
# phrase of the oracle's own assertion, and the planted log must carry it.
# A red on the test's engagement check (`VACUOUS`) is never a catch, with
# or without a witness.
oracle_line='echo "panicked: the oracle saw a stale value: 7"; echo "test the_row ... FAILED"; exit 101'
other_assert_line='echo "panicked: the fixture did not boot"; echo "test the_row ... FAILED"; exit 101'
vacuous_line='echo "panicked: VACUOUS: the fault never fired"; echo "test the_row ... FAILED"; exit 101'
dir=$(canary_fixture canary-witness "$oracle_line" "$ok_line")
echo "inf_canary_fixture crate-test fake test:suite the_row the oracle saw a stale value" >"$dir/rows"
expect green "canaries: a crate-test row FAILED on its witness phrase is a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: the catch quotes the oracle's assertion" "caught: panicked: the oracle saw a stale value: 7" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(canary_fixture canary-wrong-assertion "$other_assert_line" "$ok_line")
echo "inf_canary_fixture crate-test fake test:suite the_row the oracle saw a stale value" >"$dir/rows"
expect red "canaries: a crate-test row FAILED on another assertion is not a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: the wrong-assertion red is named" "expected the oracle's assertion 'the oracle saw a stale value'" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(canary_fixture canary-vacuous "$vacuous_line" "$ok_line")
expect red "canaries: a crate-test row FAILED on its engagement check is not a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: the engagement red is named" "the test's engagement check, not its oracle" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
echo "inf_canary_fixture crate-test fake test:suite the_row VACUOUS: the fault never fired" >"$dir/rows"
expect red "canaries: a witness cannot name the engagement check" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
# Every row's plain control runs first, and its own output (a passing
# test's prints too: `--show-output`) must not carry the witness: a phrase
# the test prints on every run is in any red's log, so it catches nothing.
printing_ok='echo "test the_row ... ok"; echo "---- the_row stdout ----"; echo "the oracle saw a stale value: 0 of 9 checked"; exit 0'
dir=$(canary_fixture canary-witness-printed "$oracle_line" "$printing_ok")
echo "inf_canary_fixture crate-test fake test:suite the_row the oracle saw a stale value" >"$dir/rows"
expect_red_because "canaries: a crate-test witness the plain run prints is no catch" "no catch possible: the plain build prints the witness" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
{
    echo '#!/usr/bin/env bash'
    echo 'if [ -n "${RUSTFLAGS:-}" ]; then'
    echo "$oracle_line"
    echo 'fi'
    echo 'case " $* " in'
    echo '  *" --show-output "*) echo "test the_row ... ok"; echo "the oracle saw a stale value: 0"; exit 0 ;;'
    echo 'esac'
    echo "$ok_line"
} >"$dir/cargo"
expect_red_because "canaries: the crate-test control reads a passing test's own prints" "no catch possible" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
# A scenario row is judged on the planted simulator's log: a line naming the
# row's token is a catch, but a line carrying `VACUOUS` is an engagement
# clause and a catch only where the token is its name (whole, closed by `:`
# or ` —`): a vacuity row. Stub simulators stand at INF_CANARY_SIM_TARGET,
# and the stub cargo builds nothing.
sim_fixture() {
    local name=$1 token=$2 planted=$3 plain=${4:-'echo "inf-sim: scenario fake-scenario: green"'}
    local dir sims
    [ -n "$name" ] && [ -n "$work" ] || { echo "sim_fixture: empty name or work dir" >&2; exit 2; }
    dir="$work/$name"
    sims="$dir/target/canary-inf_canary_fixture/release"
    mkdir -p "$dir/target/release" "$sims"
    echo "inf_canary_fixture fake-scenario $token" >"$dir/rows"
    printf '#!/usr/bin/env bash\nexit 0\n' >"$dir/cargo"
    printf '#!/usr/bin/env bash\n%s\n' "$plain" >"$dir/target/release/inf-sim"
    printf '#!/usr/bin/env bash\n%s\n' "$planted" >"$sims/inf-sim"
    chmod +x "$dir/cargo" "$dir/target/release/inf-sim" "$sims/inf-sim"
    echo "$dir"
}
violation='echo "inf-sim: ORACLE VIOLATION: TOKEN VIOLATION seed 0x1 cell 0: the rule broke"'
engaged='echo "inf-sim: ORACLE VIOLATION: TOKEN VACUOUS seed 0x1 cell 0: the regime was not reached"'
dir=$(sim_fixture sim-caught TOKEN "$violation; exit 1")
expect green "canaries: a scenario row whose planted log names its token is a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-toothless TOKEN "exit 0")
expect_red_because "canaries: a scenario row's planted build that stays green is NOT CAUGHT" "NOT CAUGHT" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-vacuous TOKEN "$engaged; ${engaged/cell 0/cell 1}; exit 1")
expect_red_because "canaries: a scenario row red on its token's engagement clause alone is not a catch" "the leg's engagement clause, not its oracle" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-vacuous-and-caught TOKEN "$engaged; $violation; exit 1")
expect green "canaries: an engagement line beside the oracle's violation is still a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
expect_output "canaries: the catch quotes the violation, not the engagement line" "caught: inf-sim: ORACLE VIOLATION: TOKEN VIOLATION" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-vacuity-row token-unengaged 'echo "inf-sim: ORACLE VIOLATION: VACUOUS seed 0x1: token-unengaged — the plant held nothing"; exit 1')
expect green "canaries: a vacuity row, whose token follows the word, is a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-vacuity-cell token-unengaged 'echo "inf-sim: VIOLATION: VACUOUS seed 0x1 cell 1: token-unengaged — the row never filled"; exit 1')
expect green "canaries: a vacuity row named after the seed and the cell is a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-vacuity-bare token-unengaged 'echo "inf-sim: VIOLATION: VACUOUS token-unengaged: the plant parked nothing"; exit 1')
expect green "canaries: a vacuity row named right after the word is a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-vacuity-name-first token-unengaged 'echo "inf-sim: VIOLATION: token-unengaged: DRAW VACUOUS: the sweep drew nothing"; exit 1')
expect green "canaries: a vacuity row whose clause names it before the word is a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-leg-vacuous TOKEN 'echo "inf-sim: VIOLATION: TOKEN leg VACUOUS: 0 suppressed removals, expected one per index (2)"; exit 1')
expect_red_because "canaries: a scenario row red on its leg's engagement clause alone is not a catch" "the leg's engagement clause, not its oracle" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-clause-text TOKEN 'echo "inf-sim: VIOLATION: other-unengaged: DRAW VACUOUS: the TOKEN reboot drew nothing"; exit 1')
expect_red_because "canaries: a token inside another clause's text is not a catch" "the leg's engagement clause, not its oracle" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-name-prefix token 'echo "inf-sim: VIOLATION: VACUOUS seed 0x1: token-unengaged — the plant held nothing"; exit 1')
expect_red_because "canaries: a token that only begins a clause's name is not a catch" "the leg's engagement clause, not its oracle" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
# A summary line every run prints carries the token: on the planted build
# any red names it (a reboot that failed for another reason, the oracle
# off), so the plain control's log refuses the row before its plant runs.
summary='echo "inf-sim: fake-scenario seed 0x1: TOKEN reboot true, trace 9 bytes"'
dir=$(sim_fixture sim-summary TOKEN "${summary/true/false}; echo \"inf-sim: ORACLE VIOLATION: seed 0x1: another check\"; exit 1" "$summary")
expect_red_because "canaries: a token the plain simulator prints on its summary line is no catch" "no catch possible: the plain build prints the substring 'TOKEN'" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
long_summary="echo \"inf-sim: fake-scenario seed 0x1: $(printf '%0120d' 0) TOKEN reboot true\""
dir=$(sim_fixture sim-summary-long TOKEN "exit 1" "$long_summary")
expect_output "canaries: the refusal quotes the control's line from just before the token" '^   \.\.\.0* TOKEN reboot true$' env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-summary-and-caught TOKEN "${summary/true/false}; $violation; exit 1" "$summary")
expect_red_because "canaries: the oracle's line beside a summary that names the token is still no catch" "no catch possible" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-summary-other-token TOKEN-VIOLATION-NAME "${summary/true/false}; echo \"inf-sim: ORACLE VIOLATION: seed 0x1: TOKEN-VIOLATION-NAME — the rule broke\"; exit 1" "$summary")
expect green "canaries: a token the oracle's line alone carries is caught beside such a summary" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
# Rows on one leg share its control run and its log: the second row is
# judged on the first row's control.
printf '%s\n' "inf_canary_fixture fake-scenario TOKEN-VIOLATION-NAME" "inf_canary_fixture fake-scenario TOKEN" >"$dir/rows"
expect_red_because "canaries: a second row on a leg is judged on the leg's one control" "no catch possible: the plain build prints the substring 'TOKEN'" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
expect_output "canaries: the leg's control runs once" "on the plain build: green above, not run again" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
dir=$(sim_fixture sim-control-red TOKEN "$violation; exit 1" 'echo "inf-sim: ORACLE VIOLATION: seed 0x1: the plain run broke"; exit 1')
expect_red_because "canaries: a scenario row whose plain build is red has no control" "the plain build is red (a canary needs a green control)" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
# INF_CANARY_CONTROLS_ONLY=1 runs every row's control and judges no plant:
# the sweep of the control rule over the table.
dir=$(sim_fixture sim-controls-only TOKEN 'touch "$(dirname "$0")/planted-ran"; exit 0')
expect green "canaries: controls-only reads a clean control green" env INF_CANARY_CONTROLS_ONLY=1 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
expect_output "canaries: controls-only says it judged no plant" "controls only: 1 rows' controls green" env INF_CANARY_CONTROLS_ONLY=1 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
expect red "canaries: controls-only never ran the planted simulator" test -e "$dir/target/canary-inf_canary_fixture/release/planted-ran"
dir=$(sim_fixture sim-controls-only-printed TOKEN "exit 0" "$summary")
expect_red_because "canaries: controls-only refuses a token the control prints" "no catch possible" env INF_CANARY_CONTROLS_ONLY=1 INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
expect_red_because "canaries: a controls-only knob other than 0 or 1 is a scope failure" "INF_CANARY_CONTROLS_ONLY wants 0 or 1" env INF_CANARY_CONTROLS_ONLY=yes INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
# INF_CANARY_SIM_TARGET is the stub route alone (ADR-0204): beside a real
# cargo, here a `cargo` on PATH that builds nothing as a root holding an
# earlier round's build would, the row would judge the simulators the knob
# names, and their stale catch would read green. Refused before any build.
dir=$(sim_fixture sim-target-real-cargo TOKEN "$violation; exit 1")
mkdir -p "$dir/bin" && cp "$dir/cargo" "$dir/bin/cargo" && chmod +x "$dir/bin/cargo"
expect_red_because "canaries: a simulator directory beside a real cargo is a scope failure" "INF_CANARY_SIM_TARGET is the self-test's stub route" env -u INF_CANARY_CARGO PATH="$dir/bin:$PATH" INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
# The same knob beside a named cargo that answers `--version` as cargo does:
# the real one, or a wrapper passing through to it. Refused as well.
dir=$(sim_fixture sim-target-named-real-cargo TOKEN "$violation; exit 1")
printf '#!/usr/bin/env bash\ncase "${1:-}" in --version) echo "cargo 1.95.0 (0123abcd 2026-03-01)" ;; esac\nexit 0\n' >"$dir/cargo"
expect_red_because "canaries: a simulator directory beside a cargo answering as cargo is a scope failure" "unset INF_CANARY_SIM_TARGET" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" INF_CANARY_SIM_TARGET="$dir/target" $CANARY
# ADR-0159 A1.6: a `loom` row builds both legs under `--cfg loom` — the
# stub answers only when it sees it, so a driver that dropped the cfg is
# red for another reason, never a catch.
loom_fixture() {
    local name=$1 planted=$2 plain=$3 dir
    [ -n "$name" ] && [ -n "$work" ] || { echo "loom_fixture: empty name or work dir" >&2; exit 2; }
    dir="$work/$name"
    mkdir -p "$dir"
    echo "inf_canary_fixture loom fake the_row WITNESS:" >"$dir/rows"
    {
        echo '#!/usr/bin/env bash'
        echo 'case "${RUSTFLAGS:-}" in'
        echo '  *"--cfg loom"*inf_canary_fixture*)'
        echo "$planted"
        echo '  ;;'
        echo '  "--cfg loom")'
        echo "$plain"
        echo '  ;;'
        echo '  *) echo "error[E0433]: use of undeclared crate loom"; exit 101 ;;'
        echo 'esac'
    } >"$dir/cargo"
    chmod +x "$dir/cargo"
    echo "$dir"
}
witness_line='echo "panicked: WITNESS: a stale effect"; echo "test the_row ... FAILED"; exit 101'
dir=$(loom_fixture loom-caught "$witness_line" "$ok_line")
expect green "canaries: a loom model FAILED on its witness when planted, ok on the plain loom one" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(loom_fixture loom-other-panic "$failed_line" "$ok_line")
expect red "canaries: a loom model FAILED without its witness assertion is not a catch" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
expect_output "canaries: the witness-less red is named" "expected the model's assertion" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(loom_fixture loom-toothless "$ok_line" "$ok_line")
expect red "canaries: a loom model that stays green when planted is NOT CAUGHT" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(loom_fixture loom-no-control "$witness_line" "$failed_line")
expect red "canaries: a loom model red on the plain loom build is no control leg" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
dir=$(loom_fixture loom-witness-printed "$witness_line" 'echo "test the_row ... ok"; echo "WITNESS: 0 stale effects"; exit 0')
expect_red_because "canaries: a loom witness the plain loom run prints is no catch" "no catch possible: the plain build prints the witness 'WITNESS:'" env INF_CANARY_ROWS_FILE="$dir/rows" INF_CANARY_CARGO="$dir/cargo" $CANARY
echo "inf_canary_fixture loom fake the_row" >"$work/canary-short-loom-row"
expect red "canaries: a loom row without its witness is a scope failure" env INF_CANARY_ROWS_FILE="$work/canary-short-loom-row" INF_CANARY_CARGO="$dir/cargo" $CANARY
}

# --------------------------------------------------- release-assert inventory
group_release_asserts() {
# ADR-0107 D2: a fixture crate with one release assert and one expect, and
# the inventory that names them; each planted drift is red.
RELEASE=./scripts/check-release-asserts.sh

# inventory <root> <rows…>: writes docs/release-assert-inventory.tsv under
# the fixture root from stdin.
inventory() {
    local root=$1
    [ -n "$root" ] && [ -d "$root" ] || { echo "inventory: bad root" >&2; exit 2; }
    mkdir -p "$root/docs"
    cat >"$root/docs/release-assert-inventory.tsv"
}

IFS= read -r -d '' body <<'EOF' || true
pub fn ok(n: u64) -> u64 {
    assert!(n > 0, "n is positive");
    let v: Option<u64> = Some(n);
    v.expect("just built")
}

#[cfg(test)]
mod tests {
    fn scratch() { assert!(false, "never counted"); }
}
EOF
root=$(printf '%s' "$body" | fixture ra-clean)
inventory "$root" <<'EOF'
# fixture
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
I	1	crates/fake/src/lib.rs	expect	just built	built two lines up
EOF
expect green "release-asserts: matching inventory" env INF_CHECK_ROOT="$root" $RELEASE
expect_output "release-asserts: scope line discloses sites and classes" "2 release-panic sites in 2 identities" env INF_CHECK_ROOT="$root" $RELEASE
# Batch 70: the stripped-line count is a number, not the last stripped
# file's temp path (the proof-pointer loop reused the variable).
expect_output "release-asserts: scope line counts stripped lines" "[0-9] test-only lines stripped" env INF_CHECK_ROOT="$root" $RELEASE

IFS= read -r -d '' body <<'EOF' || true
pub fn ok(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
EOF
root=$(printf '%s' "$body" | fixture ra-missing)
expect red "release-asserts: no inventory file is a scope failure" env INF_CHECK_ROOT="$root" $RELEASE

IFS= read -r -d '' body <<'EOF' || true
pub fn ok(n: u64) -> u64 {
    assert!(n > 0, "n is positive");
    assert!(n < 10, "n is small");
    n
}
EOF
root=$(printf '%s' "$body" | fixture ra-new)
inventory "$root" <<'EOF'
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
EOF
expect red "release-asserts: a new site is unclassified" env INF_CHECK_ROOT="$root" $RELEASE

IFS= read -r -d '' body <<'EOF' || true
pub fn ok(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
EOF
root=$(printf '%s' "$body" | fixture ra-stale)
inventory "$root" <<'EOF'
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
I	1	crates/fake/src/lib.rs	expect	gone	vanished
EOF
expect red "release-asserts: a stale row is red" env INF_CHECK_ROOT="$root" $RELEASE

IFS= read -r -d '' body <<'EOF' || true
pub fn a(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
pub fn b(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
EOF
root=$(printf '%s' "$body" | fixture ra-count)
inventory "$root" <<'EOF'
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
EOF
expect red "release-asserts: a second site behind one identity is a count mismatch" env INF_CHECK_ROOT="$root" $RELEASE

# ADR-0107 D2, first amendment (batch 12): a C row's proof pointer must
# RESOLVE — a definition in the named file's production code, by
# rust-symbol-defined.awk over the stripped file. The fixture defines a
# free fn, a `Type::method` inside a multi-line generic `impl Trait for`,
# a const, and a test-only fn that must not count.
IFS= read -r -d '' body <<'EOF' || true
pub fn write(len: usize) { assert!(len <= 255, "caller validated the length"); }
pub fn check_bounds(len: usize) -> bool { len <= 255 }
pub const MAX_LEN: usize = 255;
pub struct Store<T> { inner: T }
pub trait Bounds { fn bound(&self) -> usize; fn other(&self); }
impl<T: Clone + Send> Bounds
    for Store<T>
where
    T: Sync,
{
    fn bound(&self) -> usize { 255 }
    fn other(&self) {}
}
pub mod guard {
    pub fn admit(len: usize) -> bool { len <= 255 }
}

#[cfg(test)]
mod tests {
    pub fn check_bounds_test_only() {}
}
EOF
root=$(printf '%s' "$body" | fixture ra-caller)
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	trust me
EOF
expect red "release-asserts: a C row without a proof pointer is red" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:check_bounds` at every write entry
EOF
expect green "release-asserts: a C row citing a resolving free fn is accepted" env INF_CHECK_ROOT="$root" $RELEASE
expect_output "release-asserts: resolved pointers are counted" "1 proof pointers resolved" env INF_CHECK_ROOT="$root" $RELEASE
# ADR-0106 D14 (batch 36): the resolver exits at the first definition; a
# piped stripper took SIGPIPE under pipefail once the file outgrew the
# pipe buffer (plane.rs at 320 KiB went red on an unrelated edit). The
# fixture defines the symbol first and then outgrows any pipe buffer.
root_big=$(fixture ra-early-def-large-file <<EOF
pub fn write(len: usize) { assert!(len <= 255, "caller validated the length"); }
pub fn check_bounds(len: usize) -> bool { len <= 255 }
$(awk 'BEGIN { for (i = 0; i < 12000; i++) printf "pub fn filler_%d(n: usize) -> usize { n + %d }\n", i, i }')
EOF
)
inventory "$root_big" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:check_bounds` at every write entry
EOF
expect green "release-asserts: an early definition in a file larger than the pipe buffer resolves" env INF_CHECK_ROOT="$root_big" $RELEASE
expect_output "release-asserts: the large-file pointer is counted" "1 proof pointers resolved" env INF_CHECK_ROOT="$root_big" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:Store::bound` (a multi-line generic impl) and `crates/fake/src/lib.rs:MAX_LEN`, `crates/fake/src/lib.rs:Bounds::other`, `crates/fake/src/lib.rs:guard::admit`
EOF
expect green "release-asserts: Type::method inside impl Trait for Type, a const, Trait::method and mod::fn resolve" env INF_CHECK_ROOT="$root" $RELEASE
expect_output "release-asserts: every distinct pointer is resolved" "4 proof pointers resolved" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:check_bound` at every write entry
EOF
expect red "release-asserts: a renamed enforcing function is red (the L20 row)" env INF_CHECK_ROOT="$root" $RELEASE
expect_output "release-asserts: the unresolved pointer is named" "proof pointer 'crates/fake/src/lib.rs:check_bound' does not resolve" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:Other::bound`
EOF
expect red "release-asserts: a method on the wrong type is red" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:check_bounds_test_only`
EOF
expect red "release-asserts: a symbol that exists only under #[cfg(test)] is no proof" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`lib.rs:check_bounds` at every write entry
EOF
expect red "release-asserts: a bare file name is red" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:12` at every write entry
EOF
expect red "release-asserts: a line number is not a proof pointer" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/gone.rs:check_bounds`
EOF
expect red "release-asserts: a pointer into a missing file is red" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
Q	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:check_bounds`
EOF
expect red "release-asserts: an unknown class is red" env INF_CHECK_ROOT="$root" $RELEASE

IFS= read -r -d '' body <<'EOF' || true
pub fn ok(n: u64) -> u64 { debug_assert!(n > 0, "debug only"); n }
EOF
root=$(printf '%s' "$body" | fixture ra-debug)
inventory "$root" <<'EOF'
# nothing: debug asserts are not release sites
EOF
expect red "release-asserts: an inventory with no rows is a scope failure" env INF_CHECK_ROOT="$root" $RELEASE
}

# ---------------------------------------------------------------- clock ban
group_clock_ban() {
# ADR-0106 first amendment (review 2026-08-30 F-L18-05): the type-resolved
# clock ban's gate. Steps 1–3 run on fixtures (the real clippy.toml copied
# in); the cargo-driven probe (step 4) is fixture-skipped and disclosed,
# and runs unconditionally on the real tree inside `just check`.
CLOCK=./scripts/check-clock-ban.sh
clock_fixture() {
    local root
    root=$(fixture "$1")
    cp clippy.toml "$root/clippy.toml"
    echo "$root"
}
IFS= read -r -d '' body <<'EOF' || true
pub fn ok() -> u64 { 1 }
EOF
root=$(printf '%s' "$body" | clock_fixture clock-clean)
expect green "clock-ban: clean crate under the real config" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
expect_output "clock-ban: scope line discloses config, scan and the skipped probe" "config 9/9 entries, 0 shadow configs, 1 cell-source files; allows audited by check-lint-scopes.sh; probe: skipped (fixture mode)" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
[ -n "$root" ] && rm -f "$root/clippy.toml"
expect red "clock-ban: no clippy.toml is red" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
cp clippy.toml "$root/clippy.toml"
sed -i.bak '/std::time::SystemTime::elapsed/d' "$root/clippy.toml"
expect red "clock-ban: a deleted config entry is red" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
cp clippy.toml "$root/clippy.toml"
printf 'disallowed-methods = []\n' >"$root/crates/fake/clippy.toml"
expect red "clock-ban: a shadow clippy.toml in a crate directory is red" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
[ -n "$root" ] && rm -f "$root/crates/fake/clippy.toml"

# Suppression plants moved to the generated lint-scopes matrix below, the
# single owner of all classes and scopes (ADR-0144 I8).
}

# ------------------------------------------------------- waker atomics (D8)
group_waker_atomics() {
# ADR-0106 second amendment (review 2026-08-30 F-L20-03). The gate's three
# defects were all in the scan, so the fixtures ARE synthetic asm: a
# `.Lfunc_beginN:` on the body's first line (the old awk reset there and
# scanned two lines / zero instructions per waker), an x86 `lock` prefix in
# its own tab-separated field (the old mnemonic set anchored at the line
# start and could not match it), and the aarch64 pair. The cargo-driven
# probe is fixture-skipped and disclosed; it always runs on the real tree,
# and its verdict logic is pinned below on fixtures of its asm.
# The scan's fixtures hold no boundary, so they run with the live-boundary check off; the
# boundary's own cases below run without that switch.
WAKER="env INF_WAKER_LIVE=off ./scripts/check-waker-atomics.sh"
WAKER_LIVE=./scripts/check-waker-atomics.sh
waker_asm() { # <name>: writes a fixture .s from stdin, echoes its path
    local out="$work/$1.s"
    cat >"$out"
    echo "$out"
}
# One clean vtable of four bodies, each opening with the DWARF label.
waker_body() { # <sym> <instructions…>
    local sym=$1; shift
    printf '\t.type\t%s,@function\n%s:\n.Lfunc_begin_%s:\n\t.cfi_startproc\n' "$sym" "$sym" "$sym"
    printf '\t%b\n' "$@"
    printf '\t.cfi_endproc\n'
}
waker_vtable() {
    printf '_ZN5probe12WAKER_VTABLE17hE:\n'
    printf '\t.quad\t%s\n' waker_clone waker_wake waker_wake_by_ref waker_drop
    printf '\t.size\t_ZN5probe12WAKER_VTABLE17hE, 32\n'
}
asm=$( { for f in waker_clone waker_wake waker_wake_by_ref waker_drop; do
             waker_body "$f" 'movq\t%rdi, %rax' 'retq'
         done
         waker_vtable; } | waker_asm waker-clean )
expect green "waker: a clean vtable scans green" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
expect_output "waker: the scope line discloses instructions actually scanned" "4 wakers + 0 owner-lane bodies, 8 instruction lines scanned" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER

# Mach-O (batch 70, lane L11 N18): rustc's DWARF labels there are `Lfunc_beginN`
# and `LBBn_m` — no dot. The batch-69 scanner took `Lfunc_begin` for the body's
# owner, so every waker was "unresolved" on macOS and the gate scanned nothing.
macho_body() { # <sym> <instructions…>
    local sym=$1; shift
    printf '\t.globl\t_%s\n_%s:\nLfunc_begin_%s:\n\t.cfi_startproc\n' "$sym" "$sym" "$sym"
    printf '\t%b\n' "$@"
    printf '\t.cfi_endproc\n'
}
macho_vtable() {
    printf '__ZN5probe12WAKER_VTABLE17hE:\n'
    printf '\t.quad\t_%s\n' waker_clone waker_wake waker_wake_by_ref waker_drop
    printf '\n.subsections_via_symbols\n'
}
asm=$( { printf '\t.section\t__TEXT,__text,regular,pure_instructions\n\t.build_version macos, 15, 0\n'
         macho_body waker_clone 'b\tLBB0_2' 'ret'
         for f in waker_wake waker_wake_by_ref waker_drop; do macho_body "$f" 'mov\tx0, x1' 'ret'; done
         macho_vtable; } | waker_asm waker-macho-clean )
expect green "waker: a Mach-O vtable with DWARF labels scans green" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
expect_output "waker: Mach-O bodies are scanned, not swallowed by Lfunc_begin" "4 wakers + 0 owner-lane bodies, 8 instruction lines scanned, 0 callees with no body" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
asm=$( { printf '\t.section\t__TEXT,__text,regular,pure_instructions\n'
         macho_body waker_wake 'cbz\tx0, LBB1_2' 'ldaddal\tw8, w9, [x0]' 'ret'
         for f in waker_clone waker_wake_by_ref waker_drop; do macho_body "$f" 'ret'; done
         macho_vtable; } | waker_asm waker-macho-atomic )
expect red "waker: a Mach-O arm64 atomic past a local label is red" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER

# The F-L20-03 defect itself: an atomic in the SECOND basic block, past the
# local label the old awk stopped at.
asm=$( { waker_body waker_clone 'movq\t%rdi, %rax' 'je\t.LBB0_2' '.LBB0_2:' 'lock\t\tcmpxchgq\t%rcx, (%rax)' 'retq'
         for f in waker_wake waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
         waker_vtable; } | waker_asm waker-lock )
expect red "waker: an x86 lock-prefixed CAS past a local label is red" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
# Atomics are spelled per target, one row each: x86_64; aarch64 LL/SC and LSE;
# the RCpc-immo forms apple-m1 emits for a field at a nonzero offset (`stlur`,
# not `stlr`); the `st<op>` alias; and aarch64 Linux's outline-atomics helper
# calls, called or tail-branched — there the atomic is the CALL (the arm64
# leg's probe CAS and fetch-add scanned clean).
for insn in 'lock\t\txaddq\t%rax, (%rcx)' 'xchgq\t%rax, (%rcx)' 'mfence' 'ldaxr\tw8, [x0]' 'stlxr\tw9, w8, [x0]' 'casal\tw8, w9, [x0]' 'ldaddal\tw8, w9, [x0]' 'swpal\tw8, w9, [x0]' 'dmb\tish' \
    'stlr\tx9, [x8]' 'stlur\tx8, [x0, #8]' 'ldapur\tx0, [x0, #8]' 'staddl\tw8, [x0]' \
    'bl\t__aarch64_cas8_acq_rel' 'bl\t__aarch64_ldadd4_relax' 'bl\t__aarch64_cas16_acq' 'b\t__aarch64_swp1_rel'
do
    asm=$( { waker_body waker_wake 'movq\t%rdi, %rax' "$insn" 'retq'
             for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
             waker_vtable; } | waker_asm waker-insn )
    expect red "waker: planted '$insn'" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
done
# The helper set is LLVM's libcall table, matched whole: libgcc's cache flush
# and a near-miss name are calls, and the plain unscaled load and store are
# not the RCpc forms.
for insn in 'bl\t__aarch64_sync_cache_range' 'bl\t__aarch64_cas8_acq_rel_shim' 'stur\tx8, [x0, #8]' 'ldur\tx0, [x0, #8]'
do
    # A callee's body is in the fixture, so the closed callee list does not decide the case.
    asm=$( { waker_body waker_wake 'movq\t%rdi, %rax' "$insn" 'retq'
             for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
             for f in __aarch64_sync_cache_range __aarch64_cas8_acq_rel_shim; do waker_body "$f" 'ret'; done
             waker_vtable; } | waker_asm waker-insn-clean )
    expect green "waker: '$insn' is not an atomic" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
done
asm=$( { waker_body waker_wake 'cbz\tx0, .LBB1_2' 'bl\t__aarch64_cas8_acq_rel' 'ret'
         for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'ret'; done
         waker_vtable; } | waker_asm waker-outline )
expect_red_because "waker: an outline-atomics call is reported with its helper" \
    'at waker_wake: [0-9]*:__aarch64_cas8_acq_rel' env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
asm=$( { printf '\t.section\t__TEXT,__text,regular,pure_instructions\n'
         macho_body waker_wake 'bl\t___aarch64_ldadd8_acq_rel' 'ret'
         for f in waker_clone waker_wake_by_ref waker_drop; do macho_body "$f" 'ret'; done
         macho_vtable; } | waker_asm waker-macho-outline )
expect red "waker: a Mach-O outline-atomics call (one more underscore) is red" \
    env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
# An atomic one hop out of a waker is still on the waker path.
asm=$( { waker_body waker_drop 'callq\thelper_fn' 'retq'
         for f in waker_clone waker_wake waker_wake_by_ref; do waker_body "$f" 'retq'; done
         waker_body helper_fn 'lock\t\txaddq\t%rax, (%rcx)' 'retq'
         waker_vtable; } | waker_asm waker-hop )
expect red "waker: an atomic one call hop out of a waker is red" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
# The same atomic in a function the vtable never reaches must NOT be reported.
asm=$( { for f in waker_clone waker_wake waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
         waker_body off_path 'lock\t\txaddq\t%rax, (%rcx)' 'retq'
         waker_vtable; } | waker_asm waker-offpath )
expect green "waker: an atomic off the waker path is not reported" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
# Scope errors: no vtable, a short vtable, a body with no instructions.
asm=$( { for f in waker_clone waker_wake waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done; } | waker_asm waker-novtable )
expect red "waker: no RawWakerVTable is a scope error, not a pass" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
asm=$( { waker_body waker_clone 'retq'; waker_body waker_wake 'retq'
         printf '_ZN5probe12WAKER_VTABLE17hE:\n\t.quad\twaker_clone\n\t.quad\twaker_wake\n\t.size\tx, 16\n'; } | waker_asm waker-short )
expect red "waker: a vtable resolving fewer than four wakers is a scope error" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
asm=$( { printf '\t.type\twaker_clone,@function\nwaker_clone:\n.Lfunc_begin_x:\n\t.cfi_startproc\n\t.cfi_endproc\n'
         for f in waker_wake waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
         waker_vtable; } | waker_asm waker-empty )
expect red "waker: a body that scans zero instructions is a scope error" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
# An indirect call leaves the scan for a body no fact names: red, never an
# unfollowed edge. An indirect jump (x86's jump table `jmpq *%rax`, on the
# real waker path today) is counted on the scope line, and a call through the
# GOT — Rust's no-PLT spelling of an extern — is a direct edge.
for insn in 'callq\t*%rax' 'callq\t*8(%rax)' 'blr\tx8'; do
    asm=$( { waker_body waker_wake 'movq\t%rdi, %rax' "$insn" 'retq'
             for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
             waker_vtable; } | waker_asm waker-icall )
    expect_red_because "waker: an indirect call '$insn' on the waker path is red" \
        "INDIRECT CALL on the waker path: waker_wake" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
done
asm=$( { waker_body waker_wake 'jmpq\t*%rax' 'callq\t*memcpy@GOTPCREL(%rip)' 'retq'
         for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
         waker_vtable; } | waker_asm waker-ijump )
expect_output "waker: an indirect jump is counted on the scope line" \
    "1 callees with no body (each listed), 1 indirect jumps" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
expect_output "waker: a call through the GOT is a direct, disclosed edge" \
    "listed callees (no body in this asm): memcpy" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
# The probe's contract (scripts/waker-atomics-probe/src/lib.rs) judged against
# a fixture of its asm. The arm64 Linux shape — CAS and fetch-add as helper
# calls, the SeqCst store as `stlr` — is green; a fetch-add plant that reports
# nothing is red although `waker_wake_by_ref`, whose name it prefixes, is
# atomic, and an atomic `waker_drop` is red although a clean nested
# `waker_drop::inner` is reachable (a name matches its own symbol only, in
# legacy and v0 mangling); so is an off-path function whose atomic the
# scanner cannot see, and a probe whose controls were deleted.
psym() { # <name>: the probe item's symbol, legacy mangling unless PSYM=v0
    if [ "${PSYM:-legacy}" = v0 ]; then
        printf '_RNvCs1a2B3c_19waker_atomics_probe%s%s' "${#1}" "$1"
    else
        printf '_ZN19waker_atomics_probe%s%s17h0123456789abcdefE' "${#1}" "$1"
    fi
}
ppath() { # <module…> <name>: a nested probe item's symbol, legacy mangling unless PSYM=v0
    local part nest=
    if [ "${PSYM:-legacy}" = v0 ]; then
        for part in "${@:1:$#-1}"; do nest="${nest}Nt"; done
        printf '_RNv%sCs1a2B3c_19waker_atomics_probe' "$nest"
        for part in "$@"; do printf '%s%s' "${#part}" "$part"; done
    else
        printf '_ZN19waker_atomics_probe'
        for part in "$@"; do printf '%s%s' "${#part}" "$part"; done
        printf '17h0123456789abcdefE'
    fi
}
# The probe's second vtable, its boundary module and its lane module, in the arm64 Linux shape:
# each boundary body carries a helper call; the four roots call the boundary (one beside an
# atomic), the lane's out-of-line read-modify-write and the nested crate's function; no root
# reaches `settle_held`.
probe_boundary_asm() {
    local f
    for f in clone wake wake_by_ref drop settle_held; do
        waker_body "$(ppath executor foreign "$f")" 'cbz\tx0, .LBB9_2' 'bl\t__aarch64_ldadd8_acq_rel' 'ret'
    done
    waker_body "$(ppath lane fetch_add_one)" 'bl\t__aarch64_ldadd8_acq_rel' 'ret'
    waker_body "$(psym boundary_clone)" "bl\t$(ppath executor foreign clone)" \
        "bl\t$(ppath executor foreign wake_by_ref)" 'ret'
    waker_body "$(psym beside_wake)" "bl\t$(ppath executor foreign wake)" \
        'bl\t__aarch64_ldadd8_acq_rel' 'ret'
    waker_body "$(psym drift_wake_by_ref)" "bl\t$(ppath lane fetch_add_one)" 'ret'
    waker_body "$(psym unlisted_drop)" "bl\t$(ppath executor foreign drop)" \
        'bl\t_ZN26waker_atomics_probe_nested7outline17h0123456789abcdefE' 'ret'
    printf '%s:\n' "$(psym BOUNDARY_WAKER_VTABLE)"
    for f in boundary_clone beside_wake drift_wake_by_ref unlisted_drop; do
        printf '\t.xword\t%s\n' "$(psym "$f")"
    done
    printf '\t.size\t%s, 32\n' "$(psym BOUNDARY_WAKER_VTABLE)"
}
probe_fixture() { # <name> <clone> <wake> <wake_by_ref> <off-path> [<waker_drop insns…>]
    local name=$1 clone=$2 wake=$3 by_ref=$4 off=$5
    shift 5
    [ $# -gt 0 ] || set -- 'strb\tw8, [x0]'
    { waker_body "$(psym waker_clone)" 'cbz\tx0, .LBB2_3' "$clone" 'ret'
      waker_body "$(psym waker_wake)" 'cbz\tx0, .LBB1_2' "$wake" 'ret'
      waker_body "$(psym waker_wake_by_ref)" 'cbz\tx0, .LBB4_2' "$by_ref" 'ret'
      waker_body "$(psym waker_drop)" 'cbz\tx0, .LBB0_2' "$@" 'ret'
      waker_body "$(psym off_path_atomic)" "$off" 'ret'
      printf '%s:\n' "$(psym WAKER_VTABLE)"
      for f in waker_clone waker_wake waker_wake_by_ref waker_drop; do
          printf '\t.xword\t%s\n' "$(psym "$f")"
      done
      printf '\t.size\t%s, 32\n' "$(psym WAKER_VTABLE)"
      probe_boundary_asm; } | waker_asm "$name"
}
tree=$( { for f in waker_clone waker_wake waker_wake_by_ref waker_drop; do waker_body "$f" 'ret'; done
          waker_vtable; } | waker_asm waker-tree )
pasm=$(probe_fixture waker-probe-arm64 'bl\t__aarch64_cas8_acq_rel' 'bl\t__aarch64_ldadd8_acq_rel' \
    'stlr\tx9, [x8]' 'bl\t__aarch64_ldadd8_acq_rel')
expect_output "waker probe: the arm64 Linux shape is 3 plants red, 2 controls green" \
    "probe: fixture asm, 7 of 7 plants red, 3 of 3 controls green" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
expect green "waker probe: the arm64 Linux shape passes" env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
# The controls are exact: a probe copy with both control lines deleted is red.
mkdir -p "$work/wk/scripts"
cp scripts/check-waker-atomics.sh scripts/asm-waker-scan.awk scripts/probe-target.sh "$work/wk/scripts/"
cp -R scripts/waker-atomics-probe "$work/wk/scripts/"
sed -i.bak '/WAKER-PROBE: expect-clean/d; /WAKER-PROBE: expect-unscanned/d' \
    "$work/wk/scripts/waker-atomics-probe/src/lib.rs"
expect_red_because "waker probe: a probe whose controls were deleted is red" \
    "probe declares 0 clean, 0 off-path and 1 boundary controls (expected exactly 1, 1 and 1)" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" INF_WAKER_LIVE=off \
    "$work/wk/scripts/check-waker-atomics.sh"
pasm=$(PSYM=v0 probe_fixture waker-probe-v0 'bl\t__aarch64_cas8_acq_rel' 'bl\t__aarch64_ldadd8_acq_rel' \
    'stlr\tx9, [x8]' 'bl\t__aarch64_ldadd8_acq_rel')
expect_output "waker probe: v0-mangled names match their own symbols" \
    "probe: fixture asm, 7 of 7 plants red, 3 of 3 controls green" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
inner=_ZN19waker_atomics_probe10waker_drop5inner17h0123456789abcdefE
pasm=$(probe_fixture waker-probe-nested 'bl\t__aarch64_cas8_acq_rel' 'bl\t__aarch64_ldadd8_acq_rel' \
    'stlr\tx9, [x8]' 'bl\t__aarch64_ldadd8_acq_rel' "bl\t$inner" 'ldaddal\tw8, w9, [x0]')
waker_body "$inner" 'ret' >>"$pasm"
expect_red_because "waker probe: a clean nested waker_drop::inner does not stand in for waker_drop" \
    "probe waker_drop should scan clean with >0 instructions; it did not" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
pasm=$(probe_fixture waker-probe-prefix 'bl\t__aarch64_cas8_acq_rel' 'mov\tw0, #1' \
    'stlr\tx9, [x8]' 'bl\t__aarch64_ldadd8_acq_rel')
expect_red_because "waker probe: a silent plant is red although a name it prefixes is atomic" \
    "probe waker_wake carries a planted atomic that was NOT reported" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
pasm=$(probe_fixture waker-probe-offpath 'bl\t__aarch64_cas8_acq_rel' 'bl\t__aarch64_ldadd8_acq_rel' \
    'stlr\tx9, [x8]' 'mov\tx0, x1')
expect_red_because "waker probe: an off-path atomic the scanner cannot see is a scope error" \
    "probe off_path_atomic is supposed to carry an atomic the scanner sees" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
expect_red_because "waker probe: the probe-asm hook outside fixture mode is refused" \
    "INF_WAKER_PROBE_ASM without INF_WAKER_ASM" env INF_WAKER_PROBE_ASM="$pasm" $WAKER
# The boundary (ADR-0003 A1.6): the five functions of `executor::foreign` are present, not
# followed, and listed with their atomics; an atomic beside a call to one is red; each must be
# reached; a callee with no body is red unless the closed list holds it.
fsym() { printf '_ZN11inf_runtime8executor7foreign%s%s17h0123456789abcdefE' "${#1}" "$1"; }
boundary_tree() { # <waker_wake's extra insn> <waker_drop's callee> [settle_held reached: yes|no]
    local f settle=${3:-yes}
    for f in clone wake wake_by_ref drop settle_held; do
        waker_body "$(fsym "$f")" 'lock\t\txaddq\t%rax, (%rcx)' 'retq'
    done
    waker_body waker_clone "callq\t$(fsym clone)" 'retq'
    if [ "$settle" = yes ]; then
        waker_body waker_wake "callq\t$(fsym wake)" "callq\t$(fsym settle_held)" "$1" 'retq'
    else
        waker_body waker_wake "callq\t$(fsym wake)" "$1" 'retq'
    fi
    waker_body waker_wake_by_ref "callq\t$(fsym wake_by_ref)" 'callq\t__tls_get_addr@PLT' 'retq'
    waker_body waker_drop "callq\t$(fsym drop)" "callq\t$2" 'retq'
    waker_vtable
}
asm=$(boundary_tree 'movq\t%rdi, %rax' '_RNvNtCsjrHSEGnQ3l9_3std7process5abort' | waker_asm waker-boundary)
expect green "waker boundary: atomics behind the five boundary functions scan green" \
    env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER_LIVE
expect_output "waker boundary: each boundary function is listed with its atomics" \
    "boundary (checked): clone (2 instructions, 1 atomics), drop (2 instructions, 1 atomics)" \
    env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER_LIVE
expect_output "waker boundary: a listed callee is printed, not red" \
    "listed callees (no body in this asm): _RNvNtCsjrHSEGnQ3l9_3std7process5abort __tls_get_addr" \
    env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER_LIVE
asm=$(boundary_tree 'lock\t\txaddq\t%rax, (%rcx)' memcpy | waker_asm waker-beside)
expect_red_because "waker boundary: an atomic beside a boundary call is red" \
    "ATOMIC on the waker's owner lane: waker_wake" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER_LIVE
asm=$(boundary_tree 'movq\t%rdi, %rax' memcpy no | waker_asm waker-dead)
expect_red_because "waker boundary: a boundary function no waker reaches is red" \
    "DEAD BOUNDARY: executor::foreign::settle_held" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER_LIVE
asm=$(boundary_tree 'movq\t%rdi, %rax' _ZN14inf_foundation4wake10WakeBridge5quiet17h0123456789abcdefE |
      waker_asm waker-unlisted)
expect_red_because "waker boundary: a callee the closed list does not hold is red" \
    "UNLISTED CALLEE on the waker's owner lane: waker_drop>_ZN14inf_foundation4wake10WakeBridge5quiet" \
    env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER_LIVE
expect_red_because "waker boundary: the live-boundary switch outside fixture mode is refused" \
    "INF_WAKER_LIVE=off outside fixture mode" env INF_WAKER_LIVE=off $WAKER_LIVE
# The source census (ADR-0003 A1.6): `RawWakerVTable` named in code outside the census's entries
# is red, and so is an entry that names it nowhere; comments and string literals are not code.
census_root() { # <name>: a fixture root whose two entries each name it in code
    local root="$work/census-$1"
    mkdir -p "$root/crates/inf-runtime/src" "$root/crates/inf-runtime/benches/waker_ab" \
        "$root/crates/x/src" "$root/bins" "$root/tests"
    printf 'static V: RawWakerVTable = v();\n' >"$root/crates/inf-runtime/src/executor.rs"
    printf 'use core::task::RawWakerVTable;\n' \
        >"$root/crates/inf-runtime/benches/waker_ab/executor_a.rs"
    printf '// RawWakerVTable\n/* RawWakerVTable */ fn f() -> &str { "RawWakerVTable" }\n' \
        >"$root/crates/x/src/lib.rs"
    echo "$root"
}
root=$(census_root clean)
expect_output "waker census: a name in comments and strings is no name in code" \
    "census: 3 .rs files read, entries:" \
    env INF_CHECK_ROOT="$root" INF_WAKER_ASM="$tree" INF_WAKER_PROBE=off $WAKER
root=$(census_root outside)
printf 'pub static W: core::task::RawWakerVTable = w();\n' >>"$root/crates/x/src/lib.rs"
expect_red_because "waker census: a file outside its entries naming the vtable type is red" \
    "WAKER CENSUS: crates/x/src/lib.rs names RawWakerVTable outside the census's entries" \
    env INF_CHECK_ROOT="$root" INF_WAKER_ASM="$tree" INF_WAKER_PROBE=off $WAKER
root=$(census_root unnamed)
printf '// RawWakerVTable, in a comment only\n' \
    >"$root/crates/inf-runtime/benches/waker_ab/executor_a.rs"
expect_red_because "waker census: an entry that names the vtable type nowhere is red" \
    "WAKER CENSUS: crates/inf-runtime/benches/waker_ab/executor_a.rs is an entry that names RawWakerVTable nowhere" \
    env INF_CHECK_ROOT="$root" INF_WAKER_ASM="$tree" INF_WAKER_PROBE=off $WAKER
}

# ------------------------------------------------------- fault points (D9)
group_fault_points() {
# ADR-0106 second amendment (review 2026-08-30 F-L20-04): "exercised" used
# to mean any textual mention, so a `//!` doc line and an assert's message
# string kept `shadow_twin_read_fail` green with zero arming sites.
FAULTS=./scripts/check-fault-points.sh
fault_fixture() { # <name> <decl-body> <fire-body> <test-body>
    local root="$work/$1"
    [ -n "$1" ] && [ -n "$work" ] || { echo "fault_fixture: empty name" >&2; exit 2; }
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/fake/src" "$root/crates/fake/tests"
    printf '%s\n' "$2" >"$root/crates/fake/src/fault.rs"
    printf '%s\n' "$3" >"$root/crates/fake/src/lib.rs"
    printf '%s\n' "$4" >"$root/crates/fake/tests/t.rs"
    echo "$root"
}
DECL='pub const P_ONE: &str = "p_one";
pub const ALL: &[&str] = &[P_ONE];'
FIRE='pub fn run() -> bool { inf_foundation::fault::fire(crate::fault::P_ONE) }'
root=$(fault_fixture fp-clean "$DECL" "$FIRE" 'fn t() { fault::arm(fake::fault::P_ONE, FaultSpec::Nth(1)); }')
expect green "fault-points: fired in production, armed in a test" env INF_CHECK_ROOT="$root" $FAULTS
expect_output "fault-points: the scope line names the arming zone" "armed p_one <- crate-tests" env INF_CHECK_ROOT="$root" $FAULTS

# The finding, exactly: the only references are a doc line and a message.
root=$(fault_fixture fp-prose "$DECL" "$FIRE" '//! arms fake::fault::P_ONE
fn t() { assert!(x, "fault::P_ONE fired: {}", n); }')
expect red "fault-points: a doc line plus an assert message is not arming" env INF_CHECK_ROOT="$root" $FAULTS

# A plan row is arming; a bare mention next to no spec is not.
root=$(fault_fixture fp-plan "$DECL" "$FIRE" 'fn t() { start_node(vec![(fake::fault::P_ONE, FaultSpec::Always)]); }')
expect green "fault-points: a (POINT, FaultSpec) plan row counts as arming" env INF_CHECK_ROOT="$root" $FAULTS
root=$(fault_fixture fp-mention "$DECL" "$FIRE" 'fn t() { let name = fake::fault::P_ONE; println!("{name}"); }')
expect red "fault-points: naming the const without arming it is red" env INF_CHECK_ROOT="$root" $FAULTS

# A digit in the point name was silently dropped from the inventory.
root=$(fault_fixture fp-digit 'pub const CKPT_V2_TORN: &str = "ckpt_v2_torn";
pub const ALL: &[&str] = &[CKPT_V2_TORN];' 'pub fn run() {}' 'fn t() {}')
expect red "fault-points: a point whose name carries a digit is now counted (and red)" env INF_CHECK_ROOT="$root" $FAULTS

# ALL must agree with the declared consts, in both directions.
root=$(fault_fixture fp-all-missing 'pub const P_ONE: &str = "p_one";
pub const ALL: &[&str] = &[];' "$FIRE" 'fn t() { fault::arm(fake::fault::P_ONE, FaultSpec::Nth(1)); }')
expect red "fault-points: a const missing from ALL is red" env INF_CHECK_ROOT="$root" $FAULTS
root=$(fault_fixture fp-all-extra 'pub const P_ONE: &str = "p_one";
pub const ALL: &[&str] = &[P_ONE, P_GHOST];' "$FIRE" 'fn t() { fault::arm(fake::fault::P_ONE, FaultSpec::Nth(1)); }')
expect red "fault-points: an ALL entry that is not a declared const is red" env INF_CHECK_ROOT="$root" $FAULTS

# An unparsable declaration is a scope error, never a silent skip.
root=$(fault_fixture fp-unparsable 'pub const P_ONE: &str = "p_one";
pub const P_BAD: &str = CONCAT;
pub const ALL: &[&str] = &[P_ONE];' "$FIRE" 'fn t() { fault::arm(fake::fault::P_ONE, FaultSpec::Nth(1)); }')
expect red "fault-points: a declaration the extractor cannot parse is a scope error" env INF_CHECK_ROOT="$root" $FAULTS

# Firing is read from production code only.
root=$(fault_fixture fp-testfire "$DECL" '#[cfg(test)]
mod tests {
    fn f() { inf_foundation::fault::fire(crate::fault::P_ONE); }
}' 'fn t() { fault::arm(fake::fault::P_ONE, FaultSpec::Nth(1)); }')
expect red "fault-points: a fire site that exists only in a test module is unwired" env INF_CHECK_ROOT="$root" $FAULTS
}

# ---------------------------------------------------- fsync fail-stop (D10)
group_fsync_fail_stop() {
# ADR-0106 second amendment (review 2026-08-30 F-L20-06): the allow-list was
# per file, so a catch-and-continue inside a 1,722-line allow-listed file
# passed; and the pattern set was five hand-listed names, blind to a raw
# discarded sync.
FSYNC=./scripts/check-fsync-fail-stop.sh
fsync_fixture() { # <name>: crate src/lib.rs from stdin + the required types
    local root
    root=$(fixture "$1")
    # Only three lines here match a derived pattern (`FsyncFailed`, the
    # `Fsync(FsyncFailed)` variant, `on_fsync_error`); a bare
    # `Fsync(io::Error)` variant of another enum does not, because the
    # pattern is the qualified path. Each is marked so the fixture
    # isolates the case under test.
    cat >"$root/crates/fake/src/types.rs" <<'TYPES'
// fsync-fail-stop-allow: the type itself
pub struct FsyncFailed {
    pub source: std::io::Error,
}
pub enum LogError {
    // fsync-fail-stop-allow: the variant declaration
    Fsync(FsyncFailed),
}
pub enum TierWriteFailure {
    Fsync(std::io::Error),
}
pub enum TierFlushError {
    Fsync { source: std::io::Error },
}
pub enum ExtentWriteFailure {
    Fsync(std::io::Error),
}
impl Commit {
    // fsync-fail-stop-allow: the freeze hook itself; the caller fail-stops
    pub fn on_fsync_error(&mut self) {}
}
TYPES
    echo "$root"
}
IFS= read -r -d '' body <<'EOF' || true
pub fn ok() -> u64 { 1 }
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-clean)
expect green "fsync: the declarations alone, each marked, are green" env INF_CHECK_ROOT="$root" $FSYNC
expect_output "fsync: the scope line discloses the derived pattern set" "derived fsync-error patterns" env INF_CHECK_ROOT="$root" $FSYNC

IFS= read -r -d '' body <<'EOF' || true
pub fn swallow(r: Result<(), LogError>) {
    if let Err(LogError::Fsync(_)) = r {}
}
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-catch)
expect red "fsync: a catch-and-continue anywhere is red (there is no file allow-list)" env INF_CHECK_ROOT="$root" $FSYNC

IFS= read -r -d '' body <<'EOF' || true
pub fn seal(file: &mut std::fs::File) {
    let _ = file.sync_data();
}
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-discard)
expect red "fsync: a discarded raw sync_data is red with no named type involved" env INF_CHECK_ROOT="$root" $FSYNC
IFS= read -r -d '' body <<'EOF' || true
pub fn seal(file: &mut std::fs::File) -> std::io::Result<()> {
    file.sync_data()?;
    Ok(())
}
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-discard-ok)
expect green "fsync: a propagated sync_data is green" env INF_CHECK_ROOT="$root" $FSYNC

IFS= read -r -d '' body <<'EOF' || true
pub enum CkptWriteFailure {
    Write(std::io::Error),
    Fsync(std::io::Error),
}
pub fn barrier(r: Result<(), CkptWriteFailure>) {
    if let Err(CkptWriteFailure::Fsync(_)) = r {}
}
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-newtype)
expect red "fsync: a brand-new fsync error type is derived and gated" env INF_CHECK_ROOT="$root" $FSYNC

IFS= read -r -d '' body <<'EOF' || true
// fsync-fail-stop-allow:
pub fn swallow(r: Result<(), LogError>) {
    if let Err(LogError::Fsync(_)) = r {}
}
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-bare)
expect red "fsync: a bare marker (no reason) does not audit a site" env INF_CHECK_ROOT="$root" $FSYNC

IFS= read -r -d '' body <<'EOF' || true
// fsync-fail-stop-allow: guards nothing
pub fn ok() -> u64 { 1 }
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-stale)
expect red "fsync: a marker guarding no site is stale scope" env INF_CHECK_ROOT="$root" $FSYNC

IFS= read -r -d '' body <<'EOF' || true
//! LogError::Fsync is non-recoverable by contract (§8.4).
/// Returns TierFlushError::Fsync on a failed barrier.
pub fn ok() -> u64 { 1 }
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-prose)
expect green "fsync: prose naming the contract is not a site" env INF_CHECK_ROOT="$root" $FSYNC

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() -> u64 { 1 }

#[cfg(test)]
mod tests {
    fn t(r: Result<(), LogError>) { if let Err(LogError::Fsync(_)) = r {} }
}
EOF
root=$(printf '%s' "$body" | fsync_fixture fs-testmod)
expect green "fsync: a test-only module is stripped" env INF_CHECK_ROOT="$root" $FSYNC

# The inventory + dep-DAG fixtures plant whole crate trees and a `cargo`
# shim, so they are a Python suite; its cases join this script's tally.
inventory_dag_log="$work/inventory-dag.log"
if python3 "$SCRIPT_DIR/check-inventory-dag-selftest.py" >"$inventory_dag_log" 2>&1; then
    inventory_dag_cases=$(sed -n 's/^Ran \([0-9]*\) tests.*/\1/p' "$inventory_dag_log")
    pass=$((pass + $inventory_dag_cases))
    case_log "inventory / dep-DAG fixtures" green 0 pass "$inventory_dag_log" "$inventory_dag_cases"
else
    fail=$((fail + 1))
    echo "SELFTEST FAIL: inventory / dep-DAG fixtures"
    sed 's/^/    | /' "$inventory_dag_log"
    case_log "inventory / dep-DAG fixtures" green 1 fail "$inventory_dag_log"
fi

# Batch 64 (the 100-column gate wraps long reasons): the marker heads the
# comment block above the site, continuation lines included.
root=$(fsync_fixture fs-wrapped <<'RS'
mod types;
use types::*;
impl Commit {
    // fsync-fail-stop-allow: the reason is long enough to wrap onto a second
    // comment line, and the marker still guards the site below the block
    pub fn on_fsync_error(&mut self) {}
}
RS
)
expect green "fsync-fail-stop: a wrapped reason still pairs with the site under its block" env INF_CHECK_ROOT="$root" $FSYNC
root=$(fsync_fixture fs-cut-off <<'RS'
mod types;
use types::*;
impl Commit {
    // fsync-fail-stop-allow: a marker cut off from its site by code
    fn unrelated() {}
    pub fn on_fsync_error(&mut self) {}
}
RS
)
expect red "fsync-fail-stop: a marker separated from the site by code guards nothing" env INF_CHECK_ROOT="$root" $FSYNC
}

# ------------------------------------------------- unsafe roots (batch 44)
# ADR-0121 (review 2026-08-30 F-L17-09): `inf-alloc` and `inf-runtime` — the
# two named audited leaves — carried no root attribute at all, so a new
# unsafe block anywhere in them produced no lint and no diff signal.
unsafe_fixture() { # <name> <crate> <lib.rs body> [module file] [module body]
    local root="$work/$1"
    [ -n "$1" ] && [ -n "$2" ] && [ -n "$work" ] || { echo "unsafe_fixture: empty name" >&2; exit 2; }
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/$2/src" "$root/bins" "$root/tests"
    printf '[package]\nname = "%s"\n' "$2" >"$root/crates/$2/Cargo.toml"
    printf '%s\n' "$3" >"$root/crates/$2/src/lib.rs"
    if [ -n "${4:-}" ]; then printf '%s\n' "$5" >"$root/crates/$2/src/$4"; fi
    echo "$root"
}
group_unsafe_roots() {
ROOTS=./scripts/check-unsafe-roots.sh
root=$(unsafe_fixture ur-forbid safe '#![forbid(unsafe_code)]
pub fn f() {}')
expect green "unsafe-roots: a forbid root outside the leaf list" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="" $ROOTS
expect_output "unsafe-roots: the scope line counts the roots" "1 crate roots, 0 deny crates" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="" $ROOTS

# The finding, exactly: a listed leaf whose root carries no attribute.
root=$(unsafe_fixture ur-naked leaf 'pub mod m;' m.rs 'pub unsafe fn f() {}')
expect red "unsafe-roots: a root with neither forbid nor deny (the F-L17-09 shape)" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS

# The ADR-0049 posture: deny at the root, a module-scoped allow, on the list.
root=$(unsafe_fixture ur-deny leaf '#![deny(unsafe_code)]
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod m;' m.rs 'pub unsafe fn f() {}')
expect green "unsafe-roots: deny root plus a module allow on a listed leaf" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS
expect_output "unsafe-roots: the scope line names the deny set" "1 deny crates (leaf), 1 module-scoped allows" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS
expect red "unsafe-roots: the same deny root outside the leaf list" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="" $ROOTS

# List drift the other way: a listed leaf that is forbid has left the list.
root=$(unsafe_fixture ur-stale leaf '#![forbid(unsafe_code)]
pub fn f() {}')
expect red "unsafe-roots: a listed leaf carrying forbid is a stale list" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS

# Allows are module-scoped only.
root=$(unsafe_fixture ur-fn-allow leaf '#![deny(unsafe_code)]
pub mod m;' m.rs '#[allow(unsafe_code)]
pub fn f() {}')
expect red "unsafe-roots: an allow on a function is not module-scoped" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS
root=$(unsafe_fixture ur-inner leaf '#![deny(unsafe_code)]
pub mod m;' m.rs '//! the whole module is the audit surface
#![allow(unsafe_code)]
pub unsafe fn f() {}')
expect green "unsafe-roots: a whole-module inner allow (the log_bytes.rs shape)" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS
root=$(unsafe_fixture ur-inline leaf '#![deny(unsafe_code)]
#[allow(unsafe_code)]
mod imp { pub unsafe fn f() {} }')
expect green "unsafe-roots: an inline mod item (the group16.rs per-arch shape)" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS
root=$(unsafe_fixture ur-forbid-allow safe '#![forbid(unsafe_code)]
#[allow(unsafe_code)]
pub mod m;' m.rs 'pub fn f() {}')
expect red "unsafe-roots: an allow under a forbid root" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="" $ROOTS

# A binary crate's own root counts; the library root where the unsafe
# lives is the one the finding caught ungoverned.
root=$(unsafe_fixture ur-bin leaf '#![deny(unsafe_code)]
#[allow(unsafe_code)]
pub mod m;' m.rs 'pub unsafe fn f() {}')
printf 'fn main() {}\n' >"$root/crates/leaf/src/main.rs"
expect red "unsafe-roots: a binary root with no attribute" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS
printf '#![forbid(unsafe_code)]\nfn main() {}\n' >"$root/crates/leaf/src/main.rs"
expect green "unsafe-roots: both roots governed" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="leaf" $ROOTS

# Scope: a manifest without a root, and an empty tree, are failures.
root=$(unsafe_fixture ur-scope leaf '#![deny(unsafe_code)]')
rm -f "$root/crates/leaf/src/lib.rs"
expect red "unsafe-roots: a manifest with no crate root is a scope error" env INF_CHECK_ROOT="$root" INF_UNSAFE_LEAVES="" $ROOTS
mkdir -p "$work/ur-empty/crates" "$work/ur-empty/bins" "$work/ur-empty/tests"
expect red "unsafe-roots: zero crate roots is a scope error" env INF_CHECK_ROOT="$work/ur-empty" INF_UNSAFE_LEAVES="" $ROOTS
expect red "unsafe-roots: a missing top-level directory is a scope error" env INF_CHECK_ROOT="$work/ur-empty/crates" INF_UNSAFE_LEAVES="" $ROOTS
}

# ---------------------------------------------- safety inventory (batch 44)
group_safety_inventory() {
# The verdict line below had claimed this gate since ADR-0106 with no
# case behind it (found while adding the unsafe-roots cases).
INVENTORY=./scripts/check-safety-inventory.sh
root=$(unsafe_fixture si-gap leaf '#![deny(unsafe_code)]
#[allow(unsafe_code)]
pub mod m;' m.rs 'pub unsafe fn f() {}')
expect red "safety-inventory: an unsafe file no SAFETY.md names" env INF_CHECK_ROOT="$root" $INVENTORY
printf '| `src/m.rs` | invariant | coverage |\n' >"$root/crates/leaf/SAFETY.md"
expect green "safety-inventory: the file named by its exact path" env INF_CHECK_ROOT="$root" $INVENTORY
printf '| `src/m.rs.old` | invariant | coverage |\n' >"$root/crates/leaf/SAFETY.md"
expect red "safety-inventory: a path that only contains the file name is not naming it" env INF_CHECK_ROOT="$root" $INVENTORY
}

# --------------------------------------------- style limits (ADR-0125, batch 64)
group_style_limits() {
FILELEN=./scripts/check-file-length.sh
LINEW=./scripts/check-line-width.sh
# Batch 69 (ADR-0106 D2, macOS tier): a heredoc inside `$( )` whose body
# holds an unbalanced parenthesis is a parse error under bash 3.2 — this
# script aborted at its first such fixture and `just check` still saw exit
# 0. Fixtures are read into a variable first; every gate must parse here.
parse_ok=0
for gate in ./scripts/*.sh; do
    bash -n "$gate" || parse_ok=1
done
expect green "scripts: every gate parses under this bash ($BASH_VERSION)" [ "$parse_ok" -eq 0 ]
# style_root <name>: crates/fake/src + bins/fake/src + tests/ so every gate's
# scope assertion is satisfied; the caller writes the files.
style_root() {
    local root="$work/$1"
    [ -n "$1" ] && [ -n "$work" ] || { echo "style_root: empty name" >&2; exit 2; }
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/fake/src" "$root/bins/fake/src" "$root/tests/t" "$root/docs"
    printf 'fn main() {}\n' >"$root/bins/fake/src/main.rs"
    printf 'fn t() {}\n' >"$root/tests/t/t.rs"
    echo "$root"
}
root=$(style_root fl-over)
{ printf 'pub fn f() {\n'; for _ in $(seq 1 2999); do printf '    let _x = 1;\n'; done; printf '}\n'; } >"$root/crates/fake/src/lib.rs"
expect red "file-length: 3001 production lines" env INF_CHECK_ROOT="$root" $FILELEN
{ printf 'pub fn f() {\n'; for _ in $(seq 1 2499); do printf '    let _x = 1;\n'; done; printf '}\n#[cfg(test)]\nmod tests {\n'; for _ in $(seq 1 600); do printf '    fn t() {}\n'; done; printf '}\n'; } >"$root/crates/fake/src/lib.rs"
expect green "file-length: 3103 lines of which 602 are a test module" env INF_CHECK_ROOT="$root" $FILELEN
expect_output "file-length: the OK line discloses the largest file" "largest: 2501 crates/fake/src/lib.rs" env INF_CHECK_ROOT="$root" $FILELEN
[ -n "$root" ] && [ -d "$root/bins" ] && rm -rf "$root/bins"
expect red "file-length: a missing bins/ is a scope error, not a skip" env INF_CHECK_ROOT="$root" $FILELEN
root=$(style_root lw)
printf 'pub fn f() {}\n// %s\n' "$(printf 'x%.0s' $(seq 1 97))" >"$root/crates/fake/src/lib.rs"
expect green "line-width: a 100-column comment" env INF_CHECK_ROOT="$root" $LINEW
printf 'pub fn f() {}\n// %s\n' "$(printf 'x%.0s' $(seq 1 98))" >"$root/crates/fake/src/lib.rs"
expect red "line-width: a 101-column comment (rustfmt would pass it)" env INF_CHECK_ROOT="$root" $LINEW
printf 'pub fn f() {}\nconst S: &str = "%s";\n' "$(printf 'y%.0s' $(seq 1 90))" >"$root/crates/fake/src/lib.rs"
expect red "line-width: a 109-column string literal" env INF_CHECK_ROOT="$root" $LINEW
printf 'pub fn f() {}\n' >"$root/crates/fake/src/lib.rs"
printf '// %s\n' "$(printf 'z%.0s' $(seq 1 120))" >"$root/tests/t/t.rs"
expect red "line-width: tests/ is in scope" env INF_CHECK_ROOT="$root" $LINEW
[ -n "$root" ] && [ -d "$root/tests" ] && rm -rf "$root/tests"
expect red "line-width: a missing tests/ is a scope error" env INF_CHECK_ROOT="$root" $LINEW
}

# ------------------------------------------------------------ lint-ratchet
group_lint_ratchet() {
# ADR-0144 D3 (absorbs ADR-0125's fn-length ratchet). Fixtures are captured
# clippy JSON; the approved copies are real commits in a fixture repository.
RATCHET="$SCRIPT_DIR/check-lint-ratchet.sh"
ls_commit() { fx_git "$1" add -A && fx_git "$1" -c user.name=t -c user.email=t@t commit -q -m "$2"; }
rt_msg() { # rt_msg <lint> <file> <line> <col>
    printf '{"reason":"compiler-message","message":{"code":{"code":"clippy::%s"},"level":"warning","message":"m","children":[],"spans":[{"file_name":"%s","line_start":%s,"column_start":%s,"is_primary":true}]}}\n' "$1" "$2" "$3" "$4"
}
rt_done() { printf '{"reason":"build-finished","success":true}\n'; }
rt_root() {
    local root="$work/$1"
    [ -n "$1" ] && [ -n "$work" ] || { echo "rt_root: empty name" >&2; exit 2; }
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/fake/src" "$root/docs" "$root/scripts"
    printf 'pub fn f() {}\n' >"$root/crates/fake/src/lib.rs"
    printf 'pub fn g() {}\n' >"$root/crates/fake/src/dec.rs"
    printf 't/x\tcrates/fake/src/dec.rs\tcast,arith\tratchet\n' >"$root/docs/lint-scopes.tsv"
    git -C "$root" init -q
    ls_commit "$root" "before the gate"
    fx_git "$root" branch -q base-tip
    printf '# gate\n' >"$root/scripts/check-lint-ratchet.sh"
    echo "$root"
}
rt_run() { env INF_CHECK_ROOT="$1" INF_LINT_BASE_REF=base-tip INF_LINT_RATCHET_INPUT="$1/clippy.json" INF_LINT_RATCHET_HOST="${2:-Linux}" "$RATCHET"; }
root=$(rt_root rt)
F=crates/fake/src/lib.rs
D=crates/fake/src/dec.rs
{ rt_msg too_many_lines $F 3 1; rt_msg too_many_lines $F 9 1; rt_done; } >"$root/clippy.json"
printf 'fn_length\t2\t%s\n' $F >"$root/docs/lint-baseline.tsv"
expect green "lint-ratchet: two breaches, baseline 2 (the introducing change)" rt_run "$root"
{ rt_msg too_many_lines $F 3 1; rt_msg too_many_lines $F 9 1; rt_msg too_many_lines $F 3 1; rt_msg too_many_lines $F 9 1; rt_done; } >"$root/clippy.json"
expect green "lint-ratchet: the same two spans emitted twice count 2, not 4" rt_run "$root"
printf 'fn_length\t1\t%s\n' $F >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: a new breach above the row" rt_run "$root"
printf 'fn_length\t3\t%s\n' $F >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: a stale row above the tree" rt_run "$root"
printf '# empty\n' >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: a breach with no row" rt_run "$root"
printf 'fn_length\t2\t%s\nfn_length\t1\tcrates/fake/src/linux_only.rs\n' $F >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: a row with no breach is the ratchet on Linux" rt_run "$root"
expect green "lint-ratchet: a row with no breach is a note on another host" rt_run "$root" Darwin
expect_output "lint-ratchet: the skipped row is disclosed" "not compiled on Darwin" rt_run "$root" Darwin
for bad in 'fn_length\toops\t'$F 'fn_length\t0\t'$F 'fn_length\t2\t/etc/passwd' 'nonsense\t2\t'$F 'fn_length\t2\t'$F'\textra' 'fn_length\t2\t'$F'\nfn_length\t1\t'$F; do
    printf "$bad\n" >"$root/docs/lint-baseline.tsv"
    expect red "lint-ratchet: a malformed table is a scope error ($bad)" rt_run "$root"
done
printf '# no rows\n' >"$root/docs/lint-baseline.tsv"
rt_done >"$root/clippy.json"
expect green "lint-ratchet: zero breaches and an empty table pass (the goal)" rt_run "$root"
: >"$root/clippy.json"
expect red "lint-ratchet: no completed build is a scope error, not a clean tree" rt_run "$root"
rm -f "$root/docs/lint-baseline.tsv"
rt_done >"$root/clippy.json"
expect red "lint-ratchet: a missing table is a scope error" rt_run "$root"
# cast / arith count only on the files the scope table ratchets
{ rt_msg arithmetic_side_effects $D 4 9; rt_msg cast_possible_truncation $D 5 9; rt_msg cast_sign_loss $D 5 9; rt_msg arithmetic_side_effects $F 2 1; rt_done; } >"$root/clippy.json"
printf 'arith\t1\t%s\ncast\t1\t%s\n' $D $D >"$root/docs/lint-baseline.tsv"
expect green "lint-ratchet: cast/arith count on scoped files only; two cast lints on one span are one site" rt_run "$root"
# the approved copies
ls_commit "$root" "introduce the table: arith 1, cast 1"
expect green "lint-ratchet: the committed table" rt_run "$root"
{ rt_msg arithmetic_side_effects $D 4 9; rt_msg arithmetic_side_effects $D 8 9; rt_msg cast_possible_truncation $D 5 9; rt_done; } >"$root/clippy.json"
printf 'arith\t2\t%s\ncast\t1\t%s\n' $D $D >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: a violation with its row raised to match, uncommitted (HEAD leg)" rt_run "$root"
ls_commit "$root" "combined change"
expect red "lint-ratchet: the combined change committed is still red (introducing-commit leg)" rt_run "$root"
fx_git "$root" reset -q --hard HEAD~1
# a fix does not buy a violation: -1 in dec.rs, +1 in an existing file
printf 't/x\t%s\tcast,arith\tratchet\nt/x\t%s\tcast,arith\tratchet\n' $D $F >"$root/docs/lint-scopes.tsv"
{ rt_msg arithmetic_side_effects $F 2 1; rt_msg cast_possible_truncation $D 5 9; rt_done; } >"$root/clippy.json"
printf 'arith\t1\t%s\ncast\t1\t%s\n' $F $D >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: sites fixed in one file do not pay for new ones in an existing file" rt_run "$root"
# a split carries its counts to an added file
fx_git "$root" reset -q --hard HEAD
printf 'pub fn h() {}\n' >"$root/crates/fake/src/split.rs"
printf 't/x\t%s\tcast,arith\tratchet\nt/x\tcrates/fake/src/split.rs\tcast,arith\tratchet\n' $D >"$root/docs/lint-scopes.tsv"
{ rt_msg arithmetic_side_effects crates/fake/src/split.rs 1 1; rt_msg cast_possible_truncation $D 5 9; rt_done; } >"$root/clippy.json"
printf 'arith\t1\tcrates/fake/src/split.rs\ncast\t1\t%s\n' $D >"$root/docs/lint-baseline.tsv"
fx_git "$root" add -A
expect green "lint-ratchet: a split carries its count to an added file" rt_run "$root"
expect_output "lint-ratchet: the moved row is printed" "moved: arith crates/fake/src/split.rs" rt_run "$root"
fx_git "$root" reset -q --hard HEAD
expect red "lint-ratchet: an unresolvable base ref is a scope error" env INF_CHECK_ROOT="$root" INF_LINT_BASE_REF=no-such-ref INF_LINT_RATCHET_INPUT="$root/clippy.json" "$RATCHET"
# two changes that each pass: the base tip moved down, this branch did not
fx_git "$root" checkout -q -b feature
fx_git "$root" checkout -q -B base-tip
printf 'cast\t1\t%s\n' $D >"$root/docs/lint-baseline.tsv"
ls_commit "$root" "PR1: arith fixed on the base"
fx_git "$root" checkout -q feature
{ rt_msg arithmetic_side_effects $D 4 9; rt_msg cast_possible_truncation $D 5 9; rt_done; } >"$root/clippy.json"
expect red "lint-ratchet: a branch that is green against its merge base is red against the base tip" rt_run "$root"
# a row counts its own family in its own scope: casts denied where
# arithmetic ratchets, and an item scope counts the item's lines only
root=$(rt_root rt-family)
printf 't/x\t%s\tcast\tdeny\nt/x\t%s\tarith\tratchet\n' $D $D >"$root/docs/lint-scopes.tsv"
{ rt_msg arithmetic_side_effects $D 4 9; rt_done; } >"$root/clippy.json"
printf 'arith\t1\t%s\n' $D >"$root/docs/lint-baseline.tsv"
expect green "lint-ratchet: casts denied, arithmetic ratcheted (the control)" rt_run "$root"
{ rt_msg arithmetic_side_effects $D 4 9; rt_msg arithmetic_side_effects $D 8 9; rt_done; } >"$root/clippy.json"
expect red "lint-ratchet: arithmetic rises in a scope whose casts are denied" rt_run "$root"
{ rt_msg arithmetic_side_effects $D 4 9; rt_msg cast_possible_truncation $D 5 9; rt_done; } >"$root/clippy.json"
printf 'arith\t1\t%s\ncast\t1\t%s\n' $D $D >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: a narrowing in a cast-denied scope cannot be bought with a row" rt_run "$root"
printf 'pub fn a() {\n}\npub fn g() {\n    let _brace = "}";\n}\npub fn z() {}\n' >"$root/$D"
printf 't/x\t%s::g\tcast,arith\tratchet\n' $D >"$root/docs/lint-scopes.tsv"
{ rt_msg arithmetic_side_effects $D 4 9; rt_msg arithmetic_side_effects $D 1 5; rt_msg cast_sign_loss $D 6 1; rt_done; } >"$root/clippy.json"
printf 'arith\t1\t%s\n' $D >"$root/docs/lint-baseline.tsv"
expect green "lint-ratchet: an item scope counts the item's lines, not its file's" rt_run "$root"
{ rt_msg arithmetic_side_effects $D 4 9; rt_msg arithmetic_side_effects $D 5 1; rt_done; } >"$root/clippy.json"
expect red "lint-ratchet: a new site inside the item" rt_run "$root"
printf 't/x\t%s::gone\tcast,arith\tratchet\n' $D >"$root/docs/lint-scopes.tsv"
expect red "lint-ratchet: an item scope naming no function is a scope error" rt_run "$root"
printf 't/x\t%s\tcast,bogus\tratchet\n' $D >"$root/docs/lint-scopes.tsv"
expect red "lint-ratchet: an unknown lint family is a scope error" rt_run "$root"
# The calls: both clippy passes run in the gate's own target directory,
# `lint-ratchet-<key>` under the checkout's target root (a pass in the recipe's
# target would re-check every member at each switch of lint arguments; the key
# keeps a copied checkout's passes off the original's), with the lint set the
# backlog counts. A stub cargo first on PATH records each call; a copy of the
# gate with one part of a call changed is red.
RT_BIN="$work/rt-bin"
mkdir -p "$RT_BIN"
IFS= read -r -d '' body <<'EOF' || true
#!/usr/bin/env bash
# The stub cargo: records the call, then reports a finished build.
set -eu
{
    printf 'call:'
    printf ' %s' "$@"
    printf '\n'
} >>"$RT_CALLS"
printf '{"reason":"build-finished","success":true}\n'
EOF
printf '%s' "$body" >"$RT_BIN/cargo"
chmod +x "$RT_BIN/cargo"
root=$(rt_root rt-call)
printf '# no rows\n' >"$root/docs/lint-baseline.tsv"
printf 'too-many-lines-threshold = 70\n' >"$root/clippy.toml"
rt_call_run() { # <gate> <root> [CARGO_TARGET_DIR]: the gate's cargo passes, recorded
    rm -f "$2.calls"
    if [ -n "${3:-}" ]; then
        env PATH="$RT_BIN:$PATH" RT_CALLS="$2.calls" CARGO_TARGET_DIR="$3" INF_CHECK_ROOT="$2" \
            INF_LINT_BASE_REF=base-tip INF_LINT_RATCHET_HOST=Linux "$1"
    else
        env -u CARGO_TARGET_DIR PATH="$RT_BIN:$PATH" RT_CALLS="$2.calls" INF_CHECK_ROOT="$2" \
            INF_LINT_BASE_REF=base-tip INF_LINT_RATCHET_HOST=Linux "$1"
    fi
}
rt_calls_ok() { # <calls file> <the target directory both calls must name>
    python3 - "$1" "$2" <<'PY'
import sys
calls = [line.split(" ")[1:] for line in open(sys.argv[1], encoding="utf-8").read().split("\n") if line]
lints = ["-W", "clippy::too_many_lines", "-W", "clippy::cast_possible_truncation",
         "-W", "clippy::cast_sign_loss", "-W", "clippy::cast_possible_wrap",
         "-W", "clippy::arithmetic_side_effects", "--force-warn", "clippy::disallowed_methods",
         "--force-warn", "clippy::disallowed_types"]
tail = ["--lib", "--bins", "--message-format=json", "--target-dir", sys.argv[2], "--", *lints]
want = [["clippy", "--workspace", "--exclude", "inf-sim", *tail],
        ["clippy", "-p", "inf-sim", "--features", "dst", *tail]]
if calls != want:
    print("calls:", *(" ".join(c) for c in calls), sep="\n    ")
    sys.exit(1)
PY
}
# The verdict reads the passes' output (the scope audit the gate runs next needs
# a real tree, so its status here is not the case's).
expect_output "lint-ratchet: the verdict reads the recorded passes' report" \
    "lint-ratchet OK: no backlog" rt_call_run "$RATCHET" "$root"
rt_key=$(fx_root_key "$root")
rt_own="$(cd "$root" && pwd -P)/target/lint-ratchet-$rt_key"
expect green "lint-ratchet: both passes name <root>/target/lint-ratchet-<key>, CARGO_TARGET_DIR unset" \
    rt_calls_ok "$root.calls" "$rt_own"
rt_target="$work/rt-target"
rt_refused() { # <gate> <CARGO_TARGET_DIR>: the gate stops on the helper's refusal, before cargo
    local status=0
    rt_call_run "$1" "$root" "$2" >"$work/rt-refused" 2>&1 || status=$?
    [ "$status" -ne 0 ] && [ ! -s "$root.calls" ] && grep -q 'LINT-RATCHET SCOPE ERROR' "$work/rt-refused" &&
        grep -q "CARGO_TARGET_DIR is '$2'" "$work/rt-refused" && return 0
    echo "exit $status:"
    cat "$work/rt-refused"
    [ ! -s "$root.calls" ] || { echo "cargo ran:"; cat "$root.calls"; }
    return 1
}
expect green "lint-ratchet: a CARGO_TARGET_DIR naming another directory stops it before cargo" \
    rt_refused "$RATCHET" "$rt_target"
ln -s "$root" "$work/rt-link"
for rt_var in "$(cd "$root" && pwd -P)/target" target "$work/rt-link/target"; do
    rt_call_run "$RATCHET" "$root" "$rt_var" >/dev/null 2>&1 || true
    expect green "lint-ratchet: CARGO_TARGET_DIR=$rt_var, the checkout's target, names the kept directory" \
        rt_calls_ok "$root.calls" "$rt_own"
done
rt_mutant() { # <name> <sed expression> <CARGO_TARGET_DIR or ''> <what the copy does>
    local dir="$work/rt-gate-$1"
    rm -rf "${dir:?}"
    mkdir -p "$dir"
    cp "$SCRIPT_DIR/lint_scope_table.py" "$SCRIPT_DIR/target-root.sh" "$dir/"
    sed "$2" "$RATCHET" >"$dir/check-lint-ratchet.sh"
    chmod +x "$dir/check-lint-ratchet.sh"
    if cmp -s "$RATCHET" "$dir/check-lint-ratchet.sh"; then
        echo "selftest: the lint-ratchet mutant '$1' changed nothing" >&2
        exit 2
    fi
    if [ -n "$3" ]; then
        expect red "lint-ratchet: the refusal's check is red on a gate that $4" \
            rt_refused "$dir/check-lint-ratchet.sh" "$3"
        return
    fi
    rt_call_run "$dir/check-lint-ratchet.sh" "$root" >/dev/null 2>&1 || true
    expect red "lint-ratchet: the calls' check is red on a gate that $4" rt_calls_ok "$root.calls" "$rt_own"
}
RT_KEPT='^    TARGET_DIR=$(target_root_kept lint-ratchet)$'
rt_mutant shared "s|$RT_KEPT|    TARGET_DIR=\$TARGET_ROOT|" '' "shares the recipe's target directory"
RT_SETUP='^    target_root_setup "$(pwd -P)" ||$'
rt_mutant raw "s#$RT_SETUP#    true ||#;s|$RT_KEPT|    TARGET_DIR=\"\${CARGO_TARGET_DIR:-target}/lint-ratchet\"|" \
    "$rt_target" "skips the helper and reads CARGO_TARGET_DIR itself"
rt_mutant unkeyed "s|$RT_KEPT|    TARGET_DIR=\"\$TARGET_ROOT/lint-ratchet\"|" '' "keeps an unkeyed directory"
rt_mutant second 's|^            --target-dir "$TARGET_DIR" -- $LINTS$|            -- $LINTS|' \
    '' "drops the simulator pass's --target-dir"
rt_mutant first 's|^            --target-dir "$TARGET_DIR" -- $LINTS &&$|            -- $LINTS \&\&|' \
    '' "drops the workspace pass's --target-dir"
rt_mutant lint 's/^LINTS="-W clippy::too_many_lines /LINTS="/' '' "drops a lint the backlog counts"
}

# ---------------------------------------------------------- doc artifacts
group_doc_artifacts() {
DOCS=./scripts/check-doc-artifacts.sh
doc_case="$work/doc-artifacts"
doc_root="$doc_case/infinitydb"
mkdir -p "$doc_root/docs"
git init -q "$doc_root"
printf '[workspace]\n' >"$doc_root/Cargo.toml"
printf '# Architecture\n\nSee [the style](INFINITY_STYLE.md).\n' >"$doc_root/docs/ARCHITECTURE.md"
printf '# Style\n' >"$doc_root/docs/INFINITY_STYLE.md"
printf '> **GENERATED — do not edit.**\n' >"$doc_root/docs/compat-matrix.md"
expect green "docs: standalone workspace" env INF_CHECK_ROOT="$doc_root" $DOCS
expect_output "docs: the OK line states the published-link scope" \
    "published links: 1 relative links in 3 Markdown files" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '[gone](missing.md)\n' >>"$doc_root/docs/ARCHITECTURE.md"
expect red "docs: a relative link naming no file" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '# Architecture\n\nSee [the style](INFINITY_STYLE.md).\n' >"$doc_root/docs/ARCHITECTURE.md"
expect_output "docs: standalone discloses the absent governance scope" "parent governance absent, not validated" env INF_CHECK_ROOT="$doc_root" $DOCS
rm -f "$doc_root/Cargo.toml"
expect red "docs: workspace manifest missing" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '[workspace]\n' >"$doc_root/Cargo.toml"
rm -f "$doc_root/docs/compat-matrix.md"
expect red "docs: generated matrix missing" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '> **GENERATED — do not edit.**\n' >"$doc_root/docs/compat-matrix.md"
mkdir -p "$doc_case/docs/adr"
mkdir -p "$doc_case/docs/milestones"
printf '# M0\n' >"$doc_case/docs/milestones/m0.md"
printf '# Master plan\n' >"$doc_case/docs/infinity-master-plan.md"
cat >"$work/matrix-pointer" <<'EOF'
# Compatibility matrix

The generated [compatibility matrix](../infinitydb/docs/compat-matrix.md) lives in the Rust workspace.
EOF
cp "$work/matrix-pointer" "$doc_case/docs/compat-matrix.md"
printf '# ADR-NNNN: Title\n' >"$doc_case/docs/adr/0000-template.md"
printf '# ADR-0072: Projection seam\n' >"$doc_case/docs/adr/0072-seam.md"
expect green "docs: governance with unique identities and a pointer" env INF_CHECK_ROOT="$doc_root" $DOCS
expect_output "docs: governance scope names its ADR count" "1 ADR numbers" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '# ADR-0072: Frame decoder\n' >"$doc_case/docs/adr/0072-frame.md"
expect red "docs: duplicate ADR number" env INF_CHECK_ROOT="$doc_root" $DOCS
rm -f "$doc_case/docs/adr/0072-frame.md"
printf '# ADR-0126: Wrong title\n' >"$doc_case/docs/adr/0072-seam.md"
expect red "docs: title and filename disagree" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '' >"$doc_case/docs/adr/0072-seam.md"
expect red "docs: empty ADR" env INF_CHECK_ROOT="$doc_root" $DOCS
rm -f "$doc_case/docs/adr/0072-seam.md"
expect red "docs: template alone is an empty decision set" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '# ADR-0072: Projection seam\n' >"$doc_case/docs/adr/0072-seam.md"
printf '# A decision with no number\n' >"$doc_case/docs/adr/decision.md"
expect red "docs: unnumbered decision" env INF_CHECK_ROOT="$doc_root" $DOCS
rm -f "$doc_case/docs/adr/decision.md"
mv "$doc_case/docs/adr" "$doc_case/adr-saved"
expect red "docs: governance without ADR directory" env INF_CHECK_ROOT="$doc_root" $DOCS
mv "$doc_case/adr-saved" "$doc_case/docs/adr"
rm -f "$doc_case/docs/infinity-master-plan.md"
expect red "docs: governance without master plan" env INF_CHECK_ROOT="$doc_root" $DOCS
printf '# Master plan\n' >"$doc_case/docs/infinity-master-plan.md"
rm -f "$doc_case/docs/compat-matrix.md"
expect red "docs: governance without pointer" env INF_CHECK_ROOT="$doc_root" $DOCS
cp "$doc_root/docs/compat-matrix.md" "$doc_case/docs/compat-matrix.md"
expect red "docs: second generated matrix" env INF_CHECK_ROOT="$doc_root" $DOCS
cp "$work/matrix-pointer" "$doc_case/docs/compat-matrix.md"
printf '\n65 commands\n' >>"$doc_case/docs/compat-matrix.md"
expect red "docs: counts added to the pointer" env INF_CHECK_ROOT="$doc_root" $DOCS

cp "$work/matrix-pointer" "$doc_case/docs/compat-matrix.md"
mkdir -p "$doc_root/tests/fixtures" "$doc_root/bins/inf-sim/seeds"
printf 'regression input\n' >"$doc_root/tests/fixtures/artifacts.txt"
printf '0xC0FFEE\n' >"$doc_root/bins/inf-sim/seeds/regression.txt"
fx_git "$doc_root" add tests/fixtures/artifacts.txt bins/inf-sim/seeds/regression.txt
expect green "docs: regression inputs and seeds belong in source" env INF_CHECK_ROOT="$doc_root" $DOCS
for output in .artifacts/gate.log artifacts/claim.json tests/fuzz/artifacts/crash; do
    mkdir -p "$doc_root/$(dirname "$output")"
    printf 'generated output\n' >"$doc_root/$output"
    expect green "docs: local output may exist ($output)" env INF_CHECK_ROOT="$doc_root" $DOCS
    fx_git "$doc_root" add -f "$output"
    expect red "docs: tracked output is forbidden ($output)" env INF_CHECK_ROOT="$doc_root" $DOCS
    fx_git "$doc_root" rm -q --cached -f "$output"
done
}

# ------------------------------------------------------------- lint-scopes
# ADR-0144 D1/D2: the crate-root wildcard deny, the structural suppression
# audit (one planted case per spelling that compiles clean under the deny)
# and the frozen ADR-0143 exemption table against its approved copies —
# fixture repositories with real commits, because the approved copies are
# history (HEAD, the base tip, the table's introducing commit).
#
# The fixtures run a copy of the gate, with the probe, the toolchain pin and
# clippy.toml beside it as in the workspace: its code unchanged, its two
# constants set to the fixture's own by ls_gate (a container table's sum is
# its CONTAINER_EXEMPTIONS_MAX exactly, so an empty fixture table runs at 0).
# ls_setup: the gate copy every lint-scopes group runs, built in its own work
# directory, its two constants at 0.
ls_setup() {
LS_GATE="$work/gate"
mkdir -p "$LS_GATE/crates"
cp -R "$SCRIPT_DIR" "$LS_GATE/scripts"
cp rust-toolchain.toml clippy.toml "$LS_GATE/"
# The probe's slot plants depend on the engine's inf-foundation (ADR-0144
# A2) and its listing plants on inf-log (ADR-0202 D7): the gate copy
# resolves them under its own root, with a workspace whose members are those
# crates and inf-log's own (each manifest inherits from the workspace).
for ls_crate in inf-foundation inf-simd inf-log; do
    cp -R "crates/$ls_crate" "$LS_GATE/crates/$ls_crate"
    rm -rf "$LS_GATE/crates/$ls_crate/fuzz"
done
sed 's|^members = .*|members = ["crates/*"]|' Cargo.toml >"$LS_GATE/Cargo.toml"
grep -q '^members = \["crates/\*"\]$' "$LS_GATE/Cargo.toml" ||
    { echo "selftest: the gate copy's workspace members were not rewritten" >&2; exit 2; }
LINTSCOPES="$LS_GATE/scripts/check-lint-scopes.sh"
ls_gate 0 0
}
ls_gate() { # <maximum> <backing sites>
    sed -i.bak -e "s/^CONTAINER_EXEMPTIONS_MAX=[0-9][0-9]*\$/CONTAINER_EXEMPTIONS_MAX=$1/" \
        -e "s/^CAPPED_BACKING_SITES=[0-9][0-9]*\$/CAPPED_BACKING_SITES=$2/" "$LINTSCOPES"
    rm -f "$LINTSCOPES.bak"
    grep -q "^CONTAINER_EXEMPTIONS_MAX=$1\$" "$LINTSCOPES" && grep -q "^CAPPED_BACKING_SITES=$2\$" "$LINTSCOPES" ||
        { echo "ls_gate: the gate's constants are not where the copy sets them" >&2; exit 2; }
}
LS_ATTR='#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]'
LS_DENY='#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]'
# ls_fuzz_rows <root> <target…>: the fixture crate's fuzz manifest, one
# [[bin]] row a target — the gate reads the targets from these rows.
ls_fuzz_rows() {
    local root=$1 name
    shift
    printf '[package]\nname = "fake-fuzz"\n\n[package.metadata]\ncargo-fuzz = true\n\n[workspace]\n' \
        >"$root/crates/fake/fuzz/Cargo.toml"
    for name in "$@"; do
        printf '\n[[bin]]\nname = "%s"\npath = "fuzz_targets/%s.rs"\n' "$name" "$name" \
            >>"$root/crates/fake/fuzz/Cargo.toml"
    done
}
ls_scopes() {
    mkdir -p "$1/crates/fake/fuzz/fuzz_targets" "$1/docs"
    printf '// fuzz\n' >"$1/crates/fake/fuzz/fuzz_targets/t.rs"
    ls_fuzz_rows "$1" t
    printf 'fake/t\tnone: the fixture target enters no decoder\t-\t-\n' >"$1/docs/lint-scopes.tsv"
    printf '# file\titem\tcount\trecord\texpiry\n' >"$1/docs/container-exemptions.tsv"
}
ls_root() {
    local root="$work/$1"
    [ -n "$1" ] && [ -n "$work" ] || { echo "ls_root: empty name" >&2; exit 2; }
    [ -e "$root" ] && rm -rf "$root"
    mkdir -p "$root/crates/fake/src" "$root/bins/fake/src" "$root/docs" "$root/scripts"
    for entry in "${CELL_CRATE_EXCLUDE[@]}"; do
        mkdir -p "$root/${entry%%|*}"
    done
    printf 'name = "fake"\n' >"$root/crates/fake/Cargo.toml"
    printf 'name = "fakebin"\n' >"$root/bins/fake/Cargo.toml"
    printf '%s\npub fn f() {}\n' "$LS_ATTR" >"$root/crates/fake/src/lib.rs"
    printf '%s\nfn main() {}\n' "$LS_ATTR" >"$root/bins/fake/src/main.rs"
    printf '# file\tfn\tcolumn\n' >"$root/docs/lint-exemptions.tsv"
    printf '# gate\n' >"$root/scripts/check-lint-scopes.sh"
    cp clippy.toml "$root/clippy.toml"  # the container class reads its paths there
    ls_scopes "$root"
    git -C "$root" init -q
    fx_git "$root" add -A
    fx_git "$root" -c user.name=t -c user.email=t@t commit -q -m base
    fx_git "$root" branch -q base-tip
    echo "$root"
}
ls_run() { env INF_CHECK_ROOT="$1" INF_LINT_BASE_REF=base-tip "$LINTSCOPES"; }
ls_commit() { fx_git "$1" add -A && fx_git "$1" -c user.name=t -c user.email=t@t commit -q -m "$2"; }
ls_probe() { env INF_CHECK_ROOT="$root" INF_LINT_BASE_REF=base-tip INF_LINT_PROBE=on INF_LINT_PROBE_SRC="$1" "$LINTSCOPES"; }

group_lint_scopes_audit() {
ls_setup
root=$(ls_root ls-clean)
expect green "lint-scopes: clean roots, empty table" ls_run "$root"
printf 'pub fn f() {}\n' >"$root/crates/fake/src/lib.rs"
expect red "lint-scopes: a crate root without the D1 attribute" ls_run "$root"
root=$(ls_root ls-audit)
ls_case() {
    local want=$1 label=$2 body=$3
    printf '%s\n%s\n' "$LS_ATTR" "$body" >"$root/crates/fake/src/lib.rs"
    expect "$want" "lint-scopes: $label" ls_run "$root"
}
# The lint table, read once: a listing that fails or prints nothing stops the
# self-test here instead of running no case.
LS_RULES=$(env INF_LINT_RULES=1 INF_CHECK_ROOT="$root" "$LINTSCOPES")
[ -n "$LS_RULES" ] || { echo "selftest: the gate listed no lint rule" >&2; exit 2; }
while IFS=$'\t' read -r lint scope cls; do
    cls=${cls%:}
    if [ "$cls" = - ]; then
        # No class: a reasoned allow at any scope is red too (ADR-0144 A2).
        ls_case red "$lint — admits no allow, however reasoned" "#[allow(clippy::$lint, reason = \"consumed before the await\")]
pub async fn f() {}"
        expect_output "lint-scopes: $lint — the refusal names its rule" "$lint admits no allow" ls_run "$root"
    fi
    ls_case red "$lint — allow without a reason" "#[allow(clippy::$lint)]
pub fn f() {}"
    ls_case red "$lint — expect hides it" "#[expect(clippy::$lint, reason = \"$cls: x\")]
pub fn f() {}"
    ls_case red "$lint — cfg_attr-wrapped allow" "#[cfg_attr(not(test), allow(clippy::$lint, reason = \"$cls: x\"))]
pub fn f() {}"
    ls_case red "$lint — inner allow in a module" "pub mod m {
    #![allow(clippy::$lint, reason = \"$cls: x\")]
}"
    ls_case red "$lint — allow on an impl" "pub struct S;
#[allow(clippy::$lint, reason = \"$cls: x\")]
impl S {}"
    ls_case red "$lint — a reason of no class" "#[allow(clippy::$lint, reason = \"it is fine\")]
pub fn f() {}"
    [ "$cls" = - ] && continue
    ls_case green "$lint — a reasoned function-level allow (multi-line)" "#[allow(
    clippy::$lint,
    reason = \"$cls: stated over two lines, through another attribute\"
)]
pub fn f() {}"
done <<<"$LS_RULES"
ls_case red "filesystem methods cannot be allowed on a whole function" '#[allow(clippy::disallowed_methods, reason = "boot: filesystem setup")]
pub fn f() {}'
ls_case green "filesystem method allow is on its statement" 'pub fn f() {
    #[allow(clippy::disallowed_methods, reason = "boot: filesystem setup")]
    let _ = std::fs::read("config");
}'
ls_case red "a combined allow must satisfy every lint class" '#[allow(clippy::disallowed_types, clippy::arithmetic_side_effects, reason = "boot: file handle")]
pub fn f() {}'
ls_case red "type aliases cannot launder disallowed types" '#[allow(clippy::disallowed_types, reason = "boot: alias")]
type F = std::fs::File;'
ls_case green "test-only module remains outside the API audit" '#[allow(clippy::disallowed_methods, clippy::disallowed_types, reason = "test-only: scratch files")]
#[cfg(test)]
mod tests {
    fn f() { let _ = std::fs::read("scratch"); }
}'
ls_case green "bare test-only API allow is outside production" '#[cfg(test)]
mod tests {
    #[allow(clippy::disallowed_methods)]
    fn f() {}
}'
ls_case red "a group cannot hide behind an allowed lint" '#[allow(clippy::all, clippy::arithmetic_side_effects, reason = "bound: fixture")]
pub fn f() {}'
# Resolved diagnostics use the same structural audit. These fixtures exercise
# class laundering and missing carriers; the real probe supplies compiler rows.
ls_api_diag() {
    python3 - "$root" "$1" "$2" <<'PY'
import json, sys
from pathlib import Path
root, api, count = Path(sys.argv[1]), sys.argv[2], int(sys.argv[3])
source = root / "crates/fake/src/lib.rs"
line = next(n for n, text in enumerate(source.read_text().splitlines(), 1) if "let _ =" in text)
rows = []
for _ in range(count):
    if api != "none":
        rows.append({"reason": "compiler-message", "message": {
            "code": {"code": "clippy::disallowed_methods"},
            "message": f"use of a disallowed method `{api}`",
            "spans": [{"is_primary": True, "file_name": "crates/fake/src/lib.rs", "line_start": line}]
        }})
    rows.append({"reason": "build-finished", "success": True})
(root / "api.json").write_text("\n".join(json.dumps(row) for row in rows) + "\n")
PY
}
ls_api_run() { env INF_CHECK_ROOT="$root" INF_LINT_BASE_REF=base-tip INF_LINT_API_DIAGNOSTICS="$root/api.json" "$LINTSCOPES"; }
ls_case green "filesystem statement fixture" 'pub fn f() {
    #[allow(clippy::disallowed_methods, reason = "boot: fixture")]
    let _ = std::fs::read("config");
}'
ls_api_diag std::fs::read 2
expect green "lint-scopes: resolved filesystem call under boot" ls_api_run
ls_api_diag std::time::Instant::now 2
expect red "lint-scopes: boot allow cannot hide a clock read" ls_api_run
ls_api_diag std::fs::read 1
expect red "lint-scopes: missing feature-set carrier" ls_api_run
ls_api_diag none 2
expect red "lint-scopes: force-warn carrier produced no calls" ls_api_run
ls_api_diag std::fs::read 2
sed -i.bak '1d' "$root/api.json"
expect red "lint-scopes: one feature-set carrier emitted no calls" ls_api_run
ls_api_diag std::fs::read 2
printf '\n#[allow(clippy::disallowed_types, reason = "boot: fixture handle")]\npub struct Handle { pub file: std::fs::File }\n' >>"$root/crates/fake/src/lib.rs"
expect red "lint-scopes: method warnings cannot stand in for the type carrier" ls_api_run
ls_case green "clock function fixture" '#[allow(clippy::disallowed_methods, reason = "clock: injected origin")]
pub fn f() {
    let _ = std::time::Instant::now();
}'
ls_api_diag std::time::Instant::now 2
expect green "lint-scopes: resolved clock call under clock" ls_api_run
ls_api_diag std::fs::read 2
expect red "lint-scopes: clock function cannot hide a filesystem call" ls_api_run
ls_api_diag std::thread::sleep 2
expect red "lint-scopes: clock function cannot hide a blocking sleep" ls_api_run
# ADR-0202 D7: a whole listing is allowed only under `listing:`, and
# `listing:` admits no other API.
ls_case green "listing statement fixture" 'pub fn f(fs: &F) {
    #[allow(clippy::disallowed_methods, reason = "listing: ckpt/: the named checkpoint and orphans")]
    let _ = fs.list_dir(dir);
}'
ls_api_diag inf_log::fs::SegmentFs::list_dir 2
expect green "lint-scopes: a whole listing under listing" ls_api_run
ls_case green "boot statement over a listing fixture" 'pub fn f(fs: &F) {
    #[allow(clippy::disallowed_methods, reason = "boot: a whole directory")]
    let _ = fs.list_dir(dir);
}'
ls_api_diag inf_log::fs::SegmentFs::list_dir 2
expect red "lint-scopes: a boot allow cannot hide a whole listing" ls_api_run
ls_case green "listing statement over a filesystem call fixture" 'pub fn f() {
    #[allow(clippy::disallowed_methods, reason = "listing: ckpt/: a fixture bound")]
    let _ = std::fs::read("config");
}'
ls_api_diag std::fs::read 2
expect red "lint-scopes: a listing allow cannot hide a filesystem call" ls_api_run
for group in clippy::pedantic clippy::restriction clippy::style clippy::all warnings; do
    ls_case red "group suppression $group" "#[allow($group)]
pub fn f() {}"
done
cls=foreign
expect_output "lint-scopes: every allow is listed on the OK line" "allow: crates/fake/src/lib.rs" ls_run "$(
    ls_case green "listed allow" "#[allow(clippy::wildcard_enum_match_arm, reason = \"$cls: x\")]
pub fn f() {}" >/dev/null
    echo "$root"
)"
}

group_lint_scopes_table() {
ls_setup
# the scope table (ADR-0144 D2): targets and rows name each other, and a
# `deny` row's file carries the attribute
root=$(ls_root ls-table)
printf '// fuzz\n' >"$root/crates/fake/fuzz/fuzz_targets/new_decoder.rs"
ls_fuzz_rows "$root" t new_decoder
expect_red_because "lint-scopes: a fuzz target no row names" \
    'fuzz target `fake/new_decoder` is named by no row' ls_run "$root"
rm "$root/crates/fake/fuzz/fuzz_targets/new_decoder.rs"
ls_fuzz_rows "$root" t
printf 'fake/gone\tcrates/fake/src/lib.rs\tcast,arith\tratchet\n' >>"$root/docs/lint-scopes.tsv"
expect red "lint-scopes: a row naming a target that is gone" ls_run "$root"
ls_scopes "$root"
printf 'fake/t\tcrates/fake/src/nope.rs\tcast,arith\tratchet\n' >"$root/docs/lint-scopes.tsv"
expect red "lint-scopes: a row naming a file that does not exist" ls_run "$root"
printf 'fake/t\tcrates/fake/src/lib.rs\tcast,arith\tdeny\n' >"$root/docs/lint-scopes.tsv"
expect red "lint-scopes: a deny row whose file lacks the attribute" ls_run "$root"
printf '%s\n%s\npub fn f() {}\n' "$LS_ATTR" "$LS_DENY" >"$root/crates/fake/src/lib.rs"
expect green "lint-scopes: a deny row whose file carries it" ls_run "$root"
printf '%s\npub fn f() {}\n' "$LS_ATTR" >"$root/crates/fake/src/lib.rs"
printf 'fake/t\tcrates/fake/src/lib.rs::f\tcast,arith\tdeny\n' >"$root/docs/lint-scopes.tsv"
expect red "lint-scopes: an item-scoped deny row without the attribute on the item" ls_run "$root"
printf 'fake/t\tnone: x\t-\t-\n' >"$root/docs/lint-scopes.tsv"
expect red "lint-scopes: a none row states its reason" ls_run "$root"
printf 'fake/t\tcrates/fake/src/lib.rs\tcast,arith\tsomeday\n' >"$root/docs/lint-scopes.tsv"
expect red "lint-scopes: an unknown tier" ls_run "$root"
rm "$root/docs/lint-scopes.tsv"
expect red "lint-scopes: a missing scope table is a scope error" ls_run "$root"
# the tier is per (scope, family): casts are denied where arithmetic still
# ratchets, and the attribute sits on the scope the row names — not elsewhere
root=$(ls_root ls-family)
L=crates/fake/src/lib.rs
LS_CAST='#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )
)]'
LS_CAST_ITEM='#[cfg_attr(
    not(test),
    deny(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap)
)]'
ls_mixed() { printf 'fake/t\t%s\tcast\tdeny\nfake/t\t%s\tarith\tratchet\n' "$1" "$1" >"$root/docs/lint-scopes.tsv"; }
ls_lib() { printf '%s\n' "$LS_ATTR" "$@" >"$root/$L"; }
ls_mixed $L
ls_lib "$LS_CAST" 'pub fn f() {}'
expect green "lint-scopes: casts denied, arithmetic ratcheted (the control)" ls_run "$root"
ls_lib "$LS_DENY" 'pub fn f() {}'
expect red "lint-scopes: the file denies the arithmetic its row still ratchets" ls_run "$root"
ls_lib 'pub fn f() {}'
expect red "lint-scopes: a mixed scope without its cast deny" ls_run "$root"
ls_lib 'pub fn f() {}' "$LS_CAST_ITEM" 'pub fn g() {}'
expect red "lint-scopes: the file's deny sits on one of its functions" ls_run "$root"
ls_lib 'pub mod m {' "$LS_CAST" '}' 'pub fn f() {}'
expect red "lint-scopes: the file's deny sits inside a nested module" ls_run "$root"
ls_mixed $L::f
ls_lib "/// doc" "$LS_CAST_ITEM" '#[inline]' 'pub fn f() {}' 'pub fn g() {}'
expect green "lint-scopes: an item scope carries its deny on the item" ls_run "$root"
ls_lib 'pub fn f() {}' "$LS_CAST_ITEM" 'pub fn g() {}'
expect red "lint-scopes: an item scope's deny sits on its neighbour" ls_run "$root"
ls_lib "$LS_CAST_ITEM" 'pub fn f() {}' 'pub mod m {' '    pub fn f() {}' '}'
expect red "lint-scopes: an item scope naming two functions is ambiguous" ls_run "$root"
ls_mixed $L::nope
ls_lib "$LS_CAST_ITEM" 'pub fn f() {}'
expect red "lint-scopes: an item scope naming no function" ls_run "$root"
ls_lib 'pub fn f() {}'
for bad in "$L\tcast,bogus\tratchet" "$L\tcast,cast\tratchet\nfake/t\t$L\tarith\tratchet" \
    "$L\tcast\tratchet" "$L\tcast,arith\tratchet\nfake/t\t$L\tcast\tdeny" \
    "$L\tcast,arith\tratchet\nfake/t\t$L::f\tcast,arith\tratchet" \
    "$L\tcast,arith\tratchet\nfake/t\t$L\tcast,arith\tratchet" \
    "none: the fixture target enters no decoder\tcast\t-"; do
    printf "fake/t\t$bad\n" >"$root/docs/lint-scopes.tsv"
    expect red "lint-scopes: a malformed or conflicting family row ($bad)" ls_run "$root"
done
ls_scopes "$root"
ls_lib "$LS_CAST" 'pub fn f() {}'
expect red "lint-scopes: a decoder deny no row names" ls_run "$root"
# the frozen exemption table
root=$(ls_root ls-exempt)
ls_exempt() { printf '%s\n#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column k")]\npub fn %s() {}\n' "$LS_ATTR" "$1" >"$root/crates/fake/src/lib.rs"; }
ls_exempt f
expect red "lint-scopes: an ADR-0143 allow with no row" ls_run "$root"
printf 'crates/fake/src/lib.rs\tf\tk\n' >>"$root/docs/lint-exemptions.tsv"
expect red "lint-scopes: the row arrives with its allow — the table grew against HEAD" ls_run "$root"
# a repository whose history starts before the gate and its table
root=$(ls_root ls-exempt2)
for ls_part in .git scripts docs; do
    [ -n "$root" ] && [ -e "$root/$ls_part" ] && rm -rf "${root:?}/$ls_part"
done
git -C "$root" init -q
ls_commit "$root" "before the gate"
fx_git "$root" branch -q base-tip
mkdir -p "$root/scripts" "$root/docs"
printf "# gate\n" >"$root/scripts/check-lint-scopes.sh"
ls_scopes "$root"
printf '# file\tfn\tcolumn\n' >"$root/docs/lint-exemptions.tsv"
printf '%s\n' '#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column k")]' 'pub fn f() {}' >"$root/crates/fake/src/a.rs"
printf 'crates/fake/src/a.rs\tf\tk\n' >>"$root/docs/lint-exemptions.tsv"
expect green "lint-scopes: the introducing change (no gate at any copy) is the bootstrap" ls_run "$root"
printf '# gate\n' >"$root/scripts/check-lint-scopes.sh"
ls_commit "$root" intro
expect green "lint-scopes: committed table, one allow per row" ls_run "$root"
printf '%s\n' '#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column k")]' 'pub fn g() {}' >>"$root/crates/fake/src/a.rs"
printf 'crates/fake/src/a.rs\tg\tk\n' >>"$root/docs/lint-exemptions.tsv"
expect red "lint-scopes: a new exemption with its row, uncommitted (HEAD leg)" ls_run "$root"
ls_commit "$root" combined
expect red "lint-scopes: the same change committed is still red (introducing-commit leg)" ls_run "$root"
fx_git "$root" reset -q --hard HEAD~1
fx_git "$root" mv crates/fake/src/a.rs crates/fake/src/moved.rs
printf 'crates/fake/src/moved.rs\tf\tk\n' >"$root/docs/lint-exemptions.tsv"
fx_git "$root" add -A
expect green "lint-scopes: a renamed file keeps its row" ls_run "$root"
fx_git "$root" reset -q --hard HEAD
printf '%s\n' '#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column k")]' 'pub fn f2() {}' >"$root/crates/fake/src/extra.rs"
sed -i.bak 's/pub fn f2/pub fn f/' "$root/crates/fake/src/extra.rs" && rm "$root/crates/fake/src/extra.rs.bak"
expect red "lint-scopes: a second ADR-0143 allow with no row of its own" ls_run "$root"
fx_git "$root" rm -q -f docs/lint-exemptions.tsv
expect red "lint-scopes: a missing table is a scope error" ls_run "$root"
fx_git "$root" reset -q --hard HEAD
rm -f "$root/crates/fake/src/extra.rs"
expect red "lint-scopes: an unresolvable base ref is a scope error" env INF_CHECK_ROOT="$root" INF_LINT_BASE_REF=no-such-ref "$LINTSCOPES"
fx_git "$root" checkout -q -b feature
fx_git "$root" branch -q -f base-tip HEAD
fx_git "$root" rm -q -f docs/lint-exemptions.tsv
expect red "lint-scopes: a table deleted from under the gate is not a bootstrap" ls_run "$root"
}

# ADR-0163 D2: the container exemption table. The fixture's compiler output
# is synthesized from its source (one `disallowed_types` span per banned
# name outside comments, in two completed builds; a line tagged
# `// uncompiled` stands for code neither build compiles), so these cases
# judge the table; the probe below proves clippy's own spans. Each red names
# its cause. LC_MAX is the fixture table's sum, and the gate copy's maximum.
LC_MAX=5
LC_A=crates/fake/src/a.rs
LC_T=docs/container-exemptions.tsv
lc_diag() {
    python3 - "$1" <<'PY'
import json, re, sys
from pathlib import Path
root = Path(sys.argv[1])
rows = []
for _ in range(2):
    for f in sorted((root / "crates").rglob("*.rs")):
        if f.name == "gated.rs":  # a module this host does not compile
            continue
        rel = f.relative_to(root).as_posix()
        for n, line in enumerate(f.read_text().split("\n"), 1):
            if "// uncompiled" in line:
                continue
            for m in re.finditer(r"\b(HashMap|HashSet|VecDeque)\b", line.split("//", 1)[0]):
                rows.append({"reason": "compiler-message", "message": {
                    "code": {"code": "clippy::disallowed_types"},
                    "message": f"use of a disallowed type `std::collections::{m.group(1)}`",
                    "spans": [{"is_primary": True, "file_name": rel, "line_start": n,
                               "column_start": m.start() + 1}]}})
    rows.append({"reason": "build-finished", "success": True})
(root / "api.json").write_text("\n".join(json.dumps(r) for r in rows) + "\n")
PY
}
lc_run() {
    lc_diag "$1"
    env INF_CHECK_ROOT="$1" INF_LINT_BASE_REF=base-tip INF_LINT_API_DIAGNOSTICS="$1/api.json" "$LINTSCOPES"
}
# lc_red_once <root> <pattern>: the gate is red and prints the pattern on
# exactly one line, though the fixture's two builds both report the site.
lc_red_once() {
    local out status=0
    out=$(lc_run "$1" 2>&1) || status=$?
    printf '%s\n' "$out"
    [ "$status" -ne 0 ] && [ "$(printf '%s\n' "$out" | grep -c -- "$2")" -eq 1 ]
}
lc_census() {
    lc_diag "$1"
    env INF_CHECK_ROOT="$1" INF_LINT_BASE_REF=base-tip INF_LINT_API_DIAGNOSTICS="$1/api.json" \
        INF_CONTAINER_CENSUS=1 "$LINTSCOPES"
}
# lc_census_is_table <root> [via-ratchet]: the census's stdout is a table
# alone — every row five columns, the rows the root's table's — and a
# census through the ratchet still prints when the ratchet's verdict is red,
# which it reports on stderr. The host is pinned as rt_run pins it: the
# planted row with no site is red only where the baseline is recorded
# (Linux); elsewhere the ratchet discloses it as not compiled.
lc_census_is_table() {
    local out="$work/census.out" err="$work/census.err"
    if [ "${2:-}" = via-ratchet ]; then
        lc_diag "$1"
        env INF_CHECK_ROOT="$1" INF_LINT_BASE_REF=base-tip INF_LINT_RATCHET_INPUT="$1/api.json" \
            INF_LINT_RATCHET_HOST=Linux INF_CONTAINER_CENSUS=1 "$LS_GATE/scripts/check-lint-ratchet.sh" \
            >"$out" 2>"$err" || return 1
        grep -q 'lint-ratchet FAILED' "$err" || { echo "the ratchet's red verdict is not on stderr"; return 1; }
    else
        lc_census "$1" >"$out" 2>"$err" || return 1
    fi
    cat "$out"
    grep -v '^#' "$out" | awk -F'\t' 'NF != 5 { print "not five columns: " $0; bad = 1 } END { exit bad }' ||
        return 1
    diff <(grep -v '^#' "$out" | LC_ALL=C sort) <(grep -v '^#' "$1/$LC_T" | LC_ALL=C sort)
}
lc_row() { printf '%s\t%s\t%s\t%s\t%s\n' "$@" >>"$root/$LC_T"; }
lc_table() {
    printf '# file\titem\tcount\trecord\texpiry\n' >"$root/$LC_T"
    lc_row "$LC_A" 'use std::collections::{…}' 2 T 2099-12-31
    lc_row "$LC_A" 'struct Exempt' 1 T 2099-12-31
    lc_row "$LC_A" 'fn Owner::build' 2 E 2099-12-31
}
lc_source() {
    printf '%s\n' \
        '#[allow(clippy::disallowed_types, reason = "container: T")]' \
        'use std::collections::{HashMap, VecDeque};' \
        '' \
        '#[allow(clippy::disallowed_types, reason = "container: T")]' \
        'pub struct Exempt {' \
        '    pub queue: VecDeque<u8>,' \
        '}' \
        '' \
        'pub struct Owner;' \
        '' \
        'impl Owner {' \
        '    #[allow(clippy::disallowed_types, reason = "container: E")]' \
        '    pub fn build() -> usize {' \
        '        let map: HashMap<u8, u8> = HashMap::new();' \
        '        map.len()' \
        '    }' \
        '}' >"$root/$LC_A"
}
lc_append() { printf '%s\n' "$@" >>"$root/$LC_A"; }
lc_append_to() { # <file> <line…>
    local file=$1
    shift
    printf '%s\n' "$@" >>"$root/$file"
}
# Fixture edits that add a line, drop a line or touch a table cell take
# exact text: BSD sed (the macOS leg) reads `\n` in a replacement and `\t`
# anywhere as the letters n and t, so lc_edit refuses both.
lc_edit() {
    [ -n "$root" ] && [ -n "$2" ] || { echo "lc_edit: empty root or file" >&2; exit 2; }
    case $1 in
    *'\n'* | *'\t'*)
        echo "lc_edit: '$1' spells \\n or \\t, which BSD sed reads as a letter: use lc_lines, lc_unallow or lc_row_set" >&2
        exit 2
        ;;
    esac
    sed -i.bak "$1" "$root/$2" && rm -f "$root/$2.bak"
}
# lc_awk <file> <program>: rewrite the fixture file through awk; the program
# exits non-zero when it matched nothing, which is a harness error.
lc_awk() {
    [ -n "$root" ] && [ -n "$1" ] || { echo "lc_awk: empty root or file" >&2; exit 2; }
    awk "$2" "$root/$1" >"$root/$1.lc" && mv "$root/$1.lc" "$root/$1" ||
        { rm -f "$root/$1.lc"; echo "lc_awk: the edit of $1 matched nothing" >&2; exit 2; }
}
# lc_lines <file> <line> <line>…: every line equal to the second argument
# becomes the rest, one per line.
lc_lines() {
    local file=$1 old=$2
    shift 2
    [ "$#" -gt 0 ] || { echo "lc_lines: no replacement lines" >&2; exit 2; }
    LC_OLD=$old LC_NEW=$(printf '%s\n' "$@") lc_awk "$file" '
        "" $0 == "" ENVIRON["LC_OLD"] { hit = 1; print ENVIRON["LC_NEW"]; next }
        { print }
        END { exit !hit }'
}
# lc_unallow <file> <item line>: the `disallowed_types` allow directly above
# each line equal to <item line> is deleted.
lc_unallow() {
    LC_ITEM=$2 lc_awk "$1" '
        NR > 1 {
            if ("" $0 == "" ENVIRON["LC_ITEM"] && index(held, "#[allow(clippy::disallowed_types") == 1) hit = 1
            else print held
        }
        { held = $0 }
        END { if (NR > 0) print held; exit !hit }'
}
# lc_row_set <file> <item|*> <column> <value>: that row of the container
# table (every row of the file for `*`) takes <value> in <column>; the
# column `drop` deletes the row.
lc_row_set() {
    case $3 in
    file | item | count | record | expiry | drop) ;;
    *) echo "lc_row_set: no column '$3'" >&2; exit 2 ;;
    esac
    LC_FILE=$1 LC_ITEM=$2 LC_COL=$3 LC_VALUE=${4:-} lc_awk "$LC_T" '
        BEGIN {
            FS = OFS = "\t"
            split("file item count record expiry", names, " ")
            for (k in names) column[names[k]] = k
        }
        $1 == ENVIRON["LC_FILE"] && (ENVIRON["LC_ITEM"] == "*" || $2 == ENVIRON["LC_ITEM"]) {
            hit = 1
            if (ENVIRON["LC_COL"] == "drop") next
            $(column[ENVIRON["LC_COL"]]) = ENVIRON["LC_VALUE"]
        }
        { print }
        END { exit !hit }'
}
# lc_reset [<ref>]: the fixture at <ref> (HEAD), its untracked files removed
lc_reset() {
    fx_git "$root" reset -q --hard "${1:-HEAD}" && fx_git "$root" clean -fdq
    for entry in "${CELL_CRATE_EXCLUDE[@]}"; do
        mkdir -p "$root/${entry%%|*}"  # git keeps no empty directory
    done
}
# lc_setup: the container fixture at its committed table, the gate at its sum.
lc_setup() {
root=$(ls_root ls-containers)
printf '%s\npub mod a;\n' "$LS_ATTR" >"$root/crates/fake/src/lib.rs"
printf '# gate\nCONTAINER_EXEMPTIONS_MAX=%s\n' "$LC_MAX" >"$root/scripts/check-lint-scopes.sh"
lc_source
lc_table
ls_commit "$root" containers
fx_git "$root" branch -q -f base-tip HEAD
ls_gate "$LC_MAX" 0
}

group_lint_scopes_containers() {
ls_setup
lc_setup
expect green "lint-scopes: containers — the table equals the census (control)" lc_run "$root"
expect_output "lint-scopes: containers — no Lossy opt-in reads 0 on the OK line" \
    "0 Lossy payload(s)" lc_run "$root"
# ADR-0151 D2: every `impl Lossy for` site of cell production code is printed
# on the OK line, so each opt-in is a reviewed line; one in a comment, one in
# a string and one in a test module are not production code.
lc_append 'pub trait Lossy {}' 'pub struct Mark;' 'impl Lossy for Mark {}' 'pub struct Pair<T>(T);' \
    'impl<T: Copy> crate::a::Lossy for Pair<T> {}' '// impl Lossy for Hidden {}' \
    'pub const TEXT: &str = "impl Lossy for Quoted {}";' '#[cfg(test)]' 'mod tests {' \
    '    pub struct TestOnly;' '    impl super::Lossy for TestOnly {}' '}'
expect_output "lint-scopes: containers — an impl Lossy for site is printed" \
    "lossy: $LC_A:20: impl Lossy for Mark" lc_run "$root"
expect_output "lint-scopes: containers — a generic, path-qualified impl Lossy for site is printed" \
    "lossy: $LC_A:22: impl Lossy for Pair<T>" lc_run "$root"
expect_output "lint-scopes: containers — the OK line counts the production opt-ins only" \
    "2 Lossy payload(s)" lc_run "$root"
lc_reset
# ADR-0171 D2: every `WrapWithProof` spelling of cell production code outside
# `bounded/` is printed on the OK line, so each wrapping slab's proof is a
# reviewed line; one in a comment, a string or a test module is not
# production code, and `bounded/`'s own definition and impls are not a use.
lc_append 'pub struct Ids { pub slab: Slab<WrapWithProof> }' 'pub fn ids() { build::<WrapWithProof>(); }' \
    '// a Slab<WrapWithProof> in a comment' 'pub const NAME: &str = "WrapWithProof";' '#[cfg(test)]' \
    'mod tests {' '    pub struct Planted(super::Slab<super::WrapWithProof>);' '}'
mkdir -p "$root/crates/inf-foundation/src/bounded"
printf '%s\n' 'pub struct WrapWithProof;' 'impl GenerationPolicy for WrapWithProof {}' \
    >"$root/crates/inf-foundation/src/bounded/slab.rs"
expect_output "lint-scopes: containers — a WrapWithProof field type is printed" \
    "wrap: $LC_A:18: pub struct Ids { pub slab: Slab<WrapWithProof> }" lc_run "$root"
expect_output "lint-scopes: containers — a WrapWithProof turbofish is printed" \
    "wrap: $LC_A:19: pub fn ids() { build::<WrapWithProof>(); }" lc_run "$root"
expect_output "lint-scopes: containers — the OK line counts the uses outside bounded/ only" \
    "2 WrapWithProof site(s)" lc_run "$root"
lc_reset
# the fixture edits' canaries: an escape BSD sed misreads is refused, and an
# edit that matches nothing is a harness error; neither changes the file
lc_escape_canary() (lc_edit 's|    pub queue: VecDeque<u8>,|    pub queue: VecDeque<u8>,\n|' "$LC_A")
expect_red_because "lint-scopes: containers — lc_edit refuses an escape BSD sed reads as a letter" \
    "which BSD sed reads as a letter" lc_escape_canary
lc_unmatched_canary() (lc_row_set "$LC_A" 'struct Absent' count 2)
expect_red_because "lint-scopes: containers — a fixture edit that matches nothing is refused" \
    "the edit of $LC_T matched nothing" lc_unmatched_canary
expect green "lint-scopes: containers — the refused edits changed nothing" fx_git "$root" diff --quiet
expect_output "lint-scopes: containers — the census prints file, item, count, record, expiry" \
    "$(printf 'fn Owner::build\t2\tE\t2099-12-31')" lc_census "$root"
expect green "lint-scopes: containers — the census's stdout is the table: its rows, five columns" \
    lc_census_is_table "$root"
expect_output "lint-scopes: containers — the census's header carries the record legend" \
    "^# Records (ADR-0163 D3): T tier write path" lc_census "$root"
printf 'fn_length\t1\t%s\n' "$LC_A" >"$root/docs/lint-baseline.tsv"
expect green "lint-scopes: containers — the census runs when the ratchet's own verdict is red" \
    lc_census_is_table "$root" via-ratchet
lc_reset
lc_edit 's|pub struct Exempt {|pub struct Kept {|' "$LC_A"
expect_output "lint-scopes: containers — a renamed item's row carries its file and record's date" \
    "$(printf 'struct Kept\t1\tT\t2099-12-31')" lc_census "$root"
lc_append '#[allow(clippy::disallowed_types, reason = "container: A")]' 'pub struct Fresh {' \
    '    pub queue: VecDeque<u8>,' '}'
expect_output "lint-scopes: containers — a row with no date to carry is printed undated" \
    "$(printf 'struct Fresh\t1\tA\tundated')" lc_census "$root"
lc_reset
: >"$root/$LC_T"
expect_red_because "lint-scopes: containers — the census refuses a table emptied by its own redirect" \
    "is empty" lc_census "$root"
lc_reset
expect_output "lint-scopes: containers — the OK line discloses the table" \
    "container exemptions: 3 row(s), counts 5/$LC_MAX, 0/0 capped-backing" lc_run "$root"
lc_lines "$LC_A" '    pub queue: VecDeque<u8>,' '    pub queue: VecDeque<u8>,' '    pub second: HashMap<u8, u8>,'
expect_red_because "lint-scopes: containers — a second HashMap field in an exempt struct" \
    "\`struct Exempt\` holds 2 container span(s), its row says 1" lc_run "$root"
lc_row_set "$LC_A" 'struct Exempt' count 2
expect_red_because "lint-scopes: containers — raising the row with it is red against HEAD" \
    "record T counts sum to 4, above the HEAD's 3" lc_run "$root"
lc_reset
lc_lines "$LC_A" '        map.len()' '        let more: VecDeque<u8> = VecDeque::new();' '        map.len()'
expect_red_because "lint-scopes: containers — a second deque local in an exempt fn" \
    "\`fn Owner::build\` holds 4 container span(s), its row says 2" lc_run "$root"
lc_reset
lc_edit 's|use std::collections::{HashMap, VecDeque};|use std::collections::{HashMap, HashSet, VecDeque};|' "$LC_A"
expect_red_because "lint-scopes: containers — a third path on an exempt use" \
    "holds 3 container span(s), its row says 2" lc_run "$root"
lc_reset
lc_edit 's|    pub queue: VecDeque<u8>,|    pub queue: (VecDeque<u8>, VecDeque<u8>),|' "$LC_A"
expect_red_because "lint-scopes: containers — a second container on a line that holds one" \
    "\`struct Exempt\` holds 2 container span(s), its row says 1" lc_run "$root"
lc_reset
lc_append '#[allow(clippy::disallowed_types, reason = "container: T")]' 'pub struct Fresh {' \
    '    pub queue: VecDeque<u8>,' '}'
expect_red_because "lint-scopes: containers — an allow with no row" \
    "allow on \`struct Fresh\` has no row in $LC_T" lc_run "$root"
lc_row "$LC_A" 'struct Fresh' 1 T 2099-12-31
expect_red_because "lint-scopes: containers — a file and record sum above an approved copy's" \
    "record T counts sum to 4, above the HEAD's 3" lc_run "$root"
lc_reset
lc_append 'pub struct Bare {' '    pub queue: VecDeque<u8>,' '}'
expect_red_because "lint-scopes: containers — a new container with no allow" \
    "std::collections::VecDeque lacks a narrow allow of its own API class" lc_run "$root"
expect green "lint-scopes: containers — a new container is reported once, with the ban's sentence" \
    lc_red_once "$root" "$LC_A:19: std::collections::VecDeque lacks a narrow allow of its own API class — cell code holds no growable std container: use"
expect green "lint-scopes: containers — the site has one violation line: the backstop does not repeat it" \
    lc_red_once "$root" "LINT-SCOPES violation: $LC_A:19: "
lc_reset
lc_append '#[cfg(feature = "absent")]' 'pub struct Off {' \
    '    pub queue: VecDeque<u8>, // uncompiled: neither build enables the feature' '}'
expect_red_because "lint-scopes: containers — a container in an item neither build compiles" \
    "$LC_A:20: \`VecDeque\` names std::collections::VecDeque outside every \`container:\` allow" \
    lc_run "$root"
lc_reset
lc_lines "$LC_A" '    pub queue: VecDeque<u8>,' '    pub queue: VecDeque<u8>,' '    #[cfg(feature = "absent")]' \
    '    pub off: HashMap<u8, u8>, // uncompiled'
expect_red_because "lint-scopes: containers — a container neither build compiles, inside an exempt item" \
    "\`struct Exempt\` names 2 container(s) in its text, 1 in the compiled spans" lc_run "$root"
lc_reset
lc_unallow "$LC_A" 'pub struct Exempt {'
expect_red_because "lint-scopes: containers — a row with no allow" \
    "row $LC_A \`struct Exempt\` has no \`container:\` allow" lc_run "$root"
lc_reset
lc_append 'pub trait Build {' '    fn build() -> usize;' '}' 'impl Build for Owner {' \
    '    #[allow(clippy::disallowed_types, reason = "container: E")]' '    fn build() -> usize {' \
    '        let set: std::collections::HashSet<u8> = Default::default();' '        set.len()' '    }' '}'
expect_red_because "lint-scopes: containers — two allows in one file with one key" \
    "2 \`container:\` allows share the item key \`fn Owner::build\`" lc_run "$root"
lc_reset
lc_append '#[allow(clippy::disallowed_types, reason = "boot: a handle taken at startup")]' \
    'pub struct Booted {' '    pub queue: VecDeque<u8>,' '}'
expect_red_because "lint-scopes: containers — a container under an allow of another class" \
    "$LC_A:20: std::collections::VecDeque lacks a narrow allow of its own API class" lc_run "$root"
lc_reset
lc_edit 's|reason = "container: E"|reason = "container: T"|' "$LC_A"
expect_red_because "lint-scopes: containers — an allow's record differs from its row's" \
    "\`container: T\` but its row says record E" lc_run "$root"
lc_reset
lc_row "$LC_A" 'struct Exempt' 1 T 2099-12-31
expect_red_because "lint-scopes: containers — a row listed twice" \
    "$LC_A \`struct Exempt\` is listed twice" lc_run "$root"
lc_reset
lc_lines "$LC_A" '        let map: HashMap<u8, u8> = HashMap::new();' '        let map: HashMap<u8, u8> = Default::default();' \
    '        #[allow(clippy::disallowed_types, reason = "container: E")]' '        fn inner() -> HashMap<u8, u8> {' \
    '            Default::default()' '        }' '        let _ = inner();'
lc_row_set "$LC_A" 'fn Owner::build' count 1
lc_row "$LC_A" 'fn Owner::inner' 1 E 2099-12-31
expect green "lint-scopes: containers — a span inside two allowed items counts on the narrower (control)" \
    lc_run "$root"
lc_edit 's|            Default::default()|            HashMap::new()|' "$LC_A"
expect_red_because "lint-scopes: containers — a second container in the inner of two allowed items" \
    "\`fn Owner::inner\` holds 2 container span(s), its row says 1" lc_run "$root"
lc_reset
lc_edit 's|reason = "container: E"|reason = "container: M"|' "$LC_A"
lc_row_set "$LC_A" 'fn Owner::build' record M
expect_red_because "lint-scopes: containers — a file and record pair the copy lacks" \
    "has record M rows and the HEAD's table has none" lc_run "$root"
lc_reset
lc_edit 's|reason = "container: E"|reason = "container: X"|' "$LC_A"
lc_row_set "$LC_A" 'fn Owner::build' record X
expect_red_because "lint-scopes: containers — an unknown record letter" \
    "record \`X\` is not a site record" lc_run "$root"
lc_reset
# An out-of-line test module (`#[cfg(test)] mod x;`) is test code whole, as an
# inline one's body is: its allow sits on the declaration (INFINITY_STYLE),
# and neither production build compiles its file (`// uncompiled`).
lc_testmod() { # <file> <tag>
    mkdir -p "$(dirname "$root/$1")"
    printf '%s\n' "use std::collections::HashMap;$2" '' 'pub fn model() -> usize {' \
        "    let m: HashMap<u8, u8> = HashMap::new();$2" '    m.len()' '}' >"$root/$1"
}
LC_TEST_DECL=('#[cfg(test)]' '#[allow(clippy::disallowed_types, reason = "test-only: a model of the queue")]')
lc_append "${LC_TEST_DECL[@]}" '#[path = "a_tests.rs"]' 'mod a_tests;'
lc_testmod crates/fake/src/a_tests.rs ' // uncompiled: test code'
expect green "lint-scopes: containers — an out-of-line test module, its allow on the declaration (control)" \
    lc_run "$root"
expect_output "lint-scopes: containers — the OK line names the test module file it leaves out" \
    "out-of-line test module file(s) outside the API audit and the backstop: crates/fake/src/a_tests.rs" \
    lc_run "$root"
lc_reset
lc_append "${LC_TEST_DECL[@]}" 'mod tests;'
lc_testmod crates/fake/src/a/tests.rs ' // uncompiled: test code'
expect green "lint-scopes: containers — one with no #[path], under the declaring file's stem (control)" \
    lc_run "$root"
lc_reset
lc_append "${LC_TEST_DECL[@]:1}" '#[path = "a_tests.rs"]' 'mod a_tests;'
lc_testmod crates/fake/src/a_tests.rs ''
expect_red_because "lint-scopes: containers — the same module without #[cfg(test)] is production code" \
    "crates/fake/src/a_tests.rs:1: std::collections::HashMap lacks a narrow allow of its own API class" \
    lc_run "$root"
lc_reset
lc_append "${LC_TEST_DECL[@]}" 'mod gone;'
expect_red_because "lint-scopes: containers — a test module whose file the gate cannot find" \
    "test-only \`mod gone;\` names no file the audit can find" lc_run "$root"
lc_reset
lc_append_to crates/fake/src/lib.rs "${LC_TEST_DECL[@]}" 'mod lib_tests;'
lc_testmod crates/fake/src/lib_tests.rs ' // uncompiled: test code'
expect green "lint-scopes: containers — one declared in the crate root, its file beside it (control)" \
    lc_run "$root"
lc_reset
lc_append "${LC_TEST_DECL[@]}" 'mod dir_tests;'
lc_testmod crates/fake/src/a/dir_tests/mod.rs ' // uncompiled: test code'
expect green "lint-scopes: containers — one whose file is the module's mod.rs (control)" lc_run "$root"
lc_reset
# The mock swap: the production twin of a test-only module keeps its file,
# held by the backstop where no build compiles it.
lc_append '#[cfg(not(test))]' 'mod swap_real;' "${LC_TEST_DECL[@]}" '#[path = "swap_fake.rs"]' 'mod swap_real;'
lc_testmod crates/fake/src/a/swap_real.rs ' // uncompiled: a feature neither build enables'
lc_testmod crates/fake/src/swap_fake.rs ' // uncompiled: test code'
expect_red_because "lint-scopes: containers — a mock swap's production twin is production code" \
    "crates/fake/src/a/swap_real.rs:1: \`HashMap\` names std::collections::HashMap outside every \`container:\` allow" \
    lc_run "$root"
lc_reset
# rustc reads `a/inner/probe.rs` here; a resolver that drops the inline
# module's name takes production `a/probe.rs` for test code, and the backstop
# then skips its code no build compiles (so do the two aliases below).
lc_append 'pub mod probe;' 'mod inner {' '    #[cfg(test)]' '    mod probe;' '}'
lc_testmod crates/fake/src/a/probe.rs ' // uncompiled: a feature neither build enables'
mkdir -p "$root/crates/fake/src/a/inner"
printf 'pub fn probe() {}\n' >"$root/crates/fake/src/a/inner/probe.rs"
expect_red_because "lint-scopes: containers — a test module declared inside an inline module" \
    "$LC_A:21: \`mod probe;\` inside an inline module or a block" lc_run "$root"
lc_reset
lc_append '#[cfg(test)]' 'mod tests {' '    mod model;' '}'
expect_red_because "lint-scopes: containers — an out-of-line child of an inline test module" \
    "$LC_A:20: \`mod model;\` inside an inline module or a block" lc_run "$root"
lc_reset
# The depth is the code's: a brace in a comment or a char literal is not one.
lc_append 'mod inner {' '    // a comment that ends in a brace: }' '    #[cfg(test)]' '    mod probe;' '}'
lc_testmod crates/fake/src/a/probe.rs ' // uncompiled: test code'
expect_red_because "lint-scopes: containers — a test module inside an inline module, after a comment's brace" \
    "$LC_A:21: \`mod probe;\` inside an inline module or a block" lc_run "$root"
lc_reset
lc_append "const OPEN: char = '{';" "${LC_TEST_DECL[@]}" 'mod tests;'
lc_testmod crates/fake/src/a/tests.rs ' // uncompiled: test code'
expect green "lint-scopes: containers — a '{' char literal above a file-level test module (control)" \
    lc_run "$root"
lc_reset
# The stripper reports no `modfile` for this visibility, so the declaration
# is production code and its file is held by the backstop.
lc_append '#[cfg(test)]' 'pub(in crate::a) mod t;'
lc_testmod crates/fake/src/a/t.rs ' // uncompiled: test code'
expect_red_because "lint-scopes: containers — a test-only declaration the stripper does not report" \
    "crates/fake/src/a/t.rs:1: \`HashMap\` names std::collections::HashMap outside every \`container:\` allow" \
    lc_run "$root"
lc_reset
lc_append_to crates/fake/src/lib.rs 'pub mod b;' "${LC_TEST_DECL[@]}" '#[path = "b.rs"]' 'mod b_again;'
lc_testmod crates/fake/src/b.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a test module whose #[path] names a production module's file" \
    "test-only \`mod b_again;\` names crates/fake/src/b.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append "${LC_TEST_DECL[@]}" '#[path = "lib.rs"]' 'mod root_again;'
expect_red_because "lint-scopes: containers — a test module whose #[path] names the crate root" \
    "test-only \`mod root_again;\` names crates/fake/src/lib.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
# The resolver reads each line's code, comments removed, so a comment does
# not hide a production declaration or its #[path]; what it cannot read is
# red, and no exclusion trusts it.
lc_append '#[cfg(not(test))]' 'mod probe; // the real one' "${LC_TEST_DECL[@]}" 'mod probe;'
lc_testmod crates/fake/src/a/probe.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a mock swap whose production twin carries a comment" \
    "test-only \`mod probe;\` names crates/fake/src/a/probe.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append 'pub mod probe; // the production module'
lc_append_to crates/fake/src/lib.rs "${LC_TEST_DECL[@]}" '#[path = "a/probe.rs"]' 'mod probe_again;'
lc_testmod crates/fake/src/a/probe.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a #[path] alias of a production module whose declaration carries a comment" \
    "test-only \`mod probe_again;\` names crates/fake/src/a/probe.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
LC_ALIAS=("${LC_TEST_DECL[@]}" '#[path = "a/real.rs"]' 'mod real_again;')
lc_append '#[path = "a/real.rs"] // the production file' 'pub mod live;' "${LC_ALIAS[@]}"
lc_testmod crates/fake/src/a/real.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a production #[path] that carries a comment" \
    "test-only \`mod real_again;\` names crates/fake/src/a/real.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append '#[path = "a/real.rs"]' '// a comment line between the attribute and its item' 'pub mod live;' \
    "${LC_ALIAS[@]}"
lc_testmod crates/fake/src/a/real.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a production #[path] above a comment line" \
    "test-only \`mod real_again;\` names crates/fake/src/a/real.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append '#[path = "a/real.rs"]' '#[path = "a/other.rs"]' 'pub mod live;' "${LC_ALIAS[@]}"
lc_testmod crates/fake/src/a/real.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a production declaration with two #[path]s, of which rustc reads the first" \
    "test-only \`mod real_again;\` names crates/fake/src/a/real.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append 'pub mod r#probe;' "${LC_TEST_DECL[@]}" '#[path = "a/probe.rs"]' 'mod probe_again;'
lc_testmod crates/fake/src/a/probe.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a production declaration by a raw identifier" \
    "test-only \`mod probe_again;\` names crates/fake/src/a/probe.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append "${LC_TEST_DECL[@]}" '#[path = "swap_fake.rs"]' 'mod probe;' '#[cfg(not(test))]' 'mod probe;'
lc_testmod crates/fake/src/swap_fake.rs ' // uncompiled: test code'
mkdir -p "$root/crates/fake/src/a"
printf 'pub fn probe() {}\n' >"$root/crates/fake/src/a/probe.rs"
expect green "lint-scopes: containers — a mock swap whose production twin follows the test-only one (control)" \
    lc_run "$root"
lc_reset
lc_append '#[doc = "its path = the default"]' 'pub mod probe;'
mkdir -p "$root/crates/fake/src/a"
printf 'pub fn probe() {}\n' >"$root/crates/fake/src/a/probe.rs"
expect green "lint-scopes: containers — \`path =\` inside a string of a declaration's attribute (control)" \
    lc_run "$root"
lc_reset
lc_append 'include!("a/real.rs");' "${LC_ALIAS[@]}"
lc_testmod crates/fake/src/a/real.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a test module aliasing a file production code includes" \
    "test-only \`mod real_again;\` names crates/fake/src/a/real.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append 'include!["a/real.rs"];' "${LC_ALIAS[@]}"
lc_testmod crates/fake/src/a/real.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a test module aliasing a file production code includes in brackets" \
    "test-only \`mod real_again;\` names crates/fake/src/a/real.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append 'include! { "a/real.rs" }' "${LC_ALIAS[@]}"
lc_testmod crates/fake/src/a/real.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a test module aliasing a file production code includes in braces" \
    "test-only \`mod real_again;\` names crates/fake/src/a/real.rs, which production code also compiles" \
    lc_run "$root"
lc_reset
lc_append 'include!(concat!("a/", "real.rs"));'
expect_red_because "lint-scopes: containers — an include! whose file is an expression" \
    "$LC_A:18: \`include!\` names its file by an expression the audit does not read" lc_run "$root"
lc_reset
lc_append '#[cfg(not(test))] mod probe;' "${LC_TEST_DECL[@]}" 'mod probe;'
lc_testmod crates/fake/src/a/probe.rs ' // uncompiled: a feature neither build enables'
expect_red_because "lint-scopes: containers — a declaration not on a line of its own" \
    "$LC_A:18: \`mod probe;\` is not on a line of its own" lc_run "$root"
lc_reset
lc_append '#[cfg_attr(not(test), path = "a/real.rs")]' 'mod live;'
expect_red_because "lint-scopes: containers — a declaration whose path is a cfg_attr" \
    "$LC_A:19: \`mod live;\` names its file through an attribute the audit does not read" lc_run "$root"
lc_reset
lc_append '#[cfg_attr(' '    not(test),' '    path = "a/real.rs"' ')]' 'mod live;'
expect_red_because "lint-scopes: containers — a declaration whose path is a cfg_attr over several lines" \
    "$LC_A:22: \`mod live;\` names its file through an attribute the audit does not read" lc_run "$root"
lc_reset
lc_row_set "$LC_A" 'struct Exempt' expiry 2000-01-01
expect_red_because "lint-scopes: containers — an expired row" \
    "\`struct Exempt\` expired on 2000-01-01" lc_run "$root"
lc_reset
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-01-01
expect_red_because "lint-scopes: containers — a date moved later" \
    "a date only moves earlier" lc_run "$root"
lc_reset
lc_row_set "$LC_A" 'struct Exempt' expiry 2099-01-01
expect green "lint-scopes: containers — a date moved earlier (control)" lc_run "$root"
lc_reset
}

group_lint_scopes_moves() {
ls_setup
lc_setup
# ADR-0201 D1: a row moves later only up to its record's date on the gate's
# CONTAINER_EXPIRY_MOVES line, or on its CONTAINER_EXPIRY_MOVES_2 line where
# the record has one (a ceiling). The second line is keyed to the first: each
# entry raises a first-line record past its date there. Each line's own dates
# only move earlier, and a record joins one only by the ADR that changes the
# rule, against HEAD, the base tip and the commit that introduced the line; a
# third move line is red. lc_moves and lc_moves_2 edit the working gate's
# lines, as ls_gate edits its maximum; the fixture's rows sit at 2099-12-31
# (struct Exempt is T, fn Owner::build E).
lc_moves() { # <entries>
    sed -i.bak "s/^CONTAINER_EXPIRY_MOVES=\".*\"\$/CONTAINER_EXPIRY_MOVES=\"$1\"/" "$LINTSCOPES"
    rm -f "$LINTSCOPES.bak"
    grep -q "^CONTAINER_EXPIRY_MOVES=\"$1\"\$" "$LINTSCOPES" ||
        { echo "lc_moves: the gate's move line is not where the copy sets it" >&2; exit 2; }
}
lc_moves_2() { # <entries>
    sed -i.bak "s/^CONTAINER_EXPIRY_MOVES_2=\".*\"\$/CONTAINER_EXPIRY_MOVES_2=\"$1\"/" "$LINTSCOPES"
    rm -f "$LINTSCOPES.bak"
    grep -q "^CONTAINER_EXPIRY_MOVES_2=\"$1\"\$" "$LINTSCOPES" ||
        { echo "lc_moves_2: the gate's second move line is not where the copy sets it" >&2; exit 2; }
}
# lc_stub <first> <second> [<name>]: the fixture's committed gate copy, its
# maximum and each move line given (`-` leaves that line out), and with
# <name> a comment that spells that line's search string once more.
lc_stub() {
    {
        printf '# gate\nCONTAINER_EXEMPTIONS_MAX=%s\n' "$LC_MAX"
        [ "$1" = - ] || printf 'CONTAINER_EXPIRY_MOVES="%s"\n' "$1"
        [ "$2" = - ] || printf 'CONTAINER_EXPIRY_MOVES_2="%s"\n' "$2"
        [ -z "${3:-}" ] || printf '# the line stays spelled %s="<record>=<date> …" alone\n' "$3"
    } >"$root/scripts/check-lint-scopes.sh"
}
# lc_intro_leg <first> <second> <later first> <later second> <name>: a stub
# commit that introduces the lines, then a later one that changes them and
# spells <name>'s search string again in a comment, so `git log -S` lists
# both; the base tip moves to the later commit. HEAD and the base tip then
# agree with a working gate that spells the later lines, so only the line's
# introducing commit can be red: a gate that reads the newest commit the
# search lists, misspells the search string or drops the third copy turns
# the plant green. lc_intro_undo drops both commits and puts the base tip back.
lc_intro_leg() {
    lc_stub "$1" "$2"
    ls_commit "$root" "the move lines' introducing commit"
    lc_stub "$3" "$4" "$5"
    ls_commit "$root" "the move lines later, a search string in a comment"
    fx_git "$root" branch -q -f base-tip HEAD
}
lc_intro_undo() {
    lc_reset HEAD~2
    fx_git "$root" branch -q -f base-tip HEAD
}
LC_MOVES=$(sed -n 's/^CONTAINER_EXPIRY_MOVES="\(.*\)"$/\1/p' "$LINTSCOPES")
[ -n "$LC_MOVES" ] || { echo "the gate spells no CONTAINER_EXPIRY_MOVES line" >&2; exit 2; }
grep -q '^CONTAINER_EXPIRY_MOVES_2="[^"]*"$' "$LINTSCOPES" ||
    { echo "the gate spells no CONTAINER_EXPIRY_MOVES_2 line" >&2; exit 2; }
LC_MOVES_2=$(sed -n 's/^CONTAINER_EXPIRY_MOVES_2="\(.*\)"$/\1/p' "$LINTSCOPES")
# The first line's twelve cases run on an empty second line, its legal end
# state: each sets the first line, and the keyed rule would read the second
# line's records as records that line lacks, so a planted red would have two
# causes. The line is restored after them.
lc_moves_2 ""
sed -i.bak 's/^\(CONTAINER_EXPIRY_MOVES=".*"\)$/\1 # note/' "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
expect_red_because "lint-scopes: containers — a move line spelled so no copy can read it" \
    "the gate spells no CONTAINER_EXPIRY_MOVES line" lc_run "$root"
sed -i.bak 's/^\(CONTAINER_EXPIRY_MOVES=".*"\) # note$/\1/' "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
lc_moves "T=2100-06-30"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-06-30
expect green "lint-scopes: containers — a date moved later onto its record's move date (control)" \
    lc_run "$root"
expect_output "lint-scopes: containers — the OK line discloses the move dates" \
    "expiry moves T=2100-06-30" lc_run "$root"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-03-01
expect green "lint-scopes: containers — a date moved later, under its record's move date (control)" \
    lc_run "$root"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-07-01
expect_red_because "lint-scopes: containers — a date moved past its record's move date" \
    "a date only moves earlier, or up to T's 2100-06-30" lc_run "$root"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-06-30
lc_row_set "$LC_A" 'fn Owner::build' expiry 2100-06-30
expect_red_because "lint-scopes: containers — a date moved onto another record's move date" \
    "record E has no CONTAINER_EXPIRY_MOVES date" lc_run "$root"
lc_reset
printf '# gate\nCONTAINER_EXEMPTIONS_MAX=%s\nCONTAINER_EXPIRY_MOVES="T=2100-05-31"\n' "$LC_MAX" \
    >"$root/scripts/check-lint-scopes.sh"
ls_commit "$root" "a move date"
lc_moves "T=2100-06-30"
expect_red_because "lint-scopes: containers — a move date later than the HEAD's" \
    "a move date only moves earlier" lc_run "$root"
lc_moves "T=2100-05-31 E=2100-05-31"
expect_red_because "lint-scopes: containers — a record added to the line" \
    "E=2100-05-31 is absent from the HEAD's line" lc_run "$root"
lc_moves "T=2100-05-31"
expect green "lint-scopes: containers — a move date equal to the HEAD's (control)" lc_run "$root"
lc_moves "T=2100-05-30"
expect green "lint-scopes: containers — a move date moved earlier (control)" lc_run "$root"
lc_reset HEAD~1
# The first line's introducing-commit leg, which the cases above plant only
# at HEAD (the stub that introduces the line is HEAD there).
lc_moves "T=2100-06-30"
lc_intro_leg "T=2100-05-31" - "T=2100-06-30" - CONTAINER_EXPIRY_MOVES
expect_red_because "lint-scopes: containers — a move date later than its introducing commit's, HEAD and the base tip agreeing" \
    "CONTAINER_EXPIRY_MOVES: T=2100-06-30 is later than the line's introducing commit's 2100-05-31" lc_run "$root"
lc_intro_undo
lc_moves "T=2100-05-31 E=2100-05-31"
lc_intro_leg "T=2100-05-31" - "T=2100-05-31 E=2100-05-31" - CONTAINER_EXPIRY_MOVES
expect_red_because "lint-scopes: containers — a record on the move line in HEAD, the base tip and the gate, absent from its introducing commit" \
    "CONTAINER_EXPIRY_MOVES: E=2100-05-31 is absent from the line's introducing commit's line" lc_run "$root"
lc_intro_undo
# The second line, keyed to a first line that moves T and E to 2100-05-31.
lc_moves "T=2100-05-31 E=2100-05-31"
lc_moves_2 "T=2100-06-30"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-06-30
expect green "lint-scopes: containers — a date moved later onto its record's second-line date (control)" \
    lc_run "$root"
lc_moves_2 "T=2100-06-30 E=2100-06-15"
expect_output "lint-scopes: containers — the OK line discloses the second line after the first, records sorted" \
    "expiry moves E=2100-05-31, T=2100-05-31; second move E=2100-06-15, T=2100-06-30" lc_run "$root"
lc_moves_2 "T=2100-06-30"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-06-15
expect green "lint-scopes: containers — a date between its record's two move dates (control)" lc_run "$root"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-07-01
expect_red_because "lint-scopes: containers — a date moved past its record's second-line date" \
    "or up to T's 2100-06-30 (CONTAINER_EXPIRY_MOVES_2, ADR-0201 D1)" lc_run "$root"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-06-30
lc_row_set "$LC_A" 'fn Owner::build' expiry 2100-06-30
expect_red_because "lint-scopes: containers — a record with no second-line date, moved onto another record's" \
    "or up to E's 2100-05-31 (CONTAINER_EXPIRY_MOVES, ADR-0201 D1)" lc_run "$root"
lc_reset
lc_moves "T=2100-05-31"
lc_moves_2 "T=2100-06-30 E=2100-06-30"
expect_red_because "lint-scopes: containers — a second-line entry for a record the first line lacks" \
    "CONTAINER_EXPIRY_MOVES_2: E=2100-06-30 names a record the CONTAINER_EXPIRY_MOVES line in use lacks" \
    lc_run "$root"
lc_moves "T=2100-05-31 E=2100-05-31"
lc_moves_2 "T=2100-05-31"
expect_red_because "lint-scopes: containers — a second-line entry at its first-line date" \
    "CONTAINER_EXPIRY_MOVES_2: T=2100-05-31 is not after its CONTAINER_EXPIRY_MOVES date 2100-05-31" lc_run "$root"
lc_moves_2 "T=2100-06-30"
sed -i.bak 's/^\(CONTAINER_EXPIRY_MOVES_2=".*"\)$/\1 # note/' "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
expect_red_because "lint-scopes: containers — a second move line spelled so no copy can read it" \
    "the gate spells no CONTAINER_EXPIRY_MOVES_2 line" lc_run "$root"
sed -i.bak 's/^\(CONTAINER_EXPIRY_MOVES_2=".*"\) # note$/\1/' "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
printf 'CONTAINER_EXPIRY_MOVES_3="T=2100-07-31"\n' >>"$LINTSCOPES"
expect_red_because "lint-scopes: containers — a third move line" \
    "a third expiry move is the ADR that supersedes ADR-0201" lc_run "$root"
sed -i.bak '$d' "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
! grep -q '^CONTAINER_EXPIRY_MOVES_3=' "$LINTSCOPES" ||
    { echo "the third move line plant is still in the gate copy" >&2; exit 2; }
# HEAD's gate spells both lines and its table T's row at its second-line date.
lc_stub "T=2100-05-31 E=2100-05-31" "T=2100-06-30"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-06-30
ls_commit "$root" "a second move date"
expect green "lint-scopes: containers — a second-line date equal to the HEAD's (control)" lc_run "$root"
lc_moves_2 "T=2100-07-31"
expect_red_because "lint-scopes: containers — a second-line date later than the HEAD's" \
    "CONTAINER_EXPIRY_MOVES_2: T=2100-07-31 is later than the HEAD's 2100-06-30" lc_run "$root"
lc_moves_2 "T=2100-06-30 E=2100-06-30"
expect_red_because "lint-scopes: containers — a record added to the HEAD's second line" \
    "CONTAINER_EXPIRY_MOVES_2: E=2100-06-30 is absent from the HEAD's line" lc_run "$root"
lc_moves_2 "T=2100-06-15"
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-06-15
expect green "lint-scopes: containers — a second-line date moved earlier, still after its first (control)" \
    lc_run "$root"
lc_moves_2 ""
lc_row_set "$LC_A" 'struct Exempt' expiry 2100-05-31
expect green "lint-scopes: containers — a record leaves the second line, its row back at its first date (control)" \
    lc_run "$root"
lc_reset HEAD~1
# The second line's introducing-commit leg.
lc_moves "T=2100-05-31"
lc_moves_2 "T=2100-06-30"
lc_intro_leg "T=2100-05-31" "T=2100-06-15" "T=2100-05-31" "T=2100-06-30" CONTAINER_EXPIRY_MOVES_2
expect_red_because "lint-scopes: containers — a second-line date later than its introducing commit's, HEAD and the base tip agreeing" \
    "CONTAINER_EXPIRY_MOVES_2: T=2100-06-30 is later than the line's introducing commit's 2100-06-15" lc_run "$root"
lc_intro_undo
lc_moves "T=2100-05-31 E=2100-05-31"
lc_moves_2 "T=2100-06-30 E=2100-06-30"
lc_intro_leg "T=2100-05-31 E=2100-05-31" "T=2100-06-30" "T=2100-05-31 E=2100-05-31" "T=2100-06-30 E=2100-06-30" \
    CONTAINER_EXPIRY_MOVES_2
expect_red_because "lint-scopes: containers — a record on the second line in HEAD, the base tip and the gate, absent from its introducing commit" \
    "CONTAINER_EXPIRY_MOVES_2: E=2100-06-30 is absent from the line's introducing commit's line" lc_run "$root"
lc_intro_undo
lc_moves "$LC_MOVES"
lc_moves_2 "$LC_MOVES_2"
lc_reset
printf '# gate\nCONTAINER_EXEMPTIONS_MAX=%s\n' "$((LC_MAX - 1))" >"$root/scripts/check-lint-scopes.sh"
ls_commit "$root" "a lower maximum"
expect_red_because "lint-scopes: containers — a raised maximum" \
    "CONTAINER_EXEMPTIONS_MAX = $LC_MAX is above the HEAD's $((LC_MAX - 1))" lc_run "$root"
lc_reset HEAD~1
sed -i.bak "s/^CONTAINER_EXEMPTIONS_MAX=$LC_MAX\$/CONTAINER_EXEMPTIONS_MAX=$LC_MAX # the sum/" "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
expect_red_because "lint-scopes: containers — a maximum spelled so no approved copy can read it" \
    "the gate spells CONTAINER_EXEMPTIONS_MAX" lc_run "$root"
sed -i.bak "s/^CONTAINER_EXEMPTIONS_MAX=$LC_MAX # the sum\$/CONTAINER_EXEMPTIONS_MAX=$LC_MAX/" "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
printf 'CONTAINER_EXEMPTIONS_MAX=%s\n' "$LC_MAX" >>"$LINTSCOPES"
expect_red_because "lint-scopes: containers — the maximum spelled on two lines" \
    "the gate spells CONTAINER_EXEMPTIONS_MAX" lc_run "$root"
sed -i.bak '$d' "$LINTSCOPES"
rm -f "$LINTSCOPES.bak"
ls_gate "$LC_MAX" 0
expect green "lint-scopes: containers — the gate copy restored (control)" lc_run "$root"
lc_lines "$LC_A" '    pub queue: VecDeque<u8>,' '    pub queue: VecDeque<u8>,' '    pub second: HashMap<u8, u8>,'
lc_row_set "$LC_A" 'struct Exempt' count 2
ls_commit "$root" "a raise, committed"
expect_red_because "lint-scopes: containers — a committed raise is red against the base tip" \
    "record T counts sum to 4, above the base tip's 3" lc_run "$root"
lc_reset HEAD~1
lc_append '#[allow(clippy::disallowed_types, reason = "container: T")]' 'pub mod inner {' \
    '    pub struct Q(pub std::collections::VecDeque<u8>);' '}'
expect_red_because "lint-scopes: containers — an allow on a mod" \
    "disallowed_types needs a narrow item, never a module, impl, trait or alias" lc_run "$root"
lc_reset
lc_append 'pub struct S;' '#[allow(clippy::disallowed_types, reason = "container: T")]' 'impl S {' \
    '    pub fn q() -> std::collections::VecDeque<u8> { Default::default() }' '}'
expect_red_because "lint-scopes: containers — an allow on an impl" \
    "disallowed_types needs a narrow item, never a module, impl, trait or alias" lc_run "$root"
lc_reset
lc_append '#[allow(clippy::disallowed_types, reason = "container: T")]' 'pub type Q = VecDeque<u8>;'
expect_red_because "lint-scopes: containers — an alias under an allow" \
    "disallowed_types needs a narrow item, never a module, impl, trait or alias" lc_run "$root"
lc_reset
lc_append '#[allow(clippy::disallowed_types, reason = "container: capped-backing")]' \
    'pub struct CappedDeque<T> {' '    inner: VecDeque<T>,' '}'
expect_red_because "lint-scopes: containers — a capped-backing allow past CAPPED_BACKING_SITES" \
    "1 \`container: capped-backing\` allow(s)" lc_run "$root"
lc_reset
ls_gate "$LC_MAX" 1
LC_B=crates/inf-foundation/src/bounded/deque.rs
mkdir -p "$root/crates/inf-foundation/src/bounded"
lc_append_to "$LC_B" '#[allow(clippy::disallowed_types, reason = "container: capped-backing")]' \
    'pub struct CappedDeque<T> {' '    inner: std::collections::VecDeque<T>,' '}'
expect green "lint-scopes: containers — the one backing, on struct CappedDeque in bounded/ (control, the gate at 1)" \
    lc_run "$root"
lc_lines "$LC_B" '    inner: std::collections::VecDeque<T>,' '    inner: std::collections::VecDeque<T>,' \
    '    spare: std::collections::VecDeque<T>,'
expect_red_because "lint-scopes: containers — a second container inside the backing" \
    "the backing \`struct CappedDeque\` holds 2 container span(s)" lc_run "$root"
lc_reset
lc_append '#[allow(clippy::disallowed_types, reason = "container: capped-backing")]' 'pub struct Holder {' \
    '    pub queue: VecDeque<u8>,' '}'
expect_red_because "lint-scopes: containers — the backing allow on another item" \
    "\`container: capped-backing\` on \`struct Holder\` in $LC_A" lc_run "$root"
lc_reset
# the backing's place has two halves, each with its own plant: the item and
# the directory; and the one backing holds exactly one container
lc_append '#[allow(clippy::disallowed_types, reason = "container: capped-backing")]' \
    'pub struct CappedDeque<T> {' '    inner: VecDeque<T>,' '}'
expect_red_because "lint-scopes: containers — the backing's item outside bounded/" \
    "\`container: capped-backing\` on \`struct CappedDeque\` in $LC_A" lc_run "$root"
lc_reset
mkdir -p "$root/crates/inf-foundation/src/bounded"
lc_append_to "$LC_B" '#[allow(clippy::disallowed_types, reason = "container: capped-backing")]' \
    'pub struct Spare<T> {' '    inner: std::collections::VecDeque<T>,' '}'
expect_red_because "lint-scopes: containers — another item in bounded/ under the backing's allow" \
    "\`container: capped-backing\` on \`struct Spare\` in $LC_B" lc_run "$root"
lc_reset
mkdir -p "$root/crates/inf-foundation/src/bounded"
lc_append_to "$LC_B" '#[allow(clippy::disallowed_types, reason = "container: capped-backing")]' \
    'pub struct CappedDeque<T> {' '    inner: Vec<T>,' '}'
expect_red_because "lint-scopes: containers — the backing holds no container" \
    "the backing \`struct CappedDeque\` holds 0 container span(s)" lc_run "$root"
lc_reset
ls_gate "$LC_MAX" 0
# ADR-0144 A2: a public type of bounded/ with a lifetime parameter borrows a
# capped container, so it is an entry of `await-holding-invalid-types`; one
# that is not is red; a crate-private one, a public one without a lifetime,
# and a listed one are green.
mkdir -p "$root/crates/inf-foundation/src/bounded"
lc_append_to "$LC_B" "pub struct Peek<'a>(&'a u8);"
expect_red_because "lint-scopes: containers — a public lifetime type of bounded/ outside the await ban" \
    "\`Peek\` has a lifetime parameter" lc_run "$root"
lc_reset
mkdir -p "$root/crates/inf-foundation/src/bounded"
lc_append_to "$LC_B" "pub(super) struct Peek<'a>(&'a u8);" 'pub struct Plain<T>(T);'
expect green "lint-scopes: containers — a crate-private lifetime type and a public one without (control)" \
    lc_run "$root"
lc_reset
mkdir -p "$root/crates/inf-foundation/src/bounded"
lc_append_to "$LC_B" "pub struct Peek<'a>(&'a u8);"
lc_edit 's|^await-holding-invalid-types = \[$|await-holding-invalid-types = [ { path = "inf_foundation::bounded::Peek", reason = "fixture" },|' clippy.toml
expect_output "lint-scopes: containers — a listed lifetime type of bounded/ is counted (control)" \
    "1 reservation type(s) under the await ban" lc_run "$root"
lc_reset
lc_append 'pub const CAP: u32 = Cap::entries("x", 1);'
expect_red_because "lint-scopes: containers — Cap::entries outside a limits.rs" \
    "\`Cap::entries\` outside a \`limits.rs\`" lc_run "$root"
lc_reset
printf '%s\n' 'pub const CAP: u32 = Cap::entries("x", 1);' >"$root/crates/fake/src/limits.rs"
expect green "lint-scopes: containers — Cap::entries in a limits.rs (control)" lc_run "$root"
lc_reset
lc_edit 's|        let map: HashMap<u8, u8> = HashMap::new();|        let map: HashMap<u8, u8> = Default::default();|' "$LC_A"
expect_red_because "lint-scopes: containers — fewer than the row" \
    "\`fn Owner::build\` holds 1 container span(s), its row says 2 — lower the row to 1" lc_run "$root"
lc_row_set "$LC_A" 'fn Owner::build' count 1
expect_red_because "lint-scopes: containers — a lowered count with the maximum left above the sum" \
    "counts sum to 4, below CONTAINER_EXEMPTIONS_MAX = $LC_MAX — lower the maximum to 4" lc_run "$root"
ls_gate $((LC_MAX - 1)) 0
expect green "lint-scopes: containers — a lowered count and maximum (control)" lc_run "$root"
ls_gate "$LC_MAX" 0
lc_reset
lc_edit 's|    pub queue: VecDeque<u8>,|    pub queue: Vec<u8>,|' "$LC_A"
expect_red_because "lint-scopes: containers — an allow that covers no container" \
    "the allow on \`struct Exempt\` covers no container" lc_run "$root"
lc_unallow "$LC_A" 'pub struct Exempt {'
lc_row_set "$LC_A" 'struct Exempt' drop
ls_gate $((LC_MAX - 1)) 0
expect green "lint-scopes: containers — a removed row and a lowered maximum (control)" lc_run "$root"
ls_gate "$LC_MAX" 0
lc_reset
fx_git "$root" mv "$LC_A" crates/fake/src/moved.rs
lc_row_set "$LC_A" '*' file crates/fake/src/moved.rs
lc_edit 's|^pub mod a;|pub mod moved;|' crates/fake/src/lib.rs
fx_git "$root" add -A
expect green "lint-scopes: containers — a renamed file keeps its rows (control)" lc_run "$root"
lc_reset
lc_edit 's|pub struct Exempt {|pub struct Kept {|' "$LC_A"
expect_red_because "lint-scopes: containers — a renamed item whose row kept the old key names the fix" \
    "rename the row's item to \`struct Kept\`" lc_run "$root"
lc_row_set "$LC_A" 'struct Exempt' item 'struct Kept'
expect green "lint-scopes: containers — a renamed item (control)" lc_run "$root"
lc_reset
mv "$root/$LC_A" "$root/crates/fake/src/moved.rs"
lc_row_set "$LC_A" '*' file crates/fake/src/moved.rs
lc_edit 's|^pub mod a;|pub mod moved;|' crates/fake/src/lib.rs
expect_red_because "lint-scopes: containers — a renamed file not yet staged says to stage it" \
    "keeps its rows once the rename is staged" lc_run "$root"
lc_reset
lc_edit 's|        let map: HashMap<u8, u8> = HashMap::new();|        let map: HashMap<u8, u8> = Default::default();|' "$LC_A"
lc_append 'impl Exempt {' '    #[allow(clippy::disallowed_types, reason = "container: E")]' \
    '    pub fn fresh() -> HashMap<u8, u8> {' '        Default::default()' '    }' '}'
lc_row_set "$LC_A" 'fn Owner::build' count 1
lc_row "$LC_A" 'fn Exempt::fresh' 1 E 2099-12-31
expect green "lint-scopes: containers — a split item, the same file and record sum (control)" lc_run "$root"
lc_reset
mkdir -p "$root/crates/fake/src/two"
printf '%s\n' '#[allow(clippy::disallowed_types, reason = "container: T")]' 'pub struct Moved {' \
    '    pub queue: VecDeque<u8>,' '}' >"$root/crates/fake/src/two/other.rs"
lc_unallow "$LC_A" 'pub struct Exempt {'
lc_edit 's|    pub queue: VecDeque<u8>,|    pub queue: Vec<u8>,|' "$LC_A"
lc_row_set "$LC_A" 'struct Exempt' drop
lc_row crates/fake/src/two/other.rs 'struct Moved' 1 T 2099-12-31
expect_red_because "lint-scopes: containers — an exempt item moved to another file" \
    "crates/fake/src/two/other.rs has record T rows and the HEAD's table has none" lc_run "$root"
lc_reset
LC_G=crates/fake/src/gated.rs
printf '%s\n' '#[cfg(target_os = "windows")]' 'mod gated;' >>"$root/crates/fake/src/lib.rs"
printf '%s\n' '#[allow(clippy::disallowed_types, reason = "container: D")]' \
    'use std::collections::HashMap as Map;' '' \
    '#[allow(clippy::disallowed_types, reason = "container: D")]' 'pub struct Gated {' \
    '    pub map: Map<u8, u8>,' '}' >"$root/$LC_G"
lc_gated_rows() {
    lc_row "$LC_G" 'struct Gated' 1 D 2099-12-31
    lc_row "$LC_G" 'use std::collections::HashMap as Map' 1 D 2099-12-31
}
lc_gated_rows
printf '# gate\nCONTAINER_EXEMPTIONS_MAX=%s\n' "$((LC_MAX + 2))" >"$root/scripts/check-lint-scopes.sh"
ls_commit "$root" "a host-gated module"
fx_git "$root" branch -q -f base-tip HEAD
ls_gate $((LC_MAX + 2)) 0
expect green "lint-scopes: containers — a host-gated module, a same-file rename counted (control)" \
    lc_run "$root"
expect_output "lint-scopes: containers — a module this host does not compile is counted from its text" \
    "$LC_G counted from its text (compiled only on windows)" lc_run "$root"
lc_edit 's|    pub map: Map<u8, u8>,|    pub map: (Map<u8, u8>, std::collections::HashSet<u8>),|' "$LC_G"
expect_red_because "lint-scopes: containers — the text census sees a second container there" \
    "\`struct Gated\` holds 2 container span(s), its row says 1" lc_run "$root"
lc_reset
lc_append_to "$LC_G" 'pub struct Hole {' '    pub queue: std::collections::VecDeque<u8>,' '}'
expect_red_because "lint-scopes: containers — a new item with no allow, in a module this host does not compile" \
    "$LC_G:9: \`VecDeque\` names std::collections::VecDeque outside every \`container:\` allow" \
    lc_run "$root"
lc_reset
lc_append_to "$LC_G" 'pub struct Renamed {' '    pub map: Map<u8, u8>,' '}'
expect_red_because "lint-scopes: containers — a same-file rename used with no allow there" \
    "$LC_G:9: \`Map\` names std::collections::HashMap outside every \`container:\` allow" lc_run "$root"
lc_reset
lc_unallow "$LC_G" 'pub struct Gated {'
lc_row_set "$LC_G" 'struct Gated' drop
expect_red_because "lint-scopes: containers — an allow and its row deleted there, the container kept" \
    "$LC_G:5: \`Map\` names std::collections::HashMap outside every \`container:\` allow" lc_run "$root"
lc_reset
fx_git "$root" rm -q "$LC_T"
ls_commit "$root" "the table deleted"
lc_table
lc_gated_rows
expect_red_because "lint-scopes: containers — the table deleted at a ref whose gate has the maximum" \
    "has the container gate and no $LC_T — a deleted table is not a bootstrap" lc_run "$root"
# first landing: every copy predates the container gate (bootstrap) and the
# maximum alone bounds the sum
root=$(ls_root ls-containers-first)
printf '%s\npub mod a;\n' "$LS_ATTR" >"$root/crates/fake/src/lib.rs"
lc_source
lc_table
ls_gate "$LC_MAX" 0
expect green "lint-scopes: containers — the first landing (no copy has the gate) is the bootstrap" \
    lc_run "$root"
expect_output "lint-scopes: containers — the first landing discloses the bootstrap" \
    "predates the container gate (bootstrap)" lc_run "$root"
{
    printf '%s\n' '#[allow(clippy::disallowed_types, reason = "container: T")]' 'pub struct Wide {'
    for lc_i in $(seq 0 "$LC_MAX"); do printf '    pub f%s: VecDeque<u8>,\n' "$lc_i"; done
    printf '%s\n' '}'
} >>"$root/$LC_A"
lc_row "$LC_A" 'struct Wide' "$((LC_MAX + 1))" T 2099-12-31
expect_red_because "lint-scopes: containers — until a copy exists the maximum bounds the sum" \
    "above CONTAINER_EXEMPTIONS_MAX = $LC_MAX" lc_run "$root"
# after a commit, with a base tip that predates the container gate, the
# introducing commit is the copy that binds
root=$(ls_root ls-containers-intro)
fx_git "$root" rm -q "$LC_T"
fx_git "$root" -c user.name=t -c user.email=t@t commit -q --amend -m "base: before the container gate"
fx_git "$root" branch -q -f base-tip HEAD
printf '%s\npub mod a;\n' "$LS_ATTR" >"$root/crates/fake/src/lib.rs"
printf '# gate\nCONTAINER_EXEMPTIONS_MAX=%s\n' "$LC_MAX" >"$root/scripts/check-lint-scopes.sh"
lc_source
lc_table
ls_commit "$root" "the container gate and its table"
lc_lines "$LC_A" '    pub queue: VecDeque<u8>,' '    pub queue: VecDeque<u8>,' '    pub second: HashMap<u8, u8>,'
lc_row_set "$LC_A" 'struct Exempt' count 2
ls_commit "$root" "a raise, committed"
expect_red_because "lint-scopes: containers — a committed raise is red against the introducing commit" \
    "record T counts sum to 4, above the introducing commit's 3" lc_run "$root"
}

# Each probe case is a cold clippy build of about 3 s, so the probe's cases sit
# in groups of at most four, by the cut rule above.
group_lint_scopes_probe() {
ls_setup
# the probe's judge: a plant that compiles clean, and a plant that fails
# for an unrelated reason, are both red (ADR-0144 D5's probe rule).
ls_gate 0 0
root=$(ls_root ls-probe)
expect green "lint-scopes: stable plants draw their lints and the unstable call is refused" ls_probe "$SCRIPT_DIR/lint-scope-probe"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-inert"
sed -i.bak 's|Three::B \| Three::C => 0, // CONTROL|Three::B \| Three::C => 0, // PLANT clippy::wildcard_enum_match_arm|' "$work/probe-inert/src/lib.rs"
expect red "lint-scopes: a plant that compiles clean" ls_probe "$work/probe-inert"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-broken"
printf 'pub fn broken() -> u8 {\n    "not a u8"\n}\n' >>"$work/probe-broken/src/lib.rs"
expect red "lint-scopes: a probe that fails for an unrelated reason" ls_probe "$work/probe-broken"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unmarked"
sed -i.bak 's|_ => 0, // PLANT clippy::match_wildcard_for_single_variants|_ => 0,|' "$work/probe-unmarked/src/lib.rs"
expect red "lint-scopes: a diagnostic on a line with no marker" ls_probe "$work/probe-unmarked"
}

group_lint_scopes_probe_config() {
ls_setup
root=$(ls_root ls-probe)
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-wrong-path"
sed -i.bak 's|PLANT clippy::disallowed_methods std::fs::read$|PLANT clippy::disallowed_methods std::fs::write|' "$work/probe-wrong-path/src/filesystem.rs"
expect red "lint-scopes: the right lint naming the wrong API is red" ls_probe "$work/probe-wrong-path"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-missing-api"
sed -i.bak '/pub fn write/,/^    }/d' "$work/probe-missing-api/src/filesystem.rs"
expect red "lint-scopes: a config entry without a plant is red" ls_probe "$work/probe-missing-api"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unreachable-api"
sed 's|std::fs::write|std::fs::no_such_fn|' clippy.toml >"$work/probe-unreachable-api/clippy.toml"
expect_red_because "lint-scopes: an unreachable config entry is red" \
    'unresolved config entry: `std::fs::no_such_fn`' ls_probe "$work/probe-unreachable-api"
}

group_lint_scopes_probe_arch() {
ls_setup
root=$(ls_root ls-probe)
# ADR-0106 D18, on whichever leg runs this: an unresolved entry under the
# probe target's own `core::arch` module (on x86_64, ADR-0106 D7.5's inert
# `_rdtscp` shape) or under a module no leg compiles for is red; one under
# the other leg's module is disclosed by name; `allow-invalid` is red.
# shellcheck source=probe-target.sh
. "$SCRIPT_DIR/probe-target.sh"
# probe_target's canary: under stub tools it reports rustc's `target_arch`
# for two hosts (aarch64 for arm64e, not the triple's prefix; x86_64 under
# `uname -m` amd64), and returns 1 for a target rustc names no arch for and
# for a host arch `uname -m` disagrees with.
mkdir -p "$work/pt-bin"
IFS= read -r -d '' body <<'EOF' || true
#!/usr/bin/env bash
# stub rustc: the host and cfg lines probe-target.sh reads
for a in "$@"; do
    [ "$a" = no-such-target ] && { echo "error: could not find specification for target" >&2; exit 1; }
done
case " $* " in
*" -vV "*) echo "host: $PT_HOST" ;;
*" --print cfg "*) printf 'target_os="none"\ntarget_arch="%s"\n' "$PT_ARCH" ;;
*) exit 1 ;;
esac
EOF
printf '%s' "$body" >"$work/pt-bin/rustc"
printf '#!/usr/bin/env bash\n[ "$1" = -m ] && echo "$PT_UNAME_M"\n' >"$work/pt-bin/uname"
chmod +x "$work/pt-bin/rustc" "$work/pt-bin/uname"
pt_run() ( # <uname -m> <rustc host> <its target_arch> [<CARGO_BUILD_TARGET>]
    PATH="$work/pt-bin:$PATH" && PT_UNAME_M=$1 && PT_HOST=$2 && PT_ARCH=$3
    export PT_UNAME_M PT_HOST PT_ARCH
    unset CARGO_BUILD_TARGET
    if [ -n "${4:-}" ]; then CARGO_BUILD_TARGET=$4 && export CARGO_BUILD_TARGET; fi
    probe_target && echo "target=$PROBE_TARGET arch=$PROBE_ARCH"
)
expect_output "probe-target: the arch is rustc's target_arch, not the triple's prefix" \
    "^target=arm64e-apple-darwin arch=aarch64\$" pt_run arm64 arm64e-apple-darwin aarch64
expect_output "probe-target: an x86_64 host whose uname -m says amd64" \
    "^target=x86_64-unknown-freebsd arch=x86_64\$" pt_run amd64 x86_64-unknown-freebsd x86_64
expect_red_because "probe-target: a host arch uname -m disagrees with is red" \
    "aarch64, which disagrees with uname -m (x86_64)" pt_run x86_64 arm64e-apple-darwin aarch64
expect_red_because "probe-target: a target rustc names no arch for is red" \
    "names no target_arch for target 'no-such-target'" pt_run arm64 arm64e-apple-darwin aarch64 no-such-target
probe_target || { echo "selftest: the probe's target architecture is unknown" >&2; exit 2; }
LS_ARCH=$PROBE_ARCH
LS_OTHER=
for ls_a in $PROBE_LEG_ARCHES; do
    if [ "$ls_a" != "$LS_ARCH" ]; then LS_OTHER=$ls_a && break; fi
done
ls_arch_probe() { # <name> <entry fields>: the production config plus one entry
    cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/$1"
    awk -v row="    { $2 }," '{ print } $0 == "disallowed-methods = [" { print row }' clippy.toml \
        >"$work/$1/clippy.toml"
}
ls_discloses() { # <probe> <pattern>: green, and the verdict names the pattern
    local out
    out=$(ls_probe "$1" 2>&1) || { printf '%s\n' "$out"; return 1; }
    printf '%s\n' "$out"
    grep -q -- "$2" <<<"$out"
}
ls_arch_probe probe-arch-own "path = \"core::arch::$LS_ARCH::_no_such_intrinsic\", reason = \"plant\""
expect_red_because "lint-scopes: an unresolved entry under the probe target's own architecture is red" \
    "unresolved config entry: \`core::arch::$LS_ARCH::_no_such_intrinsic\`" ls_probe "$work/probe-arch-own"
ls_arch_probe probe-arch-typo 'path = "core::arch::x86_46::_rdtsc", reason = "plant"'
expect_red_because "lint-scopes: an entry under a module no leg compiles for is red on every leg" \
    'unresolved config entry: `core::arch::x86_46::_rdtsc`' ls_probe "$work/probe-arch-typo"
ls_arch_probe probe-arch-other "path = \"core::arch::$LS_OTHER::_no_such_intrinsic\", reason = \"plant\""
expect green "lint-scopes: an entry under the other leg's architecture is disclosed, not red" \
    ls_discloses "$work/probe-arch-other" \
    "not resolvable on $LS_ARCH, enforced on their own architecture's leg: .*core::arch::$LS_OTHER::_no_such_intrinsic"
ls_arch_probe probe-allow-invalid \
    "path = \"core::arch::$LS_ARCH::_no_such_intrinsic\", reason = \"plant\", allow-invalid = true"
expect_red_because "lint-scopes: allow-invalid, which silences an inert entry, is red" \
    "core::arch::$LS_ARCH::_no_such_intrinsic carries allow-invalid" ls_probe "$work/probe-allow-invalid"
}

group_lint_scopes_legs() {
# shellcheck source=probe-target.sh
. "$SCRIPT_DIR/probe-target.sh"
# probe-target.sh defers an entry under another leg's `core::arch` to that
# leg, so each architecture of PROBE_LEG_ARCHES needs a build-test matrix leg
# that runs every probe gate with no `if:` or `continue-on-error:`, and no
# knob that retargets or skips their probes — else its entries are disclosed
# everywhere and enforced nowhere. The fuzz-target gate is held the same way
# (ADR-0198 D2): each leg compiles the fuzz targets under its own `cfg`
# (process_info.rs's Linux and non-Linux `read_cpu` arms among them). The waker gate runs on the Linux legs only
# (ADR-0106 D8): its step may carry exactly `if: runner.os == 'Linux'`, and
# because atomics are spelled per target, each of WAKER_LEG_TARGETS needs a
# Linux leg of that triple — macOS shares aarch64 but not the spelling.
# Stdlib python, a line-and-indent parser of this file's layout that fails
# closed (a shape it does not read is red): PyYAML is not on every runner image.
legs_cover() { # <workflow>
    python3 - "$1" "$PROBE_LEG_ARCHES" "$WAKER_LEG_TARGETS" <<'PY'
import re, sys
wf, legs, waker_targets = sys.argv[1], sys.argv[2].split(), sys.argv[3].split()
# GitHub-hosted labels: architecture, target triple, runner.os. An unknown
# label is red, so a new leg names all three here before it counts.
RUNNER = {"ubuntu-24.04": ("x86_64", "x86_64-unknown-linux-gnu", "Linux"),
          "ubuntu-24.04-arm": ("aarch64", "aarch64-unknown-linux-gnu", "Linux"),
          "macos-15": ("aarch64", "aarch64-apple-darwin", "macOS")}
WAKER = "./scripts/check-waker-atomics.sh"
WAKER_IF = "if: runner.os == 'Linux'"
GATES = ("./scripts/check-clock-ban.sh", "./scripts/check-lint-scopes.sh", WAKER,
         "./scripts/check-fuzz-targets.sh")
lines = open(wf, encoding="utf-8").read().split("\n")
if "  build-test:" not in lines:
    sys.exit(f"LEG-COVERAGE: {wf} has no build-test job")
start = lines.index("  build-test:") + 1
end = next((i for i in range(start, len(lines)) if re.match(r"  \S", lines[i])), len(lines))
job, errors = lines[start:end], []
# Knobs that retarget a probe gate or end it before its probe. Set in the
# workflow's top-level env or anywhere in the job (a step can export through
# $GITHUB_ENV), the probe gates inherit them; comments aside.
KNOBS = re.compile(r"\b(CARGO_BUILD_TARGET|INF_LINT_RULES|INF_LINT_API_DIAGNOSTICS|INF_LINT_PROBE\w*"
                   r"|INF_CHECK_ROOT|INF_CLOCK_BAN_PROBE|INF_WAKER_\w+|INF_FUZZ_CARGO"
                   r"|INF_FUZZ_CHECK_PROBE)\b")
if "jobs:" not in lines:
    errors.append("no top-level `jobs:` line")
for line in (lines[:lines.index("jobs:")] if "jobs:" in lines else []) + job:
    knob = KNOBS.search(line.split("#", 1)[0])
    if knob:
        errors.append(f"`{knob.group(1)}` is set where the probe gates inherit it: {line.strip()}")
if "    runs-on: ${{ matrix.os }}" not in job:
    errors.append("build-test does not run on its matrix")
for line in job:
    if re.match(r"    (if|continue-on-error):", line):
        errors.append(f"the build-test job carries `{line.strip()}`")
    if re.match(r"\s+(include|exclude):", line):
        errors.append("a matrix include/exclude is not read by this check")
steps, step = [], None
for line in job:
    if line.startswith("      - "):
        step = [line[8:]]
        steps.append(step)
    elif step is not None and line.startswith("        "):
        step.append(line[8:])
    else:
        step = None
for gate in GATES:
    runs = [st for st in steps if f"run: {gate}" in st]
    if not runs:
        errors.append(f"no build-test step runs {gate}")
    for st in runs:
        for key in ("if", "continue-on-error"):
            rows = [row for row in st if row.startswith(f"{key}:")]
            if gate == WAKER and key == "if" and rows == [WAKER_IF]:
                continue
            if rows:
                errors.append(f"the step that runs {gate} carries `{key}:`")
waker_linux = any(WAKER_IF in st for st in steps if f"run: {WAKER}" in st)
oses = [m.group(1) for line in job for m in [re.fullmatch(r"        os: \[(.*)\]", line)] if m]
if len(oses) != 1:
    errors.append("build-test's matrix has no single `os: [...]` list")
arches, waker_legs = set(), set()
for label in [x.strip() for x in oses[0].split(",")] if oses else []:
    if label in RUNNER:
        arch, triple, runner_os = RUNNER[label]
        arches.add(arch)
        if runner_os == "Linux" or not waker_linux:
            waker_legs.add(triple)
    else:
        errors.append(f"runner {label} has no architecture in this check")
for arch in legs:
    if arch not in arches:
        errors.append(f"no build-test leg compiles for {arch}, the leg its core::arch entries are deferred to")
for triple in waker_targets:
    if triple not in waker_legs:
        errors.append(f"no build-test leg runs the waker gate for {triple}")
for e in errors:
    print(f"LEG-COVERAGE: {e}")
sys.exit(1 if errors else 0)
PY
}
LS_WF=.github/workflows/infinity-ci.yml
expect green "probe-target: each leg architecture has a build-test leg running every probe gate" \
    legs_cover "$LS_WF"
sed 's/^        os: \[ubuntu-24.04, /        os: [/' "$LS_WF" >"$work/wf-no-x86.yml"
expect_red_because "probe-target: a matrix without its x86_64 leg is red" \
    "no build-test leg compiles for x86_64" legs_cover "$work/wf-no-x86.yml"
awk -v row="        if: runner.os == 'Linux'" \
    '{ print } $0 == "        run: ./scripts/check-lint-scopes.sh" { print row }' "$LS_WF" >"$work/wf-gated.yml"
expect_red_because "probe-target: a probe gate's step under an if: is red" \
    'the step that runs ./scripts/check-lint-scopes.sh carries `if:`' legs_cover "$work/wf-gated.yml"
# The waker step's one allowed `if:` is the Linux-only row (ADR-0106 D8).
awk -v run="        run: ./scripts/check-waker-atomics.sh" -v row="        if: github.event_name == 'push'" \
    'NR > 1 { line = prev; if ($0 == run && prev ~ /^        if: /) line = row; print line } { prev = $0 } END { print prev }' \
    "$LS_WF" >"$work/wf-waker-if.yml"
expect_red_because "probe-target: the waker step under another if: is red" \
    'the step that runs ./scripts/check-waker-atomics.sh carries `if:`' legs_cover "$work/wf-waker-if.yml"
# Dropping the arm64 Linux leg leaves aarch64 covered by macOS for the
# architecture check, but not the waker gate's aarch64-unknown-linux-gnu.
sed 's/^\(        os: \[.*\), ubuntu-24.04-arm\(.*\]\)$/\1\2/' "$LS_WF" >"$work/wf-no-arm-linux.yml"
expect_red_because "probe-target: a matrix without the arm64 Linux leg is red for the waker gate" \
    "no build-test leg runs the waker gate for aarch64-unknown-linux-gnu" legs_cover "$work/wf-no-arm-linux.yml"
awk -v a="        env:" -v b='          INF_WAKER_PROBE: "off"' \
    '{ print } $0 == "        run: ./scripts/check-waker-atomics.sh" { print a; print b }' "$LS_WF" >"$work/wf-waker-off.yml"
expect_red_because "probe-target: a step env that turns the waker probe off is red" \
    "\`INF_WAKER_PROBE\` is set where the probe gates inherit it" legs_cover "$work/wf-waker-off.yml"
sed 's/^\(        os: \[.*\)\]$/\1, windows-2025]/' "$LS_WF" >"$work/wf-unknown.yml"
expect_red_because "probe-target: a runner label with no known architecture is red" \
    "runner windows-2025 has no architecture" legs_cover "$work/wf-unknown.yml"
awk -v a="    env:" -v b="      CARGO_BUILD_TARGET: aarch64-unknown-linux-gnu" \
    '{ print } $0 == "    runs-on: ${{ matrix.os }}" { print a; print b }' "$LS_WF" >"$work/wf-retarget.yml"
expect_red_because "probe-target: a job env that retargets the probes is red" \
    "\`CARGO_BUILD_TARGET\` is set where the probe gates inherit it" legs_cover "$work/wf-retarget.yml"
awk -v a="        env:" -v b='          INF_LINT_RULES: "1"' \
    '{ print } $0 == "        run: ./scripts/check-lint-scopes.sh" { print a; print b }' "$LS_WF" >"$work/wf-rules.yml"
expect_red_because "probe-target: a step env that ends lint-scopes before its probe is red" \
    "\`INF_LINT_RULES\` is set where the probe gates inherit it" legs_cover "$work/wf-rules.yml"
awk -v row="        if: runner.os == 'Linux'" \
    '{ print } $0 == "        run: ./scripts/check-fuzz-targets.sh" { print row }' "$LS_WF" >"$work/wf-fuzz-if.yml"
expect_red_because "probe-target: the fuzz-target gate's step under an if: is red" \
    'the step that runs ./scripts/check-fuzz-targets.sh carries `if:`' legs_cover "$work/wf-fuzz-if.yml"
awk -v a="        env:" -v b='          INF_FUZZ_CHECK_PROBE: "off"' \
    '{ print } $0 == "        run: ./scripts/check-fuzz-targets.sh" { print a; print b }' "$LS_WF" >"$work/wf-fuzz-off.yml"
expect_red_because "probe-target: a step env that skips the fuzz-target probe is red" \
    "\`INF_FUZZ_CHECK_PROBE\` is set where the probe gates inherit it" legs_cover "$work/wf-fuzz-off.yml"
awk -v a="    env:" -v b="      INF_FUZZ_CARGO: /bin/true" \
    '{ print } $0 == "    runs-on: ${{ matrix.os }}" { print a; print b }' "$LS_WF" >"$work/wf-fuzz-cargo.yml"
expect_red_because "probe-target: a job env that replaces the fuzz-target gate's cargo is red" \
    "\`INF_FUZZ_CARGO\` is set where the probe gates inherit it" legs_cover "$work/wf-fuzz-cargo.yml"
}

# Probe cases again: at most four a group, as above group_lint_scopes_probe.
group_lint_scopes_unstable() {
ls_setup
root=$(ls_root ls-probe)
# ADR-0144 A1: keep the unstable refusal distinct from Clippy witnesses.
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-stable"
sed -i.bak 's|std::fs::set_times(path, std::fs::FileTimes::new())|std::fs::metadata(path)|' "$work/probe-unstable-stable/unstable/set_times.rs"
expect red "lint-scopes: replacing the unstable call with a stable operation is red" ls_probe "$work/probe-unstable-stable"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-type"
printf 'pub fn unrelated() -> u8 { "not a u8" }\n' >>"$work/probe-unstable-type/unstable/set_times.rs"
expect red "lint-scopes: an unrelated error alongside the unstable refusal is red" ls_probe "$work/probe-unstable-type"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-missing"
sed -i.bak '/let _ =/d' "$work/probe-unstable-missing/unstable/set_times.rs"
expect red "lint-scopes: removing the unstable call is red" ls_probe "$work/probe-unstable-missing"
}

group_lint_scopes_unstable_census() {
ls_setup
root=$(ls_root ls-probe)
# ADR-0144 A1, as above: the unstable call's configured path, its feature, its census.
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-config"
sed 's|std::fs::set_times|std::fs::File::set_times|' clippy.toml >"$work/probe-unstable-config/clippy.toml"
expect red "lint-scopes: changing the configured unstable path is red" ls_probe "$work/probe-unstable-config"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-feature"
sed -i.bak 's|E0658 fs_set_times |E0658 wrong_feature |' "$work/probe-unstable-feature/unstable/set_times.rs"
expect red "lint-scopes: the unstable feature must match its census" ls_probe "$work/probe-unstable-feature"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-extra"
cp "$work/probe-unstable-extra/unstable/set_times.rs" "$work/probe-unstable-extra/unstable/extra.rs"
expect red "lint-scopes: the unstable census cannot grow" ls_probe "$work/probe-unstable-extra"
}

group_lint_scopes_container_plants() {
ls_setup
root=$(ls_root ls-probe)
# ADR-0163 D2: the shipped probe's container plants draw their lint naming
# their path under the production config (the green case above). A config
# that loses a path, the backing without its one allow, and a two-span plant
# over a line holding one container are each red for their own reason.
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-containers-unconfigured"
grep -v 'path = "std::collections::VecDeque"' clippy.toml >"$work/probe-containers-unconfigured/clippy.toml"
expect_red_because "lint-scopes: a config without one container path" \
    "config needs 3 distinct container disallowed-types, found 2" ls_probe "$work/probe-containers-unconfigured"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-containers-backing"
sed -i.bak '/reason = "container: capped-backing"/d' "$work/probe-containers-backing/src/containers.rs"
expect_red_because "lint-scopes: the backing without its one allow draws on its field" \
    "control drew \[('clippy::disallowed_types', 'std::collections::VecDeque')\]" ls_probe "$work/probe-containers-backing"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-containers-one-span"
sed -i.bak 's|(std::collections::VecDeque<u8>, std::collections::VecDeque<u8>)|(std::collections::VecDeque<u8>, u8)|; s|pair.1.capacity()|usize::from(pair.1)|' "$work/probe-containers-one-span/src/containers.rs"
expect_red_because "lint-scopes: a two-span plant over a line that holds one container" \
    "wants 2 distinct spans of clippy::disallowed_types" ls_probe "$work/probe-containers-one-span"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-containers-new"
printf 'pub struct Added {\n    pub value: std::collections::HashMap<u8, u8>,\n}\n' >>"$work/probe-containers-new/src/containers.rs"
expect_red_because "lint-scopes: a new container with no plant marker is red by its lint and path" \
    "unmarked diagnostic \[('clippy::disallowed_types', 'std::collections::HashMap')\]" ls_probe "$work/probe-containers-new"
}

group_lint_scopes_slot_plants() {
ls_setup
root=$(ls_root ls-probe)
# ADR-0144 A2: the slot plants draw `await_holding_invalid_type` naming their
# configured path under the production config (the green case above): the
# slot, and the answer that carries it, since the lint matches the outermost
# type of a live local. A config without either slot type, a plant whose slot
# or answer is consumed before the await, and a discarded publish that is not
# discarded are each red for their own reason.
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-await-unconfigured"
grep -v 'path = "inf_foundation::bounded::DequeSlot"' clippy.toml >"$work/probe-await-unconfigured/clippy.toml"
expect_red_because "lint-scopes: a config without the slot type" \
    "config needs 9 distinct slot type await-holding-invalid-types, found 8" ls_probe "$work/probe-await-unconfigured"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-await-unconfigured-answer"
grep -v 'path = "inf_foundation::bounded::Reserved"' clippy.toml >"$work/probe-await-unconfigured-answer/clippy.toml"
expect_red_because "lint-scopes: a config without the answer that carries the slot" \
    "plant path inf_foundation::bounded::Reserved has no config entry" ls_probe "$work/probe-await-unconfigured-answer"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-await-consumed"
sed -i.bak 's|            yield_once().await;|            slot.publish(0);|; s|            slot.publish(1);|            yield_once().await;|' "$work/probe-await-consumed/src/awaits.rs"
expect_red_because "lint-scopes: a slot consumed before the await draws nothing on the plant" \
    "did NOT draw clippy::await_holding_invalid_type inf_foundation::bounded::DequeSlot" ls_probe "$work/probe-await-consumed"
}

group_lint_scopes_answer_plants() {
ls_setup
root=$(ls_root ls-probe)
# ADR-0144 A2, as above: the answer's plant and the discard plant.
# The answer's plant at four columns: the slot's lines above sit at twelve,
# so the anchored edits reach only the answer's await and its match.
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-await-answer-matched"
sed -i.bak 's|^    yield_once().await;$|    publish_one(reserved); // matched first|; s|^    publish_one(reserved);$|    yield_once().await;|' "$work/probe-await-answer-matched/src/awaits.rs"
expect_red_because "lint-scopes: an answer matched before the await draws nothing on the plant" \
    "did NOT draw clippy::await_holding_invalid_type inf_foundation::bounded::Reserved" ls_probe "$work/probe-await-answer-matched"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-discard-published"
sed -i.bak 's|        let _ = slot.publish(1); // PLANT clippy::let_unit_value|        slot.publish(1); // PLANT clippy::let_unit_value|' "$work/probe-discard-published/src/discards.rs"
expect_red_because "lint-scopes: a publish that is not discarded draws no discard lint" \
    "did NOT draw clippy::let_unit_value" ls_probe "$work/probe-discard-published"
}

group_parent_doc_gates() {
# ADR-0164: the parent doc gates state their scope — a standalone checkout
# skips out loud; a parent whose gates are missing is red, not a skip.
PARENT=$SCRIPT_DIR/check-parent-doc-gates.sh
mkdir -p "$work/parent-none/eng" "$work/parent-bare/eng" "$work/parent-bare/docs"
: >"$work/parent-bare/docs/infinity-master-plan.md"
expect_output "parent-doc-gates: no parent governance prints the skip" "SKIPPED" \
    env INF_CHECK_ROOT="$work/parent-none/eng" "$PARENT"
expect green "parent-doc-gates: a standalone checkout is green" \
    env INF_CHECK_ROOT="$work/parent-none/eng" "$PARENT"
expect red "parent-doc-gates: a parent without its gates is red" \
    env INF_CHECK_ROOT="$work/parent-bare/eng" "$PARENT"
# One gate at a time: a parent holding the other gates, with the claim-ledger
# lint or the public-docs gate missing, not executable, or failing, is red,
# and the runner's run of a complete parent must reach each of them. Stubs
# stand in for the parent's gates, so the plants judge the runner alone.
pd_parent() { # <name> <state: ok|missing|noexec|fail> [<gate in that state>]
    local root="$work/parent-$1" target=${3:-check-claim-ledger.sh} gate
    mkdir -p "$root/eng" "$root/docs" "$root/scripts"
    : >"$root/docs/infinity-master-plan.md"
    for gate in check-adr-links.sh check-drr.sh check-claim-ledger.sh check-public-doc-links.sh; do
        [ "$gate" = "$target" ] && [ "$2" = missing ] && continue
        printf '#!/usr/bin/env bash\necho "stub %s ran on $INF_ENGINE_ROOT"\n' "$gate" \
            >"$root/scripts/$gate"
        chmod +x "$root/scripts/$gate"
    done
    case $2 in
        noexec) chmod -x "$root/scripts/$target" ;;
        fail) printf '#!/usr/bin/env bash\necho "stub %s red"\nexit 1\n' "$target" \
            >"$root/scripts/$target" ;;
    esac
}
# pd_ran <runner> <name> <gate>: the runner is green on the parent and its
# output shows the gate ran against the checkout under test.
pd_ran() {
    local out
    out=$(env INF_CHECK_ROOT="$work/parent-$2/eng" "$1" 2>&1) || return 1
    grep -qxF "stub $3 ran on $work/parent-$2/eng" <<<"$out"
}
pd_parent full ok
pd_parent no-ledger missing
pd_parent noexec-ledger noexec
pd_parent red-ledger fail
pd_parent no-doc-links missing check-public-doc-links.sh
pd_parent noexec-doc-links noexec check-public-doc-links.sh
pd_parent red-doc-links fail check-public-doc-links.sh
PD_GATES=(check-adr-links.sh check-drr.sh check-claim-ledger.sh check-public-doc-links.sh)
for pd_gate in "${PD_GATES[@]}"; do
    expect green "parent-doc-gates: a parent with all four gates runs $pd_gate" \
        pd_ran "$PARENT" full "$pd_gate"
done
expect_output "parent-doc-gates: the adr-links and drr gates ran first" \
    "stub check-drr.sh ran" env INF_CHECK_ROOT="$work/parent-no-ledger/eng" "$PARENT"
expect red "parent-doc-gates: only the claim-ledger lint missing is red" \
    env INF_CHECK_ROOT="$work/parent-no-ledger/eng" "$PARENT"
expect_output "parent-doc-gates: the missing gate is named" \
    "check-claim-ledger.sh missing or not executable" \
    env INF_CHECK_ROOT="$work/parent-no-ledger/eng" "$PARENT"
expect red "parent-doc-gates: only the claim-ledger lint not executable is red" \
    env INF_CHECK_ROOT="$work/parent-noexec-ledger/eng" "$PARENT"
expect_output "parent-doc-gates: the non-executable gate is named" \
    "check-claim-ledger.sh missing or not executable" \
    env INF_CHECK_ROOT="$work/parent-noexec-ledger/eng" "$PARENT"
expect red "parent-doc-gates: a red claim-ledger lint is red" \
    env INF_CHECK_ROOT="$work/parent-red-ledger/eng" "$PARENT"
expect red "parent-doc-gates: only the public-docs gate missing is red" \
    env INF_CHECK_ROOT="$work/parent-no-doc-links/eng" "$PARENT"
expect_output "parent-doc-gates: the missing public-docs gate is named" \
    "check-public-doc-links.sh missing or not executable" \
    env INF_CHECK_ROOT="$work/parent-no-doc-links/eng" "$PARENT"
expect_output "parent-doc-gates: the other three gates ran before the missing one" \
    "stub check-claim-ledger.sh ran" env INF_CHECK_ROOT="$work/parent-no-doc-links/eng" "$PARENT"
expect red "parent-doc-gates: only the public-docs gate not executable is red" \
    env INF_CHECK_ROOT="$work/parent-noexec-doc-links/eng" "$PARENT"
expect red "parent-doc-gates: a red public-docs gate is red" \
    env INF_CHECK_ROOT="$work/parent-red-doc-links/eng" "$PARENT"
# A runner whose gate list drops any one gate is caught by the runs above:
# every name of the list has its plant, in whatever position it stands.
for pd_gate in "${PD_GATES[@]}"; do
    sed "s/^\(for gate in\)\(.*\) ${pd_gate//./\\.}\([ ;]\)/\1\2\3/" "$PARENT" \
        >"$work/pd-drop-$pd_gate"
    chmod +x "$work/pd-drop-$pd_gate"
    expect red "parent-doc-gates: the runner plant dropped $pd_gate from the gate list" \
        cmp -s "$PARENT" "$work/pd-drop-$pd_gate"
    expect red "parent-doc-gates: a runner that drops $pd_gate is red" \
        pd_ran "$work/pd-drop-$pd_gate" full "$pd_gate"
done
}

group_arith_spellings() {
# ADR-0165 D2: the spelling table equals the tree, both ways; comments, test
# modules and code outside an item scope are not sites.
SPELL=$SCRIPT_DIR/check-arith-spellings.sh
as_root() { # <name> <scope-row> <table rows…>
    local root="$work/as-$1" row=$2
    shift 2
    mkdir -p "$root/docs" "$root/crates/fake/src"
    cat >"$root/crates/fake/src/dec.rs" <<'RS'
pub fn f(a: usize, b: u64) -> u64 {
    let q = b'"'; // a.saturating_add(9) after a char literal is a comment
    // usize::saturating_add in a comment is not a site
    let _s = "\\"; // x.saturating_add(1) after an escaped backslash is a comment
    let _t = "a.wrapping_add(1) and Wrapping in a string are text";
    /* b.saturating_mul(2) in a block comment */
    b.wrapping_mul(3).wrapping_add(q as u64 + a.saturating_add(1) as u64)
}

pub fn g(a: usize) -> usize {
    a.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    fn t() -> usize {
        1usize.saturating_add(1)
    }
}
RS
    printf 't/x\t%s\tcast,arith\tdeny\n' "$row" >"$root/docs/lint-scopes.tsv"
    printf '%s\n' "$@" >"$root/docs/arith-spellings.tsv"
}
D=crates/fake/src/dec.rs
# as_red <case> <pattern>: red, and the message names the cause.
as_red() {
    expect red "arith-spellings: $1" env INF_CHECK_ROOT="$work/as-$1" "$SPELL"
    expect_output "arith-spellings: $1 names its cause" "$2" \
        env INF_CHECK_ROOT="$work/as-$1" "$SPELL"
}
as_root clean "$D" "saturating	2	$D" "wrapping	2	$D"
expect green "arith-spellings: table equals the tree" env INF_CHECK_ROOT="$work/as-clean" "$SPELL"
expect_output "arith-spellings: strings, comments and test modules are not sites" \
    "2 saturating, 2 wrapping, 0 overflowing" \
    env INF_CHECK_ROOT="$work/as-clean" "$SPELL"
as_root added "$D" "saturating	2	$D" "wrapping	2	$D"
sed -i.bak 's/a.saturating_sub(1)/a.saturating_sub(1).saturating_add(2)/' "$work/as-added/$D"
as_red added "3 .saturating_.. call(s), docs/arith-spellings.tsv says 2"
as_root removed "$D" "saturating	3	$D" "wrapping	2	$D"
as_red removed "2 .saturating_.. call(s), docs/arith-spellings.tsv says 3"
# The two plants below add a site and keep every other count, so only the
# new family can turn them red.
as_root overflowing "$D" "saturating	2	$D" "wrapping	2	$D"
sed -i.bak 's/a.saturating_sub(1)/a.saturating_sub(1).overflowing_add(1).0/' \
    "$work/as-overflowing/$D"
as_red overflowing "1 .overflowing_.. call(s), docs/arith-spellings.tsv says 0"
as_root wrapping-type "$D" "saturating	2	$D" "wrapping	2	$D"
wrap='(core::num::Wrapping(a.saturating_sub(1)) - core::num::Wrapping(1)).0'
sed -i.bak "s/a.saturating_sub(1)/$wrap/" "$work/as-wrapping-type/$D"
as_red wrapping-type "4 .wrapping_.. call(s), docs/arith-spellings.tsv says 2"
as_root stale "$D" "saturating	2	$D" "wrapping	2	$D" "wrapping	1	crates/fake/src/gone.rs"
as_red stale "gone.rs. is not a deny-arith scope"
as_root twice "$D" "saturating	2	$D" "wrapping	2	$D" "wrapping	2	$D"
as_red twice "wrapping is listed twice"
as_root zero "$D" "saturating	2	$D" "wrapping	2	$D" "saturating	0	$D"
as_red zero "malformed row"
as_root missing "$D" "saturating	2	$D"
rm "$work/as-missing/docs/arith-spellings.tsv"
as_red missing "arith-spellings.tsv is missing"
as_root item "$D::g" "saturating	1	$D::g"
expect green "arith-spellings: an item scope counts inside its fn" \
    env INF_CHECK_ROOT="$work/as-item" "$SPELL"
as_root item-wide "$D::g" "saturating	2	$D::g"
as_red item-wide "g: 1 .saturating_.. call(s), docs/arith-spellings.tsv says 2"
}

# ------------------------------------------------- fuzz targets (ADR-0198)
group_fuzz_targets() {
# check-fuzz-targets.sh on fixture roots, each its own git repository, with a
# stub cargo (INF_FUZZ_CARGO) that records each call and answers from the
# files a case writes beside the root: the census rows (C1–C7, H1–H3), cargo's
# report (K1, K2), the probe (P1–P5, the probe on and the stub answering its
# two manifests), the call itself, and the matrix mode.
FUZZ=./scripts/check-fuzz-targets.sh
FZ_CARGO="$work/fz-cargo"
IFS= read -r -d '' body <<'EOF' || true
#!/usr/bin/env bash
# The stub cargo: records the call, then answers for the manifest it was
# given from $FZ_STUB/<manifest, / as _>.{out,err,status}.
set -eu
manifest='' previous=''
for arg in "$@"; do
    [ "$previous" != --manifest-path ] || manifest=$arg
    previous=$arg
done
key=$(printf '%s' "$manifest" | tr '/' '_')
{
    printf 'call:'
    printf ' %s' "$@"
    printf '\nRUSTFLAGS=%s\n' "${RUSTFLAGS-<unset>}"
    printf 'CARGO_ENCODED_RUSTFLAGS=%s\n' "${CARGO_ENCODED_RUSTFLAGS-<unset>}"
} >>"$FZ_STUB/calls"
[ ! -f "$FZ_STUB/$key.out" ] || cat "$FZ_STUB/$key.out"
[ ! -f "$FZ_STUB/$key.err" ] || cat "$FZ_STUB/$key.err" >&2
exit "$(cat "$FZ_STUB/$key.status" 2>/dev/null || echo 0)"
EOF
printf '%s' "$body" >"$FZ_CARGO"
chmod +x "$FZ_CARGO"
IFS= read -r -d '' FZ_PARSE <<'EOF' || true
pub fn f() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one() {
        assert_eq!(crate::parse::f(), f());
    }
}
EOF
IFS= read -r -d '' FZ_TARGET <<'EOF' || true
#[path = "../../src/parse.rs"]
mod parse;

fn main() {
    let _ = parse::f();
}
EOF
fz_key() { printf '%s' "$1" | tr '/' '_'; }
fz_sed() { sed "$1" "$2" >"$2.new" && mv "$2.new" "$2"; }  # <expression> <file>: an edit in place
# fz_depinfo <root> <bin> <file>…: the dep-info rustc writes beside the bin's
# metadata, naming the files it read for the crate (relative to its manifest).
fz_depinfo() {
    local root=$1 bin=$2 d rmeta
    shift 2
    d="$root.stub/deps/$bin-0.d"
    rmeta="$root.stub/deps/lib$bin-0.rmeta"
    mkdir -p "$root.stub/deps"
    {
        printf '%s:' "$d"
        printf ' %s' "$@"
        printf '\n%s:' "$rmeta"
        printf ' %s' "$@"
        printf '\n\n'
        printf '%s:\n' "$@"
    } >"$d"
}
fz_artifact() { # <root> <manifest> <bin>: its report line; its dep-info names its own file
    fz_depinfo "$1" "$3" "fuzz_targets/$3.rs"
    printf '{"reason":"compiler-artifact","manifest_path":"%s/%s","target":{"kind":["bin"],"name":"%s"},"filenames":["%s.stub/deps/lib%s-0.rmeta"],"fresh":false}\n' \
        "$1" "$2" "$3" "$1" "$3"
}
fz_error() { # <root> <manifest> <bin> <code> <message> <primary file>
    printf '{"reason":"compiler-message","manifest_path":"%s/%s","target":{"kind":["bin"],"name":"%s"},"message":{"level":"error","code":{"code":"%s"},"message":"%s","rendered":"error[%s]: %s\\n","spans":[{"file_name":"%s","is_primary":true}]}}\n' \
        "$1" "$2" "$3" "$4" "$5" "$4" "$5" "$6"
}
# fz_answer <root> <manifest> [<bin>…]: the stub reports each bin compiled
# and exits 0; fz_status and fz_stderr change its exit and stderr.
fz_answer() {
    local root=$1 manifest=$2 name out
    shift 2
    out="${root:?}.stub/$(fz_key "$manifest")"
    : >"$out.out"
    for name in "$@"; do
        fz_artifact "$root" "$manifest" "$name" >>"$out.out"
    done
    : >"$out.err"
    printf '0\n' >"$out.status"
}
fz_status() { printf '%s\n' "$3" >"$1.stub/$(fz_key "$2").status"; }
fz_stderr() { printf '%s\n' "$3" >"$1.stub/$(fz_key "$2").err"; }
# fz_manifest <root> <member> [<target>…]: the member's fuzz manifest, one
# [[bin]] row a target.
fz_manifest() {
    local root=$1 member=$2 name
    shift 2
    mkdir -p "$root/$member/fuzz/fuzz_targets"
    printf '[package]\nname = "%s-fuzz"\nversion = "0.0.0"\n\n[package.metadata]\ncargo-fuzz = true\n\n[workspace]\n' \
        "${member##*/}" >"$root/$member/fuzz/Cargo.toml"
    for name in "$@"; do
        printf '\n[[bin]]\nname = "%s"\npath = "fuzz_targets/%s.rs"\n' "$name" "$name" \
            >>"$root/$member/fuzz/Cargo.toml"
    done
}
# fz_root <name>: members crates/a (a library) and bins/b (a binary), one fuzz
# workspace each — t_one, and b_parse including bins/b/src/parse.rs by
# `#[path]` —, the six probe manifests, a `check` recipe running their gates,
# and the stub answering both workspaces green. Its stub files sit beside it.
fz_root() {
    local root probe
    [ -n "$1" ] && [ -n "$work" ] || { echo "fz_root: empty name" >&2; exit 2; }
    root="$work/fz-$1"
    rm -rf "${root:?}" "${root:?}.stub"
    mkdir -p "$root/crates/a/src" "$root/bins/b/src" "$root/scripts" "$root.stub"
    printf '[workspace]\nmembers = ["crates/*", "bins/*"]\n' >"$root/Cargo.toml"
    printf '[package]\nname = "a"\nversion = "0.0.0"\n' >"$root/crates/a/Cargo.toml"
    printf 'pub fn f() {}\n' >"$root/crates/a/src/lib.rs"
    printf '[package]\nname = "b"\nversion = "0.0.0"\n' >"$root/bins/b/Cargo.toml"
    printf 'mod parse;\n\nfn main() {}\n' >"$root/bins/b/src/main.rs"
    printf '%s' "$FZ_PARSE" >"$root/bins/b/src/parse.rs"
    fz_manifest "$root" crates/a t_one
    fz_manifest "$root" bins/b b_parse
    printf 'fn main() {}\n' >"$root/crates/a/fuzz/fuzz_targets/t_one.rs"
    printf '%s' "$FZ_TARGET" >"$root/bins/b/fuzz/fuzz_targets/b_parse.rs"
    cp -R "$SCRIPT_DIR/fuzz-check-probe" "$root/scripts/fuzz-check-probe"
    for probe in clock-ban-probe lint-scope-probe waker-atomics-probe; do
        mkdir -p "$root/scripts/$probe"
        printf '[package]\nname = "%s"\nversion = "0.0.0"\n\n[workspace]\n' "$probe" \
            >"$root/scripts/$probe/Cargo.toml"
    done
    mkdir -p "$root/scripts/waker-atomics-probe/nested"
    printf '[package]\nname = "nested"\nversion = "0.0.0"\n' \
        >"$root/scripts/waker-atomics-probe/nested/Cargo.toml"
    printf 'check:\n    ./scripts/check-clock-ban.sh\n    ./scripts/check-lint-scopes.sh\n' >"$root/justfile"
    printf '    ./scripts/check-fuzz-targets.sh\n    ./scripts/check-waker-atomics.sh\n' >>"$root/justfile"
    git -C "$root" init -q
    fz_answer "$root" crates/a/fuzz/Cargo.toml t_one
    fz_answer "$root" bins/b/fuzz/Cargo.toml b_parse
    fz_depinfo "$root" b_parse fuzz_targets/b_parse.rs fuzz_targets/../../src/parse.rs
    echo "$root"
}
# Each run unsets CARGO_TARGET_DIR, so a fixture's kept directory sits in its
# own target, under the work directory, whatever the caller exports.
fz_run() { # <root> [args…]: the gate on a fixture root, its probe off
    env -u CARGO_TARGET_DIR INF_CHECK_ROOT="$1" INF_FUZZ_CHECK_PROBE=off INF_FUZZ_CARGO="$FZ_CARGO" \
        FZ_STUB="$1.stub" "$FUZZ" "${@:2}"
}
fz_probe_run() { # <root>: the gate with its probe on
    env -u CARGO_TARGET_DIR INF_CHECK_ROOT="$1" INF_FUZZ_CARGO="$FZ_CARGO" FZ_STUB="$1.stub" "$FUZZ"
}
fz_red() { # <label> <pattern> <root>: red on the fixture, naming its row
    expect_red_because "fuzz-targets: $1" "$2" fz_run "$3"
}

# fz_once <log> <command…> runs a gate once and keeps its output and status;
# fz_replay <log> gives both back, so the cases that read one unchanged run
# share it (each gate run costs a Python start, two git calls and the stub).
fz_once() {
    local log=$1 status=0
    shift
    "$@" >"$log" 2>&1 || status=$?
    printf '%s\n' "$status" >"$log.status"
}
fz_replay() { cat "$1" && return "$(cat "$1.status")"; }
fz_lacks() { ! grep -q -- "$1" "$2"; }  # <pattern> <log>: the log lacks it
# The matrix mode's stdout is the whole matrix or nothing: fz_matrix_is reads
# it, fz_matrix_red an exit 1 with nothing printed.
fz_matrix_is() { # <root> <mode> <expected stdout>
    local got
    got=$(env INF_CHECK_ROOT="$1" "$FUZZ" --matrix "$2") || return 1
    [ "$got" = "$3" ] || { echo "matrix: $got"; return 1; }
}
fz_matrix_red() { # <root> [<mode>]
    local got status=0
    got=$(env INF_CHECK_ROOT="$1" "$FUZZ" --matrix "${2:-nightly}" 2>/dev/null) || status=$?
    [ "$status" -eq 1 ] && [ -z "$got" ]
}

root=$(fz_root clean)
fz_once "$root.log" fz_run "$root"
FZ_CLEAN_LOG=$root.log  # read again where the probe manifests are judged (C4)
expect green "fuzz-targets: a clean two-target root" fz_replay "$root.log"
expect_output "fuzz-targets: the OK line counts what it compiled and read" \
    "fuzz-targets OK: 2 fuzz workspaces, 2 of 2 targets compiled (2 .*bin.* rows, 2 files)" \
    fz_replay "$root.log"
expect_output "fuzz-targets: the OK line discloses the probe off in fixture mode" \
    "probe: skipped (fixture mode, INF_FUZZ_CHECK_PROBE=off); 6 probe manifests, each run by its gate" \
    fz_replay "$root.log"
expect_output "fuzz-targets: the OK line counts the #[path] sites and files read" \
    "0 seed directories (git-listed); 1 #.path. sites, 1 files read" fz_replay "$root.log"
expect_output "fuzz-targets: the OK line counts the dep-info read" \
    "carries #.macro_use.; dep-info of 2 of 2 compiled targets read, each naming only its target's file and the #.path. files read" \
    fz_replay "$root.log"
expect_output "fuzz-targets: the count line is printed on a green run" \
    "fuzz-targets: bins/b/fuzz/Cargo.toml: compiled 1 of 1" fz_replay "$root.log"
expect_red_because "fuzz-targets: the probe stays on outside a fixture root" \
    "honoured on a fixture root (INF_CHECK_ROOT) only" env -u INF_CHECK_ROOT INF_FUZZ_CHECK_PROBE=off "$FUZZ"
expect_red_because "fuzz-targets: a probe knob that is neither on nor off" "is 'no', not on or off" \
    env INF_CHECK_ROOT="$root" INF_FUZZ_CHECK_PROBE=no "$FUZZ"

# The call: every workspace's cargo call carries the flags the verdict rests
# on, with RUSTFLAGS replaced and CARGO_ENCODED_RUSTFLAGS unset whatever the
# caller set, and the gate's own target directory, `fuzz-check-<key>` under
# the checkout's target root, read with the variable unset: never the root's
# target itself, whose RUSTFLAGS differ, and never another checkout's; a
# variable naming another directory stops the gate before cargo (below).
fz_calls_ok() { # <stub dir> <the target directory every call must name>
    python3 - "$1/calls" "$2" <<'PY'
import sys
lines = open(sys.argv[1], encoding="utf-8").read().split("\n")
calls = [lines[i:i + 3] for i in range(0, len(lines) - 1, 3)]
want = {"bins/b/fuzz/Cargo.toml", "crates/a/fuzz/Cargo.toml"}
rest = ["--bins", "--locked", "--keep-going", "--message-format", "json", "--color", "never",
        "--target-dir", sys.argv[2]]
seen, bad = set(), []
for call, flags, encoded in calls:
    argv = call.split(" ")[1:]
    if len(argv) < 3 or argv[:2] != ["check", "--manifest-path"]:
        bad.append(f"not a check call: {call}")
        continue
    seen.add(argv[2])
    if argv[3:] != rest:
        bad.append(f"{argv[2]}: {' '.join(argv[3:])}")
    if flags != "RUSTFLAGS=-D warnings --cfg fuzzing":
        bad.append(f"{argv[2]}: {flags}")
    if encoded != "CARGO_ENCODED_RUSTFLAGS=<unset>":
        bad.append(f"{argv[2]}: {encoded}")
if seen != want:
    bad.append(f"workspaces called: {sorted(seen)}")
print("\n".join(bad))
sys.exit(1 if bad else 0)
PY
}
# fz_argv_run <gate> <root> [<CARGO_TARGET_DIR>]: the gate under a caller's
# own RUSTFLAGS, with CARGO_TARGET_DIR set to the third argument, or unset.
fz_argv_run() {
    if [ -n "${3:-}" ]; then
        env CARGO_TARGET_DIR="$3" RUSTFLAGS=-Awarnings CARGO_ENCODED_RUSTFLAGS=-Awarnings \
            INF_CHECK_ROOT="$2" INF_FUZZ_CHECK_PROBE=off INF_FUZZ_CARGO="$FZ_CARGO" \
            FZ_STUB="$2.stub" "$1"
    else
        env -u CARGO_TARGET_DIR RUSTFLAGS=-Awarnings CARGO_ENCODED_RUSTFLAGS=-Awarnings \
            INF_CHECK_ROOT="$2" INF_FUZZ_CHECK_PROBE=off INF_FUZZ_CARGO="$FZ_CARGO" \
            FZ_STUB="$2.stub" "$1"
    fi
}
root=$(fz_root argv)
FZ_KEY=$(fx_root_key "$root")
FZ_OWN="$(cd "$root" && pwd -P)/target/fuzz-check-$FZ_KEY"
expect green "fuzz-targets: green under a caller's RUSTFLAGS and CARGO_ENCODED_RUSTFLAGS" \
    fz_argv_run "$FUZZ" "$root"
expect green "fuzz-targets: each workspace's call carries the gate's flags and environment" \
    fz_calls_ok "$root.stub" "$FZ_OWN"
# A caller's CARGO_TARGET_DIR naming another directory, as a second worktree
# building into the first checkout's target has: the gate stops before cargo.
FZ_CALLER_TARGET="$work/fz-caller-target"
fz_refused() { # <gate> <root> <CARGO_TARGET_DIR>: a scope error on the helper's refusal, no call
    local status=0
    rm -f "${2:?}.stub/calls"
    fz_argv_run "$1" "$2" "$3" >"$work/fz-refused" 2>&1 || status=$?
    [ "$status" -ne 0 ] && [ ! -s "$2.stub/calls" ] && grep -q 'FUZZ-TARGETS SCOPE ERROR' "$work/fz-refused" &&
        grep -q "CARGO_TARGET_DIR is '$3'" "$work/fz-refused" && return 0
    echo "exit $status:"
    cat "$work/fz-refused"
    [ ! -s "$2.stub/calls" ] || { echo "cargo ran:"; cat "$2.stub/calls"; }
    return 1
}
expect green "fuzz-targets: a CARGO_TARGET_DIR naming another directory stops it before cargo" \
    fz_refused "$FUZZ" "$root" "$FZ_CALLER_TARGET"
ln -s "$root" "$work/fz-link"
for fz_var in "$(cd "$root" && pwd -P)/target" target "$work/fz-link/target"; do
    rm -f "${root:?}.stub/calls"
    fz_argv_run "$FUZZ" "$root" "$fz_var" >/dev/null 2>&1 || true
    expect green "fuzz-targets: CARGO_TARGET_DIR=$fz_var, the checkout's target, names the kept directory" \
        fz_calls_ok "$root.stub" "$FZ_OWN"
done
# The call's canaries: a copy of the gate with one part of its call changed
# turns the case red. Each copy runs on the argv root, its calls cleared,
# with CARGO_TARGET_DIR unset or set to the fourth argument.
FZ_MROOT=$root
fz_mutant() { # <name> <sed expression> <what the copy does> [<CARGO_TARGET_DIR>]
    local dir want=$FZ_OWN
    dir="$work/fz-gate-$1"
    rm -rf "${dir:?}"
    mkdir -p "$dir"
    cp "$SCRIPT_DIR/check-fuzz-targets.sh" "$SCRIPT_DIR/fuzz_targets.py" "$SCRIPT_DIR/lint_scope_table.py" \
        "$SCRIPT_DIR/strip-test-modules.awk" "$SCRIPT_DIR/target-root.sh" "$dir/"
    fz_sed "$2" "$dir/check-fuzz-targets.sh"
    chmod +x "$dir/check-fuzz-targets.sh"
    if cmp -s "$SCRIPT_DIR/check-fuzz-targets.sh" "$dir/check-fuzz-targets.sh"; then
        echo "selftest: the fuzz-targets mutant '$1' changed nothing" >&2
        exit 2
    fi
    if [ -n "${4:-}" ]; then
        expect red "fuzz-targets: the refusal's check is red on a gate that $3" \
            fz_refused "$dir/check-fuzz-targets.sh" "$FZ_MROOT" "$4"
        return
    fi
    rm -f "${FZ_MROOT:?}.stub/calls"
    fz_argv_run "$dir/check-fuzz-targets.sh" "$FZ_MROOT" >/dev/null 2>&1 || true
    expect red "fuzz-targets: the call's check is red on a gate that $3" \
        fz_calls_ok "$FZ_MROOT.stub" "$want"
}
fz_mutant bins 's/"check", "--bins", /"check", /' "drops --bins"
fz_mutant locked 's/"--locked", //' "drops --locked"
fz_mutant keep-going 's/"--keep-going", //' "drops --keep-going"
fz_mutant format 's/"--message-format", "json",/"--message-format", "short",/' "reads cargo's short format"
fz_mutant color 's/^        "--color", "never"\]$/        ]/' "drops --color never"
FZ_KEPT='^    INF_FUZZ_TARGET_DIR=$(target_root_kept fuzz-check)$'
fz_mutant target-dir "s|$FZ_KEPT|    INF_FUZZ_TARGET_DIR=\$TARGET_ROOT|" "shares the root's target directory"
FZ_SETUP='^    target_root_setup "$(pwd -P)" ||$'
fz_mutant raw "s#$FZ_SETUP#    true ||#;s|$FZ_KEPT|    INF_FUZZ_TARGET_DIR=\"\${CARGO_TARGET_DIR:-target}/fuzz-check\"|" \
    "skips the helper and reads CARGO_TARGET_DIR itself" "$FZ_CALLER_TARGET"
fz_mutant unkeyed "s|$FZ_KEPT|    INF_FUZZ_TARGET_DIR=\"\$TARGET_ROOT/fuzz-check\"|" "keeps an unkeyed directory"
fz_mutant warnings 's/^RUSTFLAGS = "-D warnings --cfg fuzzing"$/RUSTFLAGS = "--cfg fuzzing"/' "drops -D warnings"
fz_mutant cfg 's/^RUSTFLAGS = "-D warnings --cfg fuzzing"$/RUSTFLAGS = "-D warnings"/' "drops --cfg fuzzing"
fz_mutant encoded 's/ if k != "CARGO_ENCODED_RUSTFLAGS"}/}/' "keeps the caller's CARGO_ENCODED_RUSTFLAGS"

# C1: the scope.
root=$(fz_root c1-none)
rm -rf "${root:?}/crates/a/fuzz" "${root:?}/bins/b/fuzz"
fz_red "C1: no fuzz manifest is a scope error" "SCOPE ERROR: no fuzz manifest in the tree" "$root"
root=$(fz_root c1-rows)
fz_manifest "$root" crates/a
fz_manifest "$root" bins/b
fz_red "C1: no [[bin]] row in any fuzz manifest is a scope error" \
    "SCOPE ERROR: no .*bin.* row in any fuzz manifest" "$root"
# C2: every manifest is the root, a member, a fuzz manifest or a probe.
root=$(fz_root c2)
mkdir -p "$root/tools/x"
printf '[package]\nname = "x"\nversion = "0.0.0"\n' >"$root/tools/x/Cargo.toml"
fz_red "C2: an untracked crate no gate compiles" "C2 tools/x/Cargo.toml: a crate no gate compiles" "$root"
# C3: a fuzz manifest sits at <member>/fuzz with a workspace of its own.
root=$(fz_root c3-place)
mkdir -p "$root/crates/a/fuzz2"
sed 's/name = "a-fuzz"/name = "a-fuzz2"/' "$root/crates/a/fuzz/Cargo.toml" >"$root/crates/a/fuzz2/Cargo.toml"
fz_red "C3: a fuzz manifest outside <member>/fuzz" \
    "C3 crates/a/fuzz2/Cargo.toml: a fuzz manifest outside" "$root"
root=$(fz_root c3-workspace)
fz_sed '/^\[workspace\]$/d' "$root/crates/a/fuzz/Cargo.toml"
fz_red "C3: a fuzz manifest without its own [workspace]" \
    "C3 crates/a/fuzz/Cargo.toml: a fuzz manifest without a .workspace. of its own" "$root"
# ...and the root's licence, which it cannot inherit from outside the root's
# workspace (cargo deny reads it).
root=$(fz_root c3-licence)
printf '\n[workspace.package]\nlicense = "Apache-2.0"\n' >>"$root/Cargo.toml"
fz_sed 's/^version = "0.0.0"$/version = "0.0.0"\nlicense = "Apache-2.0"/' "$root/crates/a/fuzz/Cargo.toml"
fz_once "$root.log" fz_run "$root"
expect_red_because "fuzz-targets: C3: a fuzz manifest that states no licence" \
    "C3 bins/b/fuzz/Cargo.toml: a fuzz manifest whose license (none) is not the root's (Apache-2.0)" \
    fz_replay "$root.log"
expect green "fuzz-targets: C3: a fuzz manifest stating the root's licence" \
    fz_lacks "C3 crates/a/fuzz/Cargo.toml" "$root.log"
fz_sed 's/^version = "0.0.0"$/version = "0.0.0"\nlicense = "MIT"/' "$root/bins/b/fuzz/Cargo.toml"
fz_red "C3: a fuzz manifest stating another licence" \
    "C3 bins/b/fuzz/Cargo.toml: a fuzz manifest whose license (MIT) is not the root's" "$root"
fz_sed 's/^license = "MIT"$/license = "Apache-2.0"/' "$root/bins/b/fuzz/Cargo.toml"
expect green "fuzz-targets: every fuzz manifest stating the root's licence" fz_run "$root"
# C4: a probe row whose manifest is gone, or whose gate no recipe runs.
root=$(fz_root c4-gone)
rm "${root:?}/scripts/waker-atomics-probe/Cargo.toml"
fz_red "C4: a probe row whose manifest is gone" \
    "C4 scripts/waker-atomics-probe/Cargo.toml: a probe row whose manifest is gone" "$root"
root=$(fz_root c4-recipe)
fz_sed '/check-lint-scopes.sh/d' "$root/justfile"
fz_red "C4: a probe whose gate the check recipe does not run" \
    "C4 scripts/lint-scope-probe/Cargo.toml: its gate scripts/check-lint-scopes.sh is not run" "$root"
# ...or that names a third-party crate, whose lock no cargo-deny leg reads: a
# registry package in a lock it commits, a registry, git or workspace
# dependency, a target's dependency, a path out of the tree; a path to a
# manifest of the tree is the sanctioned shape.
root=$(fz_root c4-third-party)
printf '\n[[package]]\nname = "serde"\nversion = "1.0.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n' \
    >>"$root/scripts/fuzz-check-probe/Cargo.lock"
printf '\n[dependencies]\nserde = "1"\ngitdep = { git = "https://example.invalid/x" }\n' \
    >>"$root/scripts/clock-ban-probe/Cargo.toml"
printf "\n[target.'cfg(unix)'.dependencies]\nlibc = { version = \"0.2\" }\n" \
    >>"$root/scripts/waker-atomics-probe/Cargo.toml"
printf '\n[dependencies]\na = { path = "../../crates/a" }\nout = { path = "../../../out" }\n' \
    >>"$root/scripts/lint-scope-probe/Cargo.toml"
printf '\n[dev-dependencies]\nws = { workspace = true }\n' >>"$root/scripts/fuzz-check-probe/stale-lock/Cargo.toml"
fz_once "$root.log" fz_run "$root"
FZ_C4=": a probe that names"
expect_red_because "fuzz-targets: C4: a registry package in a probe's lock" \
    "C4 scripts/fuzz-check-probe/Cargo.toml$FZ_C4 .serde 1.0.0. from registry+https://github.com/rust-lang/crates.io-index in scripts/fuzz-check-probe/Cargo.lock" \
    fz_replay "$root.log"
expect_red_because "fuzz-targets: C4: a registry dependency of a probe" \
    "C4 scripts/clock-ban-probe/Cargo.toml$FZ_C4 .serde., a dependency not by path" fz_replay "$root.log"
expect_red_because "fuzz-targets: C4: a git dependency of a probe" \
    "C4 scripts/clock-ban-probe/Cargo.toml$FZ_C4 .gitdep., a dependency not by path" fz_replay "$root.log"
expect_red_because "fuzz-targets: C4: a target's dependency of a probe" \
    "C4 scripts/waker-atomics-probe/Cargo.toml$FZ_C4 .libc., a dependency not by path" fz_replay "$root.log"
expect_red_because "fuzz-targets: C4: a path dependency out of the tree" \
    "C4 scripts/lint-scope-probe/Cargo.toml$FZ_C4 .out., a dependency not by path" fz_replay "$root.log"
expect_red_because "fuzz-targets: C4: a workspace-inherited dependency of a probe" \
    "C4 scripts/fuzz-check-probe/stale-lock/Cargo.toml$FZ_C4 .ws., a dependency not by path" \
    fz_replay "$root.log"
expect green "fuzz-targets: C4: a probe's path dependency on a manifest of the tree" \
    fz_lacks "$FZ_C4 .a.," "$root.log"
expect_output "fuzz-targets: the OK line says no probe names a third-party crate" \
    "6 probe manifests, each run by its gate and naming no third-party crate" fz_replay "$FZ_CLEAN_LOG"
# C5: one row a file, at fuzz_targets/<name>.rs, each name once.
root=$(fz_root c5-file)
printf 'fn main() {}\n' >"$root/crates/a/fuzz/fuzz_targets/x.rs"
fz_red "C5: a file under fuzz_targets/ no row names" \
    "C5 crates/a/fuzz/fuzz_targets/x.rs: a file under fuzz_targets/ that no" "$root"
root=$(fz_root c5-row)
fz_manifest "$root" crates/a t_one t_gone
fz_red "C5: a row whose file is missing" "names fuzz_targets/t_gone.rs, which is not a listed file" "$root"
root=$(fz_root c5-path)
printf '\n[[bin]]\nname = "t_src"\npath = "src/t_src.rs"\n' >>"$root/crates/a/fuzz/Cargo.toml"
fz_red "C5: a row whose path is not fuzz_targets/<name>.rs" "at .src/t_src.rs., not fuzz_targets" "$root"
fz_answer "$root" crates/a/fuzz/Cargo.toml t_one t_src
fz_red "C5: a refused row cargo compiled is C5's alone, its dep-info unread" \
    "at .src/t_src.rs., not fuzz_targets" "$root"
root=$(fz_root c5-twice)
fz_manifest "$root" bins/b b_parse t_one
printf 'fn main() {}\n' >"$root/bins/b/fuzz/fuzz_targets/t_one.rs"
fz_red "C5: a target name in two workspaces" "target .t_one. is also a target of" "$root"
root=$(fz_root c5-orphan)
mkdir -p "$root/crates/c/fuzz/fuzz_targets"
printf 'fn main() {}\n' >"$root/crates/c/fuzz/fuzz_targets/y.rs"
fz_red "C5: a fuzz_targets/ file beside no fuzz manifest" "beside no fuzz manifest" "$root"
# C6: every seed directory names a target of its workspace; git's listing
# decides what exists, so an ignored local corpus is never read.
root=$(fz_root c6-stray)
mkdir -p "$root/crates/a/fuzz/corpora/no_such_target"
printf 'x' >"$root/crates/a/fuzz/corpora/no_such_target/seed"
fz_red "C6: corpora/<a name no target has>" \
    "C6 crates/a/fuzz/corpora/no_such_target: a seed directory that names no target" "$root"
root=$(fz_root c6-seeds)
mkdir -p "$root/bins/b/fuzz/seeds/gone"
printf 'x' >"$root/bins/b/fuzz/seeds/gone/seed"
fz_red "C6: seeds/gone" "C6 bins/b/fuzz/seeds/gone: a seed directory" "$root"
root=$(fz_root c6-ignored)
mkdir -p "$root/crates/a/fuzz/corpus/old" "$root/crates/a/fuzz/corpus/t_one"
printf 'x' >"$root/crates/a/fuzz/corpus/old/seed"
printf 'x' >"$root/crates/a/fuzz/corpus/t_one/seed"
printf 'crates/a/fuzz/corpus/old/\n' >"$root/.gitignore"
expect_output "fuzz-targets: an ignored local corpus is not read; a listed one is counted" \
    "1 seed directories (git-listed)" fz_run "$root"
# C7: GitHub's matrix cap.
root=$(fz_root c7)
names=t_one
i=0
while [ "$i" -lt 255 ]; do
    names="$names t_$i"
    printf 'fn main() {}\n' >"$root/crates/a/fuzz/fuzz_targets/t_$i.rs"
    i=$((i + 1))
done
# shellcheck disable=SC2086
fz_manifest "$root" crates/a $names
fz_red "C7: 257 targets are past one matrix" "C7 257 targets: more than one matrix holds" "$root"
# ...and the deny matrix, a row a fuzz manifest and the root's: 256 fuzz
# workspaces of one target each hold 256 targets, the cap, and 257 deny rows,
# one past it; 255 workspaces fill the deny matrix exactly.
# The workspaces are written by shell builtins under one mkdir, and the gate
# runs under a stub that answers nothing (a K2 red a workspace, beside C7),
# so the 256 workspaces cost no process each but that stub's.
root=$(fz_root c7-deny)
dirs=()
i=0
while [ "$i" -lt 254 ]; do
    dirs+=("$root/crates/m$i/fuzz/fuzz_targets")
    i=$((i + 1))
done
mkdir -p "${dirs[@]}"
i=0
while [ "$i" -lt 254 ]; do
    printf '[package]\nname = "m%s"\nversion = "0.0.0"\n' "$i" >"$root/crates/m$i/Cargo.toml"
    printf '[package]\nname = "m%s-fuzz"\nversion = "0.0.0"\n\n[package.metadata]\ncargo-fuzz = true\n\n[workspace]\n\n[[bin]]\nname = "m%s_t"\npath = "fuzz_targets/m%s_t.rs"\n' \
        "$i" "$i" "$i" >"$root/crates/m$i/fuzz/Cargo.toml"
    printf 'fn main() {}\n' >"$root/crates/m$i/fuzz/fuzz_targets/m${i}_t.rs"
    i=$((i + 1))
done
printf '#!/bin/sh\nexit 0\n' >"$work/fz-silent-cargo"
chmod +x "$work/fz-silent-cargo"
FZ_C7D="C7 257 cargo-deny manifests (the root and 256 fuzz manifests): more than one matrix holds"
expect_red_because "fuzz-targets: C7: 257 deny rows are past one matrix, in the gate" "$FZ_C7D" \
    env INF_CHECK_ROOT="$root" INF_FUZZ_CHECK_PROBE=off INF_FUZZ_CARGO="$work/fz-silent-cargo" "$FUZZ"
expect_red_because "fuzz-targets: C7: 257 deny rows are past one matrix, in --matrix deny" "$FZ_C7D" \
    env INF_CHECK_ROOT="$root" "$FUZZ" --matrix deny
expect green "fuzz-targets: C7: no deny matrix past the cap" fz_matrix_red "$root" deny
expect green "fuzz-targets: C7: a red census prints no fuzz matrix, its 256 targets fitting" \
    fz_matrix_red "$root" nightly
rm -rf "${root:?}/crates/m0"
fz_deny_rows() { # <root> <rows>: --matrix deny is green with exactly that many rows
    local got
    got=$(env INF_CHECK_ROOT="$1" "$FUZZ" --matrix deny) || return 1
    [ "$(printf '%s' "$got" | grep -o '"manifest"' | wc -l | tr -d ' ')" = "$2" ]
}
expect green "fuzz-targets: C7: 256 deny rows fill one matrix" fz_deny_rows "$root" 256

# H1–H3: a target reaches production code through its crate (ADR-0198 D4).
fz_h_root() { # <name> <parse.rs addition>: the clean root, parse.rs extended
    local hroot
    hroot=$(fz_root "$1")
    printf '%s\n' "$2" >>"$hroot/bins/b/src/parse.rs"
    echo "$hroot"
}
root=$(fz_root h1)
printf '#[path = "../../src/lib.rs"]\nmod a;\n\nfn main() {}\n' >"$root/crates/a/fuzz/fuzz_targets/t_one.rs"
fz_red "H1: a #[path] into a package with a library target" \
    "from .crates/a., a package with a library target" "$root"
root=$(fz_root h1-outside)
printf '#[path = "../../../../outside.rs"]\nmod a;\n\nfn main() {}\n' >"$root/crates/a/fuzz/fuzz_targets/t_one.rs"
fz_red "H1: a #[path] out of the tree" "which is not a file of this tree" "$root"
root=$(fz_h_root h2-crate 'pub fn g() -> u8 { crate::parse::f() }')
fz_red "H2: an included file naming crate::" "bins/b/src/parse.rs:14 names .crate::." "$root"
root=$(fz_h_root h2-super 'pub fn g() -> u8 { super::parse::f() }')
fz_red "H2: an included file naming super::" "names .super::." "$root"
# An alias names the root without `::` after the keyword; rustc accepts each.
root=$(fz_h_root h2-crate-alias 'use crate as root;
pub fn g() -> u8 { root::parse::f() }')
fz_red "H2: an included file aliasing crate" "parse.rs:14 aliases .crate. or .super. (.crate as.)" "$root"
root=$(fz_h_root h2-super-alias 'use super as up;')
fz_red "H2: an included file aliasing super" "parse.rs:14 aliases .crate. or .super. (.super as.)" "$root"
root=$(fz_h_root h2-extern-self 'extern crate self as root;')
fz_red "H2: an included file naming the root by extern crate self" \
    "parse.rs:14 aliases .crate. or .super. (.extern crate self.)" "$root"
root=$(fz_h_root h2-mod 'mod helper;')
fz_red "H2: an included file declaring a module by file" "declares a module by file without" "$root"
root=$(fz_h_root h2-include 'include!("other.rs");')
fz_red "H2: an included file calling include!" "parse.rs:14 calls .include!." "$root"
root=$(fz_h_root h2-deep '#[path = "deep.rs"]
mod deep;')
printf '#[path = "deeper.rs"]\nmod deeper;\n' >"$root/bins/b/src/deep.rs"
printf 'pub fn h() {}\n' >"$root/bins/b/src/deeper.rs"
fz_red "H2: a third level of #[path]" "a third level of" "$root"
root=$(fz_root h2-shape)
printf '#[cfg_attr(fuzzing, path = "../../src/parse.rs")]\nmod parse;\n\nfn main() {}\n' \
    >"$root/bins/b/fuzz/fuzz_targets/b_parse.rs"
fz_red "H2: a #[path] shape the gate does not read" "the gate does not read" "$root"
root=$(fz_root h3-macro)
printf 'macro_rules! bound {\n    () => {\n        1\n    };\n}\n\n%s' "$FZ_TARGET" \
    >"$root/bins/b/fuzz/fuzz_targets/b_parse.rs"
fz_red "H3: an including target defining macro_rules!" "defines a .macro_rules!. and includes" "$root"
root=$(fz_root h3-macro-use)
printf '#[macro_use]\n%s' "$FZ_TARGET" >"$root/bins/b/fuzz/fuzz_targets/b_parse.rs"
fz_red "H3: an including target carrying #[macro_use]" "carries a .#.macro_use.. and includes" "$root"
root=$(fz_root h3-include)
printf '%s\ninclude!("../../src/parse.rs");\n' "$FZ_TARGET" >"$root/bins/b/fuzz/fuzz_targets/b_parse.rs"
fz_red "H3: an including target calling include!" "b_parse.rs:8 calls .include!. (or names it)" "$root"
# include! is red in every target, not only in one that includes by #[path]:
# a library's file compiled that way resolves crate:: to the fuzz crate.
root=$(fz_root h3-include-alone)
printf 'mod copied {\n    include!("../../src/lib.rs");\n}\n\nfn main() {}\n' \
    >"$root/crates/a/fuzz/fuzz_targets/t_one.rs"
fz_red "H3: a target with no #[path] calling include! of a library file" \
    "t_one: crates/a/fuzz/fuzz_targets/t_one.rs:2 calls .include!." "$root"
root=$(fz_root h3-include-spaced)
printf 'mod copied {\n    core::include !("../../src/lib.rs");\n}\n\nfn main() {}\n' \
    >"$root/crates/a/fuzz/fuzz_targets/t_one.rs"
fz_red "H3: include! spelled with a path and a space" "t_one.rs:2 calls .include!." "$root"
root=$(fz_root h3-include-alias)
printf 'use core::include as copy;\n\nmod copied {\n    super::copy!("%s");\n}\n\nfn main() {}\n' \
    ../../src/lib.rs >"$root/crates/a/fuzz/fuzz_targets/t_one.rs"
fz_red "H3: include! called through an alias" "t_one.rs:1 calls .include!. (or names it)" "$root"

# K1–K2: cargo's status and its report, against the [[bin]] rows.
fz_k_root() { # <name>: the clean root with a second target t_two in crates/a
    local kroot
    kroot=$(fz_root "$1")
    fz_manifest "$kroot" crates/a t_one t_two
    printf 'fn main() {}\n' >"$kroot/crates/a/fuzz/fuzz_targets/t_two.rs"
    echo "$kroot"
}
root=$(fz_k_root k1)
fz_answer "$root" crates/a/fuzz/Cargo.toml t_one
fz_error "$root" crates/a/fuzz/Cargo.toml t_two E0433 'cannot find module or crate `gone`' \
    fuzz_targets/t_two.rs >>"$root.stub/$(fz_key crates/a/fuzz/Cargo.toml).out"
fz_status "$root" crates/a/fuzz/Cargo.toml 101
fz_once "$root.log" fz_run "$root"
expect_red_because "fuzz-targets: K1: cargo exits non-zero" \
    "K1 crates/a/fuzz/Cargo.toml: cargo exited 101" fz_replay "$root.log"
expect_output "fuzz-targets: K1 prints the count line too" \
    "crates/a/fuzz/Cargo.toml: compiled 1 of 2 (missing: t_two)" fz_replay "$root.log"
expect_output "fuzz-targets: K1 prints each error under its workspace and target" \
    "crates/a/fuzz/Cargo.toml bin t_two:" fz_replay "$root.log"
FZ_KROOT=$root  # the K cases below rewrite its stub's answer, so they share it
root=$(fz_root k1-unread)
printf 'Compiling a-fuzz\n' >>"$root.stub/$(fz_key crates/a/fuzz/Cargo.toml).out"
fz_red "K1: a report line that is not cargo's JSON" "1 report lines unread" "$root"
# cargo's own error prints whole, its Caused by: chain and help: lines up to
# the next status line, and a long block's middle is counted, not printed.
root=$(fz_root k1-block)
fz_status "$root" crates/a/fuzz/Cargo.toml 101
fz_stderr "$root" crates/a/fuzz/Cargo.toml "error: failed to run custom build command for \`libfuzzer-sys v0.4.13\`

Caused by:
  process didn't exit successfully: \`build-script-build\` (exit status: 1)
  --- stderr
  error occurred in cc-rs: failed to find tool \"c++\": No such file or directory (os error 2)

    Checking a-fuzz v0.0.0
warning: build failed, waiting for other jobs to finish..."
fz_once "$root.log" fz_run "$root"
FZ_K1_BLOCK_LOG=$root.log  # read again after the refused-lock cases
expect_red_because "fuzz-targets: K1 prints cargo's Caused by: chain" "^    Caused by:$" \
    fz_replay "$root.log"
expect_output "fuzz-targets: K1 prints a build script's reason" \
    "^      error occurred in cc-rs: failed to find tool" fz_replay "$root.log"
expect green "fuzz-targets: cargo's block ends at its next status line" \
    fz_lacks "Checking a-fuzz" "$root.log"
root=$(fz_root k1-long)
fz_status "$root" crates/a/fuzz/Cargo.toml 101
long="error: failed to run custom build command for \`x\`"
i=1
while [ "$i" -le 40 ]; do
    long="$long
  line $i"
    i=$((i + 1))
done
fz_stderr "$root" crates/a/fuzz/Cargo.toml "$long"
fz_once "$root.log" fz_run "$root"
expect_output "fuzz-targets: a long cargo block counts what it omits" "^    \.\.\. 17 lines omitted$" \
    fz_replay "$root.log"
expect_output "fuzz-targets: a long cargo block keeps its last line" "^      line 40$" fz_replay "$root.log"
expect green "fuzz-targets: a long cargo block omits its middle" fz_lacks "line 20$" "$root.log"
# A lock cargo refuses: its help: line, and the refresh that adds what it lacks.
root=$(fz_root k1-lock)
fz_status "$root" crates/a/fuzz/Cargo.toml 101
fz_stderr "$root" crates/a/fuzz/Cargo.toml "error: cannot update the lock file $root/crates/a/fuzz/Cargo.lock because --locked was passed to prevent this
help: to generate the lock file without accessing the network, remove the --locked flag and use --offline instead."
fz_once "$root.log" fz_run "$root"
expect_red_because "fuzz-targets: K1 prints cargo's help: line on a refused lock" \
    "^    help: to generate the lock file without accessing the network" fz_replay "$root.log"
expect_output "fuzz-targets: K1 names the refresh of a refused lock" \
    "^crates/a/fuzz/Cargo.toml fix: cargo update --workspace --offline --manifest-path crates/a/fuzz/Cargo.toml adds what the lock lacks and updates a pinned package only when the manifests no longer admit it" \
    fz_replay "$root.log"
expect_output "fuzz-targets: K1 names the lock to commit" "then commit crates/a/fuzz/Cargo.lock$" \
    fz_replay "$root.log"
expect green "fuzz-targets: no refresh is named for another cargo error" fz_lacks " fix: cargo update" \
    "$FZ_K1_BLOCK_LOG"
fz_answer "$FZ_KROOT" crates/a/fuzz/Cargo.toml t_one
fz_once "$FZ_KROOT.log" fz_run "$FZ_KROOT"
expect_red_because "fuzz-targets: K2: cargo exits 0 having compiled one of two bins" \
    "K2 crates/a/fuzz/Cargo.toml: the bins cargo compiled are not its" fz_replay "$FZ_KROOT.log"
expect_output "fuzz-targets: K2 names the missing bin" "compiled 1 of 2 (missing: t_two)" \
    fz_replay "$FZ_KROOT.log"
fz_answer "$FZ_KROOT" crates/a/fuzz/Cargo.toml
fz_red "K2: cargo exits 0 printing no artifact" "compiled 0 of 2 (missing: t_one, t_two)" \
    "$FZ_KROOT"
root=$(fz_root k2-extra)
fz_answer "$root" crates/a/fuzz/Cargo.toml t_one t_extra
fz_red "K2: a bin no row names" "(unexpected: t_extra)" "$root"

# K3: rustc's dep-info of a compiled target names only the target's file and
# the files the census read for it by `#[path]`; a file it reached another way
# (a dependency's macro calling include! from the target's call site) is red,
# and so is a compiled target whose dep-info the gate cannot find or read.
root=$(fz_root k3)
fz_depinfo "$root" t_one fuzz_targets/t_one.rs fuzz_targets/../../src/lib.rs
fz_once "$root.log" fz_run "$root"
expect_red_because "fuzz-targets: K3: a target compiles a file the gate did not read" \
    "K3 crates/a/fuzz/Cargo.toml t_one: rustc compiled crates/a/src/lib.rs into it" \
    fz_replay "$root.log"
expect green "fuzz-targets: K3 counts a #[path] file the census read as read" \
    fz_lacks "K3 bins/b/fuzz/Cargo.toml" "$root.log"
root=$(fz_root k3-absolute)
fz_depinfo "$root" t_one "$root/crates/a/fuzz/fuzz_targets/t_one.rs"
expect green "fuzz-targets: K3 reads an absolute dep-info path" fz_run "$root"
root=$(fz_root k3-missing)
rm -f "${root:?}.stub/deps/t_one-0.d"
fz_red "K3: a compiled target with no dep-info" \
    "K3 crates/a/fuzz/Cargo.toml t_one: no dep-info at" "$root"
root=$(fz_root k3-unnamed)
printf '{"reason":"compiler-artifact","manifest_path":"%s/%s","target":{"kind":["bin"],"name":"t_one"},"fresh":false}\n' \
    "$root" crates/a/fuzz/Cargo.toml >"$root.stub/$(fz_key crates/a/fuzz/Cargo.toml).out"
fz_red "K3: an artifact that names no metadata" \
    "K3 crates/a/fuzz/Cargo.toml t_one: no dep-info (its artifact names no .rmeta)" "$root"

# P1–P5: the probe on, the stub answering its two manifests as the toolchain
# does (fz_probe <root> [<plant>]: the true answer, or that plant's escape).
FZ_PM=scripts/fuzz-check-probe/Cargo.toml
FZ_PL=scripts/fuzz-check-probe/stale-lock/Cargo.toml
fz_probe() {
    local root=$1 plant=${2:-none} out
    out="$root.stub/$(fz_key "$FZ_PM").out"
    : >"$out"
    case $plant in
    p1-compiled) fz_artifact "$root" "$FZ_PM" plant_crate >>"$out" ;;
    p1-code) fz_error "$root" "$FZ_PM" plant_crate E0425 'cannot find value `BOUND`' \
        fuzz_targets/../shared/leaf.rs >>"$out" ;;
    p1-file) fz_error "$root" "$FZ_PM" plant_crate E0433 'cannot find module or crate `absent_crate`' \
        fuzz_targets/plant_crate.rs >>"$out" ;;
    *) fz_error "$root" "$FZ_PM" plant_crate E0433 \
        'cannot find module or crate `absent_crate` in this scope' fuzz_targets/../shared/leaf.rs >>"$out" ;;
    esac
    case $plant in
    p2-compiled) fz_artifact "$root" "$FZ_PM" plant_warning >>"$out" ;;
    p2-code) fz_error "$root" "$FZ_PM" plant_warning E0308 'mismatched types' \
        fuzz_targets/plant_warning.rs >>"$out" ;;
    *) fz_error "$root" "$FZ_PM" plant_warning unused_imports \
        'unused import: `std::collections::BTreeMap`' fuzz_targets/plant_warning.rs >>"$out" ;;
    esac
    case $plant in
    p5-compiled) fz_artifact "$root" "$FZ_PM" plant_cfg >>"$out" ;;
    p5-crate) fz_error "$root" "$FZ_PM" plant_cfg E0433 'cannot find module or crate `other_crate`' \
        fuzz_targets/plant_cfg.rs >>"$out" ;;
    *) fz_error "$root" "$FZ_PM" plant_cfg E0433 \
        'cannot find module or crate `absent_cfg_crate` in this scope' fuzz_targets/plant_cfg.rs >>"$out" ;;
    esac
    [ "$plant" = p3-missing ] || fz_artifact "$root" "$FZ_PM" control >>"$out"
    [ "$plant" = p3-depinfo ] || [ "$plant" = p3-missing ] \
        || fz_depinfo "$root" control fuzz_targets/control.rs fuzz_targets/../shared/ok.rs
    fz_status "$root" "$FZ_PM" 101
    case $plant in
    p4-compiled) fz_answer "$root" "$FZ_PL" stale_lock ;;
    p4-other)
        fz_answer "$root" "$FZ_PL"
        fz_status "$root" "$FZ_PL" 101
        fz_stderr "$root" "$FZ_PL" "error: failed to parse manifest at \`$root/$FZ_PL\`" ;;
    *)
        fz_answer "$root" "$FZ_PL"
        fz_status "$root" "$FZ_PL" 101
        fz_stderr "$root" "$FZ_PL" "error: cannot update the lock file $root/${FZ_PL%Cargo.toml}Cargo.lock because --locked was passed to prevent this" ;;
    esac
}
root=$(fz_root probe)
fz_probe "$root"
expect_output "fuzz-targets: the probe's five verdicts on the OK line" \
    "probe: plant_crate E0433 absent_crate, plant_warning denied, plant_cfg E0433 absent_cfg_crate, control compiled naming shared/ok.rs, stale_lock refused by --locked;" \
    fz_probe_run "$root"
# The probe's directory: one a run, made by mktemp -d under the kept directory
# and gone at exit, while the census workspaces build in the kept one.
FZ_PKEPT="$(cd "$root" && pwd -P)/target/fuzz-check-$(fx_root_key "$root")"
fz_probe_dirs() { # <root> <kept>: prints one run's probe directory; red unless it is per run
    rm -f "$1.stub/calls"
    fz_probe_run "$1" >/dev/null 2>&1 || return 1
    python3 - "$1.stub/calls" "$2" <<'PY'
import os
import re
import sys
lines = open(sys.argv[1], encoding="utf-8").read().split("\n")
calls = [lines[i].split(" ")[1:] for i in range(0, len(lines) - 1, 3)]
probe = {"scripts/fuzz-check-probe/Cargo.toml", "scripts/fuzz-check-probe/stale-lock/Cargo.toml"}
dirs, bad, called = set(), [], set()
for argv in calls:
    where = argv[argv.index("--target-dir") + 1] if "--target-dir" in argv else None
    called.add(argv[2])
    if argv[2] in probe:
        dirs.add(where)
    elif where != sys.argv[2]:
        bad.append(f"{argv[2]} builds in {where}, not the kept {sys.argv[2]}")
pattern = re.compile(re.escape(sys.argv[2]) + r"/probe\.[A-Za-z0-9]{6}")
if not probe <= called:
    bad.append(f"the probe's manifests were not both called: {sorted(called)}")
if len(dirs) != 1 or not all(d and pattern.fullmatch(d) for d in dirs):
    bad.append(f"the probe builds in {sorted(map(str, dirs))}, not one mktemp directory under the kept one")
bad += [f"the probe's directory {d} outlived the run" for d in dirs if d and os.path.exists(d)]
print("\n".join(bad) if bad else dirs.pop())
sys.exit(1 if bad else 0)
PY
}
fz_probe_per_run() { # <root> <kept>: two runs, two probe directories
    local first second
    first=$(fz_probe_dirs "$1" "$2") || { echo "$first"; return 1; }
    second=$(fz_probe_dirs "$1" "$2") || { echo "$second"; return 1; }
    [ "$first" != "$second" ] || { echo "both runs built the probe in $first"; return 1; }
}
expect green "fuzz-targets: the probe builds in a directory of its own each run, gone at exit" \
    fz_probe_per_run "$root" "$FZ_PKEPT"
fz_probe_mutant() { # <name> <sed expression> <what the copy does>
    local dir="$work/fz-probe-gate-$1" FUZZ
    rm -rf "${dir:?}"
    mkdir -p "$dir"
    cp "$SCRIPT_DIR/check-fuzz-targets.sh" "$SCRIPT_DIR/fuzz_targets.py" "$SCRIPT_DIR/lint_scope_table.py" \
        "$SCRIPT_DIR/strip-test-modules.awk" "$SCRIPT_DIR/target-root.sh" "$dir/"
    fz_sed "$2" "$dir/check-fuzz-targets.sh"
    chmod +x "$dir/check-fuzz-targets.sh"
    if cmp -s "$SCRIPT_DIR/check-fuzz-targets.sh" "$dir/check-fuzz-targets.sh"; then
        echo "selftest: the fuzz-targets probe mutant '$1' changed nothing" >&2
        exit 2
    fi
    FUZZ="$dir/check-fuzz-targets.sh"
    expect red "fuzz-targets: the probe's directory check is red on a gate that $3" \
        fz_probe_per_run "$root" "$FZ_PKEPT"
}
fz_probe_mutant fixed 's|^\(        INF_FUZZ_PROBE_DIR=\)$(mktemp -d "$INF_FUZZ_TARGET_DIR/probe.XXXXXX")$|\1"$INF_FUZZ_TARGET_DIR/probe.fixed1"; mkdir -p "$INF_FUZZ_PROBE_DIR"|' \
    "builds the probe in a fixed directory"
fz_probe_mutant kept 's|^    PROBE_TARGET = \["--target-dir", os.environ\["INF_FUZZ_PROBE_DIR"\]\]$|    PROBE_TARGET = KEPT|' \
    "builds the probe in the kept directory"
FZ_PROOT=$root  # fz_probe rewrites both probe answers, so the plants share it
fz_plant() { # <plant> <label> <pattern>
    fz_probe "$FZ_PROOT" "$1"
    expect_red_because "fuzz-targets: $2" "$3" fz_probe_run "$FZ_PROOT"
}
fz_plant p1-compiled "P1: plant_crate compiled" "P1 probe target plant_crate compiled"
fz_plant p1-code "P1: plant_crate refused for another code" "P1 probe target plant_crate refused without"
fz_plant p1-file "P1: plant_crate's E0433 outside shared/leaf.rs" "P1 probe target plant_crate refused without"
fz_plant p2-compiled "P2: plant_warning compiled" "P2 probe target plant_warning compiled"
fz_plant p2-code "P2: plant_warning refused for another reason" "P2 probe target plant_warning refused without"
fz_plant p3-missing "P3: no control artifact" "P3 probe target control not compiled"
fz_plant p3-depinfo "P3: control's dep-info without the file it includes" \
    "P3 probe target control's dep-info names"
fz_plant p4-compiled "P4: a stale lock compiled" "P4 probe target stale_lock compiled, or refused without"
fz_plant p4-other "P4: stale_lock refused without --locked's message" \
    "P4 probe target stale_lock compiled, or refused without"
fz_plant p5-compiled "P5: plant_cfg compiled" "P5 probe target plant_cfg compiled"
fz_plant p5-crate "P5: plant_cfg's E0433 naming another crate" "P5 probe target plant_cfg refused without"
# The main probe's errors are printed for its own red, not for P4's.
fz_probe "$FZ_PROOT" p4-compiled
fz_once "$FZ_PROOT.log" fz_probe_run "$FZ_PROOT"
expect green "fuzz-targets: a P4 red prints none of the main probe's planted errors" \
    fz_lacks "^scripts/fuzz-check-probe/Cargo.toml bin plant_crate:" "$FZ_PROOT.log"
fz_probe "$FZ_PROOT" p1-compiled
fz_once "$FZ_PROOT.log" fz_probe_run "$FZ_PROOT"
expect_output "fuzz-targets: a P1 red prints the main probe's errors" \
    "^scripts/fuzz-check-probe/Cargo.toml bin plant_warning:" fz_replay "$FZ_PROOT.log"
# A run without --locked rewrites stale-lock's lock to the manifest's version:
# that red names the restore, not the gate's flags, and compiles nothing.
root=$(fz_root probe-lock-rewritten)
fz_probe "$root"
fz_sed 's/^version = "0.0.0"$/version = "0.1.0"/' "$root/${FZ_PL%Cargo.toml}Cargo.lock"
fz_once "$root.log" fz_probe_run "$root"
expect_red_because "fuzz-targets: P4: a rewritten stale lock names its restore" \
    "names fuzz-check-stale-lock at 0.1.0, the version its manifest says, so it plants no stale lock: a cargo run without --locked rewrote it; restore it with .git checkout -- scripts/fuzz-check-probe/stale-lock/Cargo.lock." \
    fz_replay "$root.log"
expect green "fuzz-targets: a rewritten stale lock blames no flag" \
    fz_lacks "the gate does not pass --locked" "$root.log"
expect green "fuzz-targets: a rewritten stale lock prints no main probe error" \
    fz_lacks "^scripts/fuzz-check-probe/Cargo.toml bin" "$root.log"
expect green "fuzz-targets: a rewritten stale lock is not compiled" \
    fz_lacks "manifest-path $FZ_PL" "$root.stub/calls"
root=$(fz_root probe-short)
fz_probe "$root"
fz_sed '/^name = "plant_cfg"$/,/^path = /d' "$root/$FZ_PM"
expect_red_because "fuzz-targets: an edited-down probe" "an edited-down probe proves less" fz_probe_run "$root"

# The matrix mode: one row per target in census order with its seed
# directories, or (deny) the root and every fuzz manifest; a red census
# prints nothing on stdout and exits 1.
root=$(fz_root matrix)
for kind in corpora seeds; do
    mkdir -p "$root/crates/a/fuzz/$kind/t_one"
    printf 'x' >"$root/crates/a/fuzz/$kind/t_one/seed"
done
FZ_ROWS='{"include":[{"dir":"bins/b","target":"b_parse","seeds":""},{"dir":"crates/a","target":"t_one","seeds":"fuzz/corpora/t_one fuzz/seeds/t_one"}]}'
expect green "fuzz-targets: --matrix nightly prints every target with its seeds" \
    fz_matrix_is "$root" nightly "$FZ_ROWS"
expect green "fuzz-targets: --matrix smoke prints the same rows" fz_matrix_is "$root" smoke "$FZ_ROWS"
FZ_DENY='{"include":[{"manifest":"Cargo.toml"},{"manifest":"bins/b/fuzz/Cargo.toml"},{"manifest":"crates/a/fuzz/Cargo.toml"}]}'
expect green "fuzz-targets: --matrix deny prints the root and every fuzz manifest" \
    fz_matrix_is "$root" deny "$FZ_DENY"
fz_matrix_knob() { # <root> <rows>: the matrix mode compiles nothing, so it reads no probe knob
    local got
    got=$(env INF_CHECK_ROOT="$1" INF_FUZZ_CHECK_PROBE=no "$FUZZ" --matrix nightly) || return 1
    [ "$got" = "$2" ]
}
expect green "fuzz-targets: the matrix mode reads no probe knob" fz_matrix_knob "$root" "$FZ_ROWS"
mkdir -p "$root/crates/a/fuzz/corpora/gone"
printf 'x' >"$root/crates/a/fuzz/corpora/gone/seed"
expect green "fuzz-targets: a red census prints no matrix and exits 1" fz_matrix_red "$root"
expect green "fuzz-targets: a red census prints no deny matrix and exits 1" fz_matrix_red "$root" deny
fz_matrix_scope() { # <root>: a scope error in matrix mode, its reason on stderr alone
    local got status=0
    got=$(env INF_CHECK_ROOT="$1" "$FUZZ" --matrix nightly 2>"$1.stderr") || status=$?
    [ "$status" -eq 1 ] && [ -z "$got" ] \
        && grep -q 'FUZZ-TARGETS SCOPE ERROR: no fuzz manifest in the tree' "$1.stderr"
}
root=$(fz_root matrix-scope)
rm -rf "${root:?}/crates/a/fuzz" "${root:?}/bins/b/fuzz"
expect green "fuzz-targets: a matrix scope error prints no matrix, its reason on stderr" \
    fz_matrix_scope "$root"
expect green "fuzz-targets: an empty census prints no deny matrix" fz_matrix_red "$root" deny
expect_red_because "fuzz-targets: an unknown matrix" "usage: check-fuzz-targets.sh" \
    env INF_CHECK_ROOT="$root" "$FUZZ" --matrix weekly
}

group_fuzz_matrices() {
FUZZ=./scripts/check-fuzz-targets.sh
# The workflows and the local deny recipe read the census (ADR-0198 D3, D6):
# every fuzz run and every cargo-deny call, each judged by its own rule
# wherever it sits (a step holding both is red), is in a job whose matrix is
# `${{ fromJSON(needs.<census job>.outputs.matrix) }}` alone, the census job
# prints that matrix with `check-fuzz-targets.sh --matrix <mode>` in the
# assignment shape (an inline `$(…)` in the echo drops its status) at the
# commit the matrix job checks out, each fuzz call names the row's target and
# its step the row's seeds and directory, each cargo-deny call the row's
# manifest; the nightly, the full smoke and PR CI's cargo-deny each read their
# mode; PR CI's one-target smoke, `resp_parse`, is the one literal (D3), and
# its step runs no cargo-deny; and the justfile runs cargo-deny only in
# `deny`, each call over `--matrix deny`'s row. A list kept by hand beside the
# census would pass every other gate. Stdlib python, a line-and-indent reader
# of these files' layout that fails closed, as legs_cover: it judges a call
# written unquoted on one line of a step (jobs at two columns, steps at
# `      - `) or of a recipe, and any other spelling of a fuzz run or of
# cargo-deny (the reader's first comment lists them), in any case or quoting,
# folded or continued across lines, is red: a call it cannot place. It
# strips a comment only where YAML or just does: a block scalar's
# content and a recipe's body are read whole, because a quote the line does
# not show (open across lines, nested in `$(…)`) decides where a shell's
# comment starts, so a call spelled in a script's comment is red too. A line
# it cannot read is red as well: a YAML anchor, alias or tag (an alias moves a
# call to a line judged as something else), an escape in a double-quoted
# scalar, a quote left open at the line's end outside a block or a recipe
# body, a block indentation indicator. A job id or a `name:` runs nothing.
# Review holds what text cannot read: a call in another file a step or a
# recipe runs or loads (a script, an action, an imported justfile, a cargo
# alias), and one a shell or an expression assembles at run time (a variable,
# `${{ … }}` or `{{ … }}`, quoting or escaping inside a word); following
# either would make this reader a second shell or expression engine.
census_matrices() { # <root holding .github/workflows and justfile>
    python3 - "$1" <<'PY'
import bisect, os, re, sys
root, TOKEN = sys.argv[1], "CENSUS-MATRICES"
wf_dir = os.path.join(root, ".github/workflows")
files = sorted(f for f in os.listdir(wf_dir) if f.endswith((".yml", ".yaml"))) \
    if os.path.isdir(wf_dir) else []
if not files or not os.path.isfile(os.path.join(root, "justfile")):
    sys.exit(f"{TOKEN} SCOPE ERROR: no workflow under {wf_dir}, or no justfile")
# One spelling of each call, however cargo is invoked and in any case: what the
# reader counts on a line is what its rules judge. A fuzz run is `fuzz run` or
# its alias `fuzz r`, after any cargo option or as the cargo-fuzz binary (a
# YAML key `run:` after a line ending in `fuzz` is none). A cargo-deny call is
# `cargo-deny` (the binary, the action's name) or `cargo`, its options (a word
# led by `-` or `+`, each with at most one value word), then the word `deny`.
VALUE = r"""(?:"[^"]*"|'[^']*'|[^\s;&|()<>"'`])+"""
FUZZ = r"(?i:\bfuzz(?:\.exe)?\s+(?:run|r)\b)(?!:)"
DENY = (r"(?i:\bcargo-deny\b|\bcargo(?:\.exe)?(?:\s+[-+][^\s;&|()<>]*(?:\s+(?![-+])"
        + VALUE + r")?)*\s+deny(?=$|[\s;&|()<>\"'`]))")
FUZZ_RUN, DENY_RUN = re.compile(FUZZ), re.compile(DENY)
FUZZ_CALL = re.compile(FUZZ + r"\s+(\S+)")
ROW_CALL = re.compile(FUZZ + r"\s+\$\{\{ matrix\.target \}\}(?=\s|$)")
ROW_DENY = re.compile(DENY + r"\s+--manifest-path\s+\$\{\{ matrix\.manifest \}\}(?=\s|$)")
RECIPE_DENY = re.compile(DENY + r'\s+--manifest-path\s+"\$manifest"(?=\s|$)')
DENY_USES, ROW_MANIFEST = "EmbarkStudios/cargo-deny-action", "${{ matrix.manifest }}"
SPELLED = (("a fuzz run", FUZZ_RUN), ("a cargo-deny call", DENY_RUN))
NAME = {what: float("inf") for what, _ in SPELLED}  # a job id or a name runs nothing
BLOCK = re.compile(r"(\s*)((?:-\s+)*)(?:(\S.*?):\s+)?[|>]([-+0-9]*)")
MATRIX = re.compile(r"\$\{\{ fromJSON\(needs\.([A-Za-z0-9_-]+)\.outputs\.matrix\) \}\}")
CENSUS = re.compile(r"matrix=\$\(\./scripts/check-fuzz-targets\.sh --matrix (nightly|smoke|deny)\)")
WRITE = 'echo "matrix=$matrix" >> "$GITHUB_OUTPUT"'
PR_SMOKE = ("infinity-ci.yml", "fuzz-smoke", "resp_parse")
EXPECTED = {("infinity-fuzz-nightly.yml", "nightly"), ("infinity-ci-full.yml", "smoke"),
            ("infinity-ci.yml", "deny")}
errors, read, found, pr_smoke = [], [], set(), 0


def command(line):
    """A shell line with its comment and quoted strings blanked: what it runs."""
    line = re.sub(r"\"[^\"]*\"|'[^']*'", '""', line)
    return re.sub(r"(^|\s)#.*", "", line)


def yaml_text(line):
    """A workflow line outside a block scalar: its text without its YAML
    comment, and what in it the reader cannot place. YAML opens a quoted
    scalar only where a node starts (a line's start, after `- `, `? `, `: `,
    `[`, `{` or `,`), so an apostrophe inside a plain scalar is text; a node
    property (`&anchor`, `*alias`, `!tag`) can move a call where the reader
    does not judge it; an escape can spell one the text does not show; and a
    quoted scalar open at the line's end spans lines."""
    quote, start, why, i = None, True, None, 0
    while i < len(line):
        c, after = line[i], line[i + 1:i + 2]
        if quote == '"' and c == "\\":
            why, i = why or "an escape in a double-quoted YAML scalar", i + 1
        elif quote == "'" and c == "'" and after == "'":
            i += 1
        elif quote:
            quote = None if c == quote else quote
        elif c == "#" and (i == 0 or line[i - 1].isspace()):
            return line[:i], why
        elif c == "$" and line[i + 1:i + 3] == "{{":  # an expression is not a flow mapping
            end = line.find("}}", i)
            i, start = (len(line) if end < 0 else end + 1), False
        elif start and c in "\"'":
            quote, start = c, False
        elif start and c in "&*!":
            why, start = why or "a YAML anchor, alias or tag", False
        elif c in "-?" and start and after in ("", " ", "\t"):
            pass
        elif c in ":[{," and (c != ":" or after in ("", " ", "\t")):
            start = True
        elif not c.isspace():
            start = False
        i += 1
    return line, why or ("a quote left open at the line's end" if quote else None)


def just_text(line):
    """A justfile line outside a recipe body: its text without its comment,
    and why the reader cannot place it: a string open at the line's end
    spans lines."""
    quote, i = None, 0
    while i < len(line):
        c = line[i]
        if quote == '"' and c == "\\":
            i += 1
        elif quote:
            quote = None if c == quote else quote
        elif c in "\"'`":
            quote = c
        elif c == "#":
            return line[:i], None
        i += 1
    return line, "a quote left open at the line's end" if quote else None


def workflow_texts(lines):
    """Per line, the text the counter reads and why the reader cannot place
    it. A block scalar's content (`|` or `>`) is read whole: YAML strips no
    comment there, and a shell's comment is not this reader's to find, since
    a quote open across lines or nested in `$(…)` decides where it starts."""
    out, i = [], 0
    while i < len(lines):
        text, why = yaml_text(lines[i])
        m = BLOCK.fullmatch(text.rstrip())
        if m and m.group(4).strip("-+"):
            why = why or "a block indentation indicator"
        out.append((text, why))
        i += 1
        if not m:
            continue
        lead, dashes = len(m.group(1)), m.group(2)
        parent = lead + len(dashes) if m.group(3) else lead + len(dashes.rstrip()) - 1
        indent = None
        while i < len(lines) and (not lines[i].strip() or indent is None
                                  or len(lines[i]) - len(lines[i].lstrip(" ")) >= indent):
            width = len(lines[i]) - len(lines[i].lstrip(" "))
            if lines[i].strip() and indent is None:
                if width <= parent:
                    break
                indent = width
            out.append((lines[i], None))
            i += 1
    return out


def unplaced(name, lines, judged, texts):
    """Every spelling in the text as read (a comment stripped only where YAML
    or just strips one), read as one stream so a call folded or continued
    across lines is seen, counted on the line it starts on: a line may hold
    no more than the calls the reader judged there. One it cannot place is
    red, so a layout it does not read fails closed; so is a line it cannot
    read."""
    stream, starts, offset, out = [], [], 0, []
    for number, (text, why) in enumerate(texts, 1):
        if why:
            out.append(f"{name}:{number}: {why} this reader cannot place: "
                       f"{lines[number - 1].strip()}")
        text = text.rstrip()
        stream.append(text[:-1] if text.endswith("\\") else text)
        starts.append(offset)
        offset += len(stream[-1]) + 1
    stream = " ".join(stream)
    for what, spelled in SPELLED:
        held = {}
        for m in spelled.finditer(stream):
            number = bisect.bisect_right(starts, m.start())
            held[number] = held.get(number, 0) + 1
        for number in sorted(held):
            if judged.get(number, {}).get(what, 0) < held[number]:
                out.append(f"{name}:{number}: {what} this reader cannot place: "
                           f"{lines[number - 1].strip()}")
    return out


def jobs_of(lines):
    """{job: [(line number, line)]} under `jobs:`, and the job ids' lines."""
    if "jobs:" not in lines:
        return None, set()
    out, ids, job, first = {}, set(), None, lines.index("jobs:") + 1
    for number, line in enumerate(lines[first:], first + 1):
        if line[:1] not in ("", " ", "#"):
            break
        m = re.fullmatch(r"  ([A-Za-z0-9_-]+):\s*", line)
        if m:
            job = out.setdefault(m.group(1), [])
            ids.add(number)
        elif job is not None:
            job.append((number, line))
    return out, ids


def shape(body):
    """(needs, strategy lines, outputs lines, steps, the job's `name:` lines),
    a step's lines without their 8-column indent; a block form this reader
    does not parse reads as absent, so the rules below fail closed."""
    needs, strategy, outputs, steps, names, section, step = [], [], [], [], set(), None, None
    for number, line in body:
        if re.match(r"    [A-Za-z]", line):
            section, _, value = line.strip().partition(":")
            step = None
            if section == "needs":
                needs = [n.strip() for n in value.strip().strip("[]").split(",") if n.strip()]
            if section == "name":
                names.add(number)
        elif section == "strategy" and line.strip():
            strategy.append(line.strip())
        elif section == "outputs" and line.strip():
            outputs.append(line.strip())
        elif section == "steps" and line.strip().startswith("#"):
            continue
        elif section == "steps" and line.startswith("      - "):
            step = [(number, line[8:])]
            steps.append(step)
        elif section == "steps" and step is not None:
            step.append((number, line[8:]))
    return needs, strategy, outputs, [fields(s) for s in steps], names


def fields(step):
    """A step's keys, its `run` lines with their numbers, and each key's line."""
    f, i = {"run": [], "run_lines": [], "with": {}, "at": {}}, 0
    while i < len(step):
        number, text = step[i]
        m = re.fullmatch(r"([A-Za-z-]+):\s*(.*)", text)
        i += 1
        if not m:
            continue
        key, value = m.groups()
        f["at"].setdefault(key, number)
        if key == "run" and value in ("|", "|-", ">", ">-"):
            while i < len(step) and (step[i][1].startswith("  ") or not step[i][1].strip()):
                f["run"].append(step[i][1].strip())
                f["run_lines"].append((step[i][0], step[i][1].strip()))
                i += 1
        elif key == "with":
            while i < len(step) and step[i][1].startswith("  "):
                k, _, v = step[i][1].strip().partition(":")
                f["with"][k] = v.strip()
                i += 1
        elif key == "run":
            f["run"].append(value)
            f["run_lines"].append((number, value))
        else:
            f[key] = value
    return f


def judge_lines(judged, step):
    """The calls the reader judges on each of a step's lines."""
    for number, text in step["run_lines"]:
        line = judged.setdefault(number, {})
        for what, spelled in SPELLED:
            line[what] = line.get(what, 0) + len(spelled.findall(command(text)))
    if step.get("uses", "").startswith(DENY_USES):
        judged.setdefault(step["at"]["uses"], {})["a cargo-deny call"] = 1
    if "name" in step["at"]:
        judged[step["at"]["name"]] = NAME


def refs(steps):
    return [s["with"].get("ref", "") for s in steps
            if s.get("uses", "").startswith("actions/checkout")]


def census_mode(shapes, job):
    """The mode the census job prints, or why it prints none."""
    if job not in shapes:
        return None, f"no job `{job}`"
    _, _, outputs, steps, _ = shapes[job]
    if "matrix: ${{ steps.census.outputs.matrix }}" not in outputs:
        return None, f"`{job}` has no output `matrix: ${{{{ steps.census.outputs.matrix }}}}`"
    census = [s for s in steps if s.get("id") == "census"]
    run = [l for l in census[0]["run"] if l] if len(census) == 1 else []
    m = CENSUS.fullmatch(run[0]) if len(run) == 2 and run[1] == WRITE else None
    if m is None or census[0].get("shell") != "bash":
        return None, (f"`{job}`'s census step is not one `id: census` step, `shell: bash`, running "
                      "`matrix=$(./scripts/check-fuzz-targets.sh --matrix <mode>)` then "
                      f"`{WRITE}`")
    return m.group(1), None


def under_census(where, what, want, job, shapes):
    """The mode a matrix job's call reads from its census job, or None after
    the reason it reads none of `want`."""
    needs, strategy, _, steps, _ = shapes[job]
    matrix = [l for l in strategy if l.startswith("matrix:")]
    other = [l for l in strategy if not l.startswith(("matrix:", "#"))
             and not re.fullmatch(r"(fail-fast|max-parallel):\s*\S+", l)]
    m = MATRIX.fullmatch(matrix[0][len("matrix:"):].strip()) if len(matrix) == 1 else None
    if m is None or other:
        errors.append(f"{where}: {what} not under a matrix read from a census job "
                      "(`matrix: ${{ fromJSON(needs.<job>.outputs.matrix) }}` alone)")
        return None
    census = m.group(1)
    mode, why = census_mode(shapes, census)
    if census not in needs:
        errors.append(f"{where}: its matrix is `{census}`'s, a job it does not need")
    elif why:
        errors.append(f"{where}: {why}")
    elif mode not in want:
        errors.append(f"{where}: {what} reads --matrix {mode}, not {' or '.join(want)}")
    elif refs(steps) != refs(shapes[census][3]):
        errors.append(f"{where}: checks out {refs(steps)}, and `{census}` read the census "
                      f"at {refs(shapes[census][3])}: the matrix is another commit's")
    else:
        read.append(f"{where} <- {census} (--matrix {mode})")
        return mode
    return None


for wf in files:
    lines = open(os.path.join(wf_dir, wf), encoding="utf-8").read().split("\n")
    texts = workflow_texts(lines)
    jobs, ids = jobs_of(lines)
    judged = {number: NAME for number in ids}
    if jobs is None:
        errors.append(f"{wf}: no top-level `jobs:` line")
        errors.extend(unplaced(wf, lines, judged, texts))
        continue
    shapes = {job: shape(body) for job, body in jobs.items()}
    for job, (_, _, _, steps, names) in shapes.items():
        judged.update({number: NAME for number in names})
        for step in steps:
            judge_lines(judged, step)
            raw = " ".join(step["run"])
            text = " ".join(command(l) for l in step["run"])
            fuzz, deny = len(FUZZ_RUN.findall(text)), len(DENY_RUN.findall(text))
            action, where = step.get("uses", "").startswith(DENY_USES), f"{wf} {job}"
            # Each call is judged by its own rule, wherever it sits: a step
            # holding both meets both, which no one census mode allows.
            if fuzz and (wf, job) == PR_SMOKE[:2]:
                targets = FUZZ_CALL.findall(text)
                for target in targets + ["?"] * (fuzz - len(targets)):
                    if target == PR_SMOKE[2]:
                        pr_smoke += 1
                    else:
                        errors.append(f"{where}: the PR smoke fuzzes `{target}`, not "
                                      f"{PR_SMOKE[2]}, its one named target (ADR-0198 D3)")
            elif fuzz:
                mode = under_census(where, "a fuzz run", ("nightly", "smoke"), job, shapes)
                if mode and not (len(ROW_CALL.findall(text)) == fuzz
                                 and "${{ matrix.seeds }}" in raw
                                 and step.get("working-directory") == "${{ matrix.dir }}"):
                    errors.append(f"{where}: a fuzz run that does not name its row's "
                                  "`${{ matrix.target }}`, `${{ matrix.seeds }}` and "
                                  "`working-directory: ${{ matrix.dir }}`")
                elif mode:
                    found.add((wf, mode))
            if deny or action:
                mode = under_census(where, "a cargo-deny call", ("deny",), job, shapes)
                if mode and not (len(ROW_DENY.findall(text)) == deny and (
                        not action or step["with"].get("manifest-path") == ROW_MANIFEST)):
                    errors.append(f"{where}: a cargo-deny call that does not name its row's "
                                  f"`{ROW_MANIFEST}`")
                elif mode:
                    found.add((wf, mode))
    errors.extend(unplaced(wf, lines, judged, texts))
for wf, mode in sorted(EXPECTED - found):
    errors.append(f"{wf}: no job reads --matrix {mode} from a census job (ADR-0198 "
                  f"{'D6' if mode == 'deny' else 'D3'})")
if pr_smoke != 1:
    errors.append(f"{PR_SMOKE[0]} {PR_SMOKE[1]}: {pr_smoke} runs of {PR_SMOKE[2]}, not one")
just = open(os.path.join(root, "justfile"), encoding="utf-8").read().split("\n")
recipes, body = {}, None
for number, line in enumerate(just, 1):
    m = re.match(r"@?([A-Za-z_][\w-]*)(?:[ \t]+[^:\n]*)?:(?!=)", line)
    if m:
        body = recipes.setdefault(m.group(1), [])
    elif line[:1] in (" ", "\t") and body is not None:
        body.append((number, line))
    elif line.strip() and not line.startswith("#"):
        body = None
judged = {}
for name, lines in sorted(recipes.items()):
    calls = [(n, l) for n, l in lines if DENY_RUN.search(command(l))]
    judged.update({n: {"a cargo-deny call": len(DENY_RUN.findall(command(l)))} for n, l in calls})
    if name != "deny" and calls:
        errors.append(f"justfile: recipe `{name}` runs cargo-deny outside `deny`: "
                      f"{calls[0][1].strip()}")
deny = recipes.get("deny", [])
if not any(command(l).split() == ["rows=$(./scripts/check-fuzz-targets.sh", "--matrix", "deny)"]
           for _, l in deny):
    errors.append("justfile: the `deny` recipe does not read "
                  "`rows=$(./scripts/check-fuzz-targets.sh --matrix deny)`")
calls = [(n, l) for n, l in deny if DENY_RUN.search(command(l))]
if not calls:
    errors.append("justfile: the `deny` recipe runs cargo-deny on no census row "
                  "(`--manifest-path \"$manifest\"`)")
for n, l in calls:  # per call: each one on the line names the row
    if len(RECIPE_DENY.findall(l)) != len(DENY_RUN.findall(l)):
        errors.append(f"justfile:{n}: a cargo-deny call in `deny` that does not name its census "
                      f"row (`--manifest-path \"$manifest\"`): {l.strip()}")
# A recipe's body is read whole, as a block scalar is.
body_lines = {n for lines in recipes.values() for n, _ in lines}
texts = [(l, None) if n in body_lines else just_text(l) for n, l in enumerate(just, 1)]
errors.extend(unplaced("justfile", just, judged, texts))
for e in errors:
    print(f"{TOKEN}: {e}")
if errors:
    sys.exit(1)
print(f"{TOKEN} OK: {'; '.join(read)}; {PR_SMOKE[0]} {PR_SMOKE[1]} {PR_SMOKE[2]}, the PR "
      "smoke's one named target; justfile deny <- --matrix deny")
PY
}
expect green "census-matrices: the workflows and the deny recipe read the census" census_matrices .
expect_output "census-matrices: the OK line names what it read" \
    "infinity-fuzz-nightly.yml fuzz-hour <- targets (--matrix nightly)" census_matrices .
# cm_copy <name>: a copy of the workflows and the justfile, as CM_ROOT.
cm_copy() {
    CM_ROOT="$work/cm-$1"
    rm -rf "${CM_ROOT:?}"
    mkdir -p "$CM_ROOT/.github"
    cp -R .github/workflows "$CM_ROOT/.github/workflows"
    cp justfile "$CM_ROOT/justfile"
}
# cm_plant <name> <file> <text> <replacement>: the copy with one exact edit; a
# plant whose text is not there once is a failed case, never a vacuous red.
cm_plant() {
    cm_copy "$1"
    if ! python3 - "$CM_ROOT/$2" "$3" "$4" <<'PY'
import sys
path, old, new = sys.argv[1:]
text = open(path, encoding="utf-8").read()
if text.count(old) != 1:
    sys.exit(f"found {text.count(old)} times")
open(path, "w", encoding="utf-8").write(text.replace(old, new))
PY
    then
        fail=$((fail + 1))
        echo "SELFTEST FAIL: census-matrices plant '$1' did not apply"
    fi
}
# cm_add <name> <text>: the copy with one workflow added, plant.yml.
cm_add() {
    cm_copy "$1"
    printf '%s\n' "$2" >"$CM_ROOT/.github/workflows/plant.yml"
}
CM_NIGHTLY=.github/workflows/infinity-fuzz-nightly.yml
CM_FULL=.github/workflows/infinity-ci-full.yml
CM_PR=.github/workflows/infinity-ci.yml
cm_red() { # <label> <pattern>: the last plant is red, naming its rule
    expect_red_because "census-matrices: $1" "$2" census_matrices "$CM_ROOT"
}
cm_plant literal "$CM_NIGHTLY" '      matrix: ${{ fromJSON(needs.targets.outputs.matrix) }}' \
    $'      matrix:\n        include:\n          - { dir: crates/inf-wire, target: resp_parse, seeds: "" }'
cm_red "a literal nightly matrix" \
    "infinity-fuzz-nightly.yml fuzz-hour: a fuzz run not under a matrix"
cm_plant hand-step "$CM_FULL" '      - name: criterion benches compile (no run)' \
    $'      - run: cargo +nightly fuzz run frame_decode -- -max_total_time=60\n      - name: criterion benches compile (no run)'
cm_red "a hand-kept fuzz step" "infinity-ci-full.yml bench-build: a fuzz run not under a matrix"
cm_plant target "$CM_FULL" 'fuzz run ${{ matrix.target }} fuzz/corpus' 'fuzz run resp_parse fuzz/corpus'
cm_red "a matrix job fuzzing a literal target" "fuzz-smoke: a fuzz run that does not name its row's"
cm_plant seeds "$CM_NIGHTLY" ' ${{ matrix.seeds }} -- ' ' -- '
cm_red "a fuzz run without its row's seeds" "fuzz-hour: a fuzz run that does not name its row's"
cm_plant inline "$CM_NIGHTLY" $'          matrix=$(./scripts/check-fuzz-targets.sh --matrix nightly)\n          echo "matrix=$matrix" >> "$GITHUB_OUTPUT"' \
    '          echo "matrix=$(./scripts/check-fuzz-targets.sh --matrix nightly)" >> "$GITHUB_OUTPUT"'
cm_red "the census step's inline form" "\`targets\`'s census step is not one"
cm_plant needs "$CM_NIGHTLY" '    needs: targets' '    needs: []'
cm_red "a matrix job that does not need its census job" "a job it does not need"
cm_plant ref "$CM_FULL" $'      - uses: actions/checkout@v5\n        with:\n          ref: ${{ needs.authorize.outputs.sha }}\n      - name: the fuzz targets, from the census' \
    $'      - uses: actions/checkout@v5\n      - name: the fuzz targets, from the census'
cm_red "a census read at another commit" "the matrix is another commit's"
cm_plant build "$CM_NIGHTLY" 'cargo +nightly fuzz run ${{ matrix.target }}' 'cargo +nightly fuzz build ${{ matrix.target }}'
cm_red "a nightly that fuzzes no census target" \
    "infinity-fuzz-nightly.yml: no job reads --matrix nightly from a census job"
cm_plant deny-path "$CM_PR" '          manifest-path: ${{ matrix.manifest }}' '          manifest-path: Cargo.toml'
cm_red "a cargo-deny step on a literal manifest" "a cargo-deny call that does not name its row's"
cm_plant deny-mode "$CM_PR" '--matrix deny)' '--matrix smoke)'
cm_red "cargo-deny reading the fuzz matrix" "a cargo-deny call reads --matrix smoke, not deny"
cm_plant pr-smoke "$CM_PR" 'fuzz run resp_parse --' 'fuzz run frame_decode --'
cm_red "the PR smoke on another literal target" "the PR smoke fuzzes .frame_decode., not resp_parse"
cm_plant recipe justfile '    rows=$(./scripts/check-fuzz-targets.sh --matrix deny)' '    rows=Cargo.toml'
cm_red "the deny recipe off the census" "the .deny. recipe does not read"
cm_plant second-recipe justfile $'\ntest:\n' $'\ndeny-root:\n    cargo deny check\n\ntest:\n'
cm_red "a second recipe running cargo-deny" "recipe .deny-root. runs cargo-deny outside .deny."

# The read fails closed. Each plant is valid YAML with its step intact; one in
# a layout, a quoting or a YAML form the reader does not judge is red as a
# call or a line it cannot place, on the line it starts on, and a spelling it
# judges, in the tree's layout, is placed and red for its rule.
cm_places() { # <root>: the reader ran and placed every call
    census_matrices "$1" >"$work/cm-places.log" 2>&1 || :
    grep -q '^CENSUS-MATRICES' "$work/cm-places.log" \
        && ! grep -q 'this reader cannot place' "$work/cm-places.log"
}
cm_judged() { # <label> <pattern>: the last plant is placed, and red naming its rule
    cm_red "$1" "$2"
    expect green "census-matrices: $1, placed" cm_places "$CM_ROOT"
}
CM_HEAD=$'name: plant\non: push\njobs:\n'
CM_JOB=$CM_HEAD$'  plant:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/checkout@v5\n'
CM_FUZZ='cargo +nightly fuzz run frame_decode -- -max_total_time=60'
CM_WD=$'\n        working-directory: crates/inf-log'
cm_add tree-layout "$CM_JOB      - run: $CM_FUZZ$CM_WD"
cm_judged "a hand fuzz step in the tree's layout" "plant.yml plant: a fuzz run not under a matrix"
# Every spelling cargo runs is judged as the tree's own calls are: an option
# before the subcommand, its alias, the binary, a quoted option value.
for CM_SPELLING in 'cargo --locked fuzz run' 'cargo +nightly --locked fuzz run' \
    'cargo +nightly fuzz r' 'cargo-fuzz run' 'cargo-fuzz r'; do
    cm_add spelling "$CM_JOB      - run: $CM_SPELLING frame_decode -- -max_total_time=60$CM_WD"
    cm_judged "\`$CM_SPELLING\` in the tree's layout" "plant.yml plant: a fuzz run not under a matrix"
done
for CM_SPELLING in 'cargo -q deny' 'cargo --color never deny' 'cargo --config "net.retry = 2" deny'; do
    cm_add spelling "$CM_JOB      - run: $CM_SPELLING --manifest-path Cargo.toml check$CM_WD"
    cm_judged "\`$CM_SPELLING\` in the tree's layout" \
        "plant.yml plant: a cargo-deny call not under a matrix"
done
cm_add compact "$CM_HEAD"$'  plant:\n    runs-on: ubuntu-24.04\n    steps:\n    - uses: actions/checkout@v5\n'"    - run: $CM_FUZZ"
cm_red "a compact step sequence" "plant.yml:8: a fuzz run this reader cannot place"
cm_add indent4 "$CM_HEAD"$'    plant:\n        runs-on: ubuntu-24.04\n        steps:\n            - uses: actions/checkout@v5\n'"            - run: $CM_FUZZ"
cm_red "a job at four columns" "plant.yml:8: a fuzz run this reader cannot place"
cm_add bash-c "$CM_JOB      - run: bash -c \"cd crates/inf-log && $CM_FUZZ\""
cm_red "a fuzz run in a quoted shell string" "plant.yml:8: a fuzz run this reader cannot place"
cm_add quoted "$CM_JOB"'      - run: "cargo deny --manifest-path Cargo.toml check"'
cm_red "a cargo-deny call in a quoted scalar" "plant.yml:8: a cargo-deny call this reader cannot place"
cm_add action-case "$CM_JOB"$'      - uses: embarkstudios/cargo-deny-action@v2\n        with:\n          manifest-path: Cargo.toml'
cm_red "the cargo-deny action in another case" "plant.yml:8: a cargo-deny call this reader cannot place"
cm_add folded "$CM_JOB"$'      - run: cargo +nightly fuzz\n          run frame_decode -- -max_total_time=60'
cm_red "a fuzz run folded across lines" "plant.yml:8: a fuzz run this reader cannot place"
# A YAML anchor, alias or tag is red wherever it sits: an alias carries the
# anchored text, a call on a name line or on a judged run line, into a step
# the reader judges as something else.
cm_add alias-name "$CM_JOB      - name: &fz $CM_FUZZ"$'\n        run: *fz'"$CM_WD"
cm_red "a call aliased out of a step's name" \
    "plant.yml:8: a YAML anchor, alias or tag this reader cannot place"
CM_HOUR='cargo +nightly fuzz run ${{ matrix.target }} fuzz/corpus/${{ matrix.target }} ${{ matrix.seeds }} -- -max_total_time=19800'
cm_plant alias-run "$CM_NIGHTLY" "        run: $CM_HOUR" "        run: &hour $CM_HOUR"
printf '%s\n' '' '  plant:' '    runs-on: ubuntu-24.04' '    steps:' '      - uses: actions/checkout@v5' \
    '      - run: *hour' >>"$CM_ROOT/$CM_NIGHTLY"
cm_red "a matrix call aliased into another job" \
    "infinity-fuzz-nightly.yml:[0-9]*: a YAML anchor, alias or tag this reader cannot place"
cm_add tag "$CM_JOB      - run: !!str cargo deny --manifest-path Cargo.toml check$CM_WD"
cm_red "a tagged run" "plant.yml:8: a YAML anchor, alias or tag this reader cannot place"
cm_add escape "$CM_JOB"'      - run: "cargo\x20deny --manifest-path Cargo.toml check"'
cm_red "an escape that spells a call" "plant.yml:8: an escape in a double-quoted YAML scalar"
cm_add quote-lines "$CM_JOB"$'      - run: \'echo "\n          # "; cargo deny --manifest-path Cargo.toml check\''
cm_red "a quoted scalar across lines" "plant.yml:8: a quote left open at the line's end"
cm_add indicator "$CM_JOB"$'      - run: |2\n            echo a\n          cargo deny --manifest-path Cargo.toml check'
cm_red "a block indentation indicator" "plant.yml:8: a block indentation indicator"
# A block's content is read whole: a quote open across its lines, or nested
# in `$(…)`, puts a call the shell runs after a `#`.
cm_add open-quote "$CM_JOB"$'      - run: |\n          echo "\n          # "; '"$CM_FUZZ$CM_WD"
cm_red "a call after a quote open across a block's lines" \
    "plant.yml:10: a fuzz run this reader cannot place"
cm_add nested-quote "$CM_JOB"$'      - run: |\n          x="$(echo "a # b")"; cargo deny --manifest-path Cargo.toml check'
cm_red "a call after a quote nested in \$(…)" "plant.yml:9: a cargo-deny call this reader cannot place"
cm_add script-comment "$CM_JOB"$'      - run: |\n          # '"$CM_FUZZ"$'\n          true'
cm_red "a call spelled in a script's comment" "plant.yml:9: a fuzz run this reader cannot place"
cm_add text "$CM_JOB"$'      - name: the census\'s targets\n        if: ${{ !cancelled() }}\n        run: |\n          python3 - <<\'PY\'\n          print("can\'t")\n          PY'
expect green "census-matrices: an expression, an apostrophe and a heredoc read as text" \
    census_matrices "$CM_ROOT"
cm_plant second-quoted "$CM_NIGHTLY" ' -- -max_total_time=19800' " -- -max_total_time=19800 && bash -c \"$CM_FUZZ\""
cm_red "a quoted second fuzz run on a judged line" \
    "infinity-fuzz-nightly.yml:[0-9]*: a fuzz run this reader cannot place"
cm_plant second-call "$CM_NIGHTLY" "        run: $CM_HOUR" $'        run: |\n'"          $CM_HOUR"$'\n'"          $CM_FUZZ"
cm_red "a second, literal fuzz run in a matrix step" "fuzz-hour: a fuzz run that does not name its row's"
CM_DENY=$'      - uses: EmbarkStudios/cargo-deny-action@v2\n        with:\n          manifest-path: ${{ matrix.manifest }}'
CM_DENY_ROW='cargo deny --manifest-path ${{ matrix.manifest }} --all-features check'
cm_plant deny-run "$CM_PR" "$CM_DENY" "      - run: $CM_DENY_ROW"
expect green "census-matrices: a cargo-deny call naming its row is read" census_matrices "$CM_ROOT"
cm_plant deny-run-second "$CM_PR" "$CM_DENY" $'      - run: |\n'"          $CM_DENY_ROW"$'\n          cargo deny --manifest-path Cargo.toml --all-features check'
cm_red "a second, literal cargo-deny call in a matrix step" \
    "cargo-deny: a cargo-deny call that does not name its row's"
cm_plant pr-smoke-second "$CM_PR" 'fuzz run resp_parse -- -max_total_time=60' \
    "fuzz run resp_parse -- -max_total_time=60 && $CM_FUZZ"
cm_red "a second target in the PR smoke's step" "the PR smoke fuzzes .frame_decode., not resp_parse"
# A cargo-deny call is judged by its own rule wherever it sits: in a fuzz step
# (the PR smoke, the nightly, the full smoke), beside its row's call on one
# line, or beside the deny recipe's call on its line.
cm_plant pr-smoke-deny "$CM_PR" 'fuzz run resp_parse -- -max_total_time=60' \
    'fuzz run resp_parse -- -max_total_time=60 && cargo deny --manifest-path Cargo.toml check'
cm_red "a cargo-deny call in the PR smoke's step" \
    "infinity-ci.yml fuzz-smoke: a cargo-deny call not under a matrix"
cm_plant nightly-deny "$CM_NIGHTLY" ' -- -max_total_time=19800' \
    ' -- -max_total_time=19800 && cargo deny --manifest-path crates/inf-log/fuzz/Cargo.toml check'
cm_red "a cargo-deny call in the nightly's fuzz step" \
    "fuzz-hour: a cargo-deny call reads --matrix nightly, not deny"
CM_SMOKE='cargo +nightly fuzz run ${{ matrix.target }} fuzz/corpus/${{ matrix.target }} ${{ matrix.seeds }} -- -max_total_time=300'
cm_plant smoke-deny "$CM_FULL" "        run: $CM_SMOKE" \
    $'        run: |\n'"          $CM_SMOKE"$'\n          cargo deny --manifest-path Cargo.toml check'
cm_red "a cargo-deny line in the full smoke's fuzz step" \
    "fuzz-smoke: a cargo-deny call reads --matrix smoke, not deny"
cm_plant deny-one-line "$CM_PR" "$CM_DENY" \
    "      - run: $CM_DENY_ROW && cargo deny --manifest-path Cargo.toml --all-features check"
cm_red "a literal cargo-deny call on its row's line" \
    "cargo-deny: a cargo-deny call that does not name its row's"
cm_plant deny-path-prefix "$CM_PR" '          manifest-path: ${{ matrix.manifest }}' \
    '          manifest-path: x/${{ matrix.manifest }}'
cm_red "the action on a path built from its row" \
    "cargo-deny: a cargo-deny call that does not name its row's"
cm_plant recipe-second justfile '--all-features $fetch </dev/null; then' \
    '--all-features $fetch </dev/null && cargo deny --manifest-path Cargo.toml check </dev/null; then'
cm_red "a literal cargo-deny call beside the deny recipe's" \
    "justfile:[0-9]*: a cargo-deny call in .deny. that does not name its census row"
cm_plant recipe-quoted justfile $'\ntest:\n' $'\ndeny-root:\n    bash -c "cargo deny check"\n\ntest:\n'
cm_red "a quoted cargo-deny call in a second recipe" "justfile:[0-9]*: a cargo-deny call this reader cannot place"
cm_plant just-string justfile $'\ntest:\n' $'\nnote := """\n# """ + `cargo deny check`\n\ntest:\n'
cm_red "a justfile string across lines" "justfile:[0-9]*: a quote left open at the line's end"

# The deny recipe, run as just runs it (its body a bash script in the
# justfile's directory) against a stub census and a stub cargo that reads its
# stdin to the end: every census row is a leg, a red leg fails the recipe, and
# so do an empty list and a leg count other than the census's rows. A leg that
# read the list the loop reads would end the loop early, green; the copy whose
# legs inherit the loop's stdin is red on the count.
DR=$work/deny-recipe
mkdir -p "$DR/scripts" "$DR/bin"
awk '/^deny:/ { on = 1; next } on && /^[^ \t]/ { exit } on { sub(/^    /, ""); print }' justfile >"$DR/recipe.sh"
sed 's| </dev/null||' "$DR/recipe.sh" >"$DR/recipe-stdin.sh"
printf '%s\n' '#!/bin/sh' '[ "$*" = "--matrix deny" ] || exit 2' 'cat "$DR_ROWS"' \
    'exit "$(cat "$DR_ROWS.status")"' >"$DR/scripts/check-fuzz-targets.sh"
printf '%s\n' '#!/bin/sh' 'printf "%s\n" "$*" >>"$DR_LOG"' 'cat >/dev/null' \
    'case " $* " in *" $DR_RED "*) exit 4 ;; esac' >"$DR/bin/cargo"
chmod +x "$DR/scripts/check-fuzz-targets.sh" "$DR/bin/cargo"
DR_ROWS3='{"include":[{"manifest":"Cargo.toml"},{"manifest":"bins/b/fuzz/Cargo.toml"},{"manifest":"crates/a/fuzz/Cargo.toml"}]}'
dr_run() { # <recipe> <census stdout> <census status> <manifest cargo-deny refuses>
    printf '%s' "$2" >"$DR/rows"
    printf '%s\n' "$3" >"$DR/rows.status"
    : >"$DR/cargo.log"
    (cd "$DR" && env PATH="$DR/bin:$PATH" DR_ROWS="$DR/rows" DR_LOG="$DR/cargo.log" DR_RED="$4" \
        bash "$1") >"$DR/out" 2>&1
}
dr_legs() { # <count>: cargo was called that many times
    [ "$(grep -c . "$DR/cargo.log")" -eq "$1" ]
}
dr_every_leg() {
    dr_run "$DR/recipe.sh" "$DR_ROWS3" 0 - || return 1
    grep -qx 'deny OK: 3 manifests (the root and 2 fuzz manifests), each rc 0' "$DR/out" \
        && [ "$(cat "$DR/cargo.log")" = "$(printf '%s\n' \
            'deny --manifest-path Cargo.toml --all-features check' \
            'deny --manifest-path bins/b/fuzz/Cargo.toml --all-features check --disable-fetch' \
            'deny --manifest-path crates/a/fuzz/Cargo.toml --all-features check --disable-fetch')" ]
}
expect green "deny: every census row is a leg, the first one fetching" dr_every_leg
dr_red_leg() {
    ! dr_run "$DR/recipe.sh" "$DR_ROWS3" 0 bins/b/fuzz/Cargo.toml \
        && grep -qx 'deny: bins/b/fuzz/Cargo.toml red (exit 4)' "$DR/out" \
        && grep -qx 'deny FAILED: 1 of 3 manifests red' "$DR/out" && dr_legs 3
}
expect green "deny: a red leg fails the recipe, and every leg still runs" dr_red_leg
dr_short_run() {
    ! cmp -s "$DR/recipe.sh" "$DR/recipe-stdin.sh" \
        && ! dr_run "$DR/recipe-stdin.sh" "$DR_ROWS3" 0 - \
        && grep -qx 'deny FAILED: ran 1 of the 3 manifests the census listed' "$DR/out" && dr_legs 1
}
expect green "deny: a leg that reads the loop's list is red on the count (canary)" dr_short_run
dr_empty() {
    ! dr_run "$DR/recipe.sh" '{"include":[]}' 0 - \
        && grep -qx 'deny SCOPE ERROR: the fuzz census listed no manifest' "$DR/out" && dr_legs 0
}
expect green "deny: an empty census list is a scope error, no leg run" dr_empty
dr_red_census() {
    ! dr_run "$DR/recipe.sh" '' 1 - && ! grep -q 'deny OK' "$DR/out" && dr_legs 0
}
expect green "deny: a red census runs no leg" dr_red_census
}

# ------------------------------------------------------------- target root
group_target_root() {
# ADR-0204 D1, D2, D5: scripts/target-root.sh on fixture checkouts, each with a
# copy of the helper under scripts/. A stub cargo first on PATH records each
# call and answers `cargo metadata` as TR_META says, from the directory it
# runs in (its `clean` removes the one directory it names, under the work
# root only); the config cases run the pinned cargo itself. The key's oracle
# is fx_root_key. Paths hold a space, so a printed command that splits a path
# is red. Each case runs the sourced helper in a process of its own: the root
# it sets is read-only in the shell that set it.
TRW="$work/tr"
TR_BIN="$TRW/bin"
mkdir -p "$TR_BIN"
IFS= read -r -d '' body <<'EOF' || true
#!/usr/bin/env bash
# The stub cargo: records each call, answers metadata, and `clean --target-dir
# <dir>` removes that one directory when it lies under the work root.
set -eu
{
    printf 'call'
    printf '\t%s' "$@"
    printf '\tCARGO_TARGET_DIR=%s\n' "${CARGO_TARGET_DIR-<unset>}"
} >>"$TR_CALLS"
case "$1" in
metadata)
    t="$(pwd -P)/target"
    case "${TR_META:-same}" in
    same) printf '{"target_directory":"%s","build_directory":"%s"}\n' "$t" "$t" ;;
    missing) printf '{"target_directory":"%s"}\n' "$t" ;;
    number) printf '{"target_directory":"%s","build_directory":7}\n' "$t" ;;
    split) printf '{"target_directory":"%s","build_directory":"%s/bd"}\n' "$t" "$TR_WORK" ;;
    elsewhere) printf '{"target_directory":"%s/td","build_directory":"%s/td"}\n' "$TR_WORK" "$TR_WORK" ;;
    fail) echo "error: failed to parse manifest" >&2; exit 101 ;;
    esac ;;
clean)
    [ "$#" -eq 3 ] && [ "$2" = --target-dir ] || exit 3
    case "$3" in "$TR_WORK"/?*) rm -rf "$3" ;; *) exit 3 ;; esac ;;
*) exit 3 ;;
esac
EOF
printf '%s' "$body" >"$TR_BIN/cargo"
chmod +x "$TR_BIN/cargo"
tr_checkout() { # <dir>: a fixture checkout holding the helper
    mkdir -p "$1/scripts"
    cp "$SCRIPT_DIR/target-root.sh" "$1/scripts/target-root.sh"
}
tr_phys() { (cd "$1" && pwd -P); }
tr_key() { fx_root_key "$1"; }
# tr_entry <checkout> <CARGO_TARGET_DIR, or - for unset> <--print|--clean>
# [<VAR=value>…]: an entry, stdout to $TRW/out, stderr to $TRW/err, the
# stub's calls to $TRW/calls; the last arguments are set in its environment.
tr_entry() {
    local dir=$1 var=$2 how=$3
    shift 3
    : >"$TRW/calls"
    if [ "$var" = - ]; then
        env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET PATH="$TR_BIN:$PATH" \
            TR_CALLS="$TRW/calls" TR_WORK="$TRW" "$@" "$dir/scripts/target-root.sh" "$how" \
            >"$TRW/out" 2>"$TRW/err"
    else
        env -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET CARGO_TARGET_DIR="$var" PATH="$TR_BIN:$PATH" \
            TR_CALLS="$TRW/calls" TR_WORK="$TRW" "$@" "$dir/scripts/target-root.sh" "$how" \
            >"$TRW/out" 2>"$TRW/err"
    fi
}
tr_print() { local dir=$1 var=$2; shift 2; tr_entry "$dir" "$var" --print "$@"; }
tr_clean() { local dir=$1 var=$2; shift 2; tr_entry "$dir" "$var" --clean "$@"; }
# tr_clean_refused <checkout> <var or -> <pattern> [<VAR=value>…]: --clean
# exits 2, nothing on stdout, nothing removed, no cargo clean called.
tr_clean_refused() {
    local dir=$1 var=$2 pattern=$3 status=0
    shift 3
    tr_clean "$dir" "$var" "$@" || status=$?
    [ "$status" -eq 2 ] && [ ! -s "$TRW/out" ] && grep -q -- "$pattern" "$TRW/err" &&
        ! grep -q "$(printf 'call\tclean')" "$TRW/calls" && return 0
    echo "--clean exit $status, want 2 naming '$pattern':"
    cat "$TRW/out" "$TRW/err" "$TRW/calls"
    return 1
}
tr_prints() { # <checkout> <var or -> <root>: exit 0, stdout exactly the root
    local status=0
    tr_print "$1" "$2" || status=$?
    [ "$status" -eq 0 ] && [ "$(cat "$TRW/out")" = "$3" ] && return 0
    echo "exit $status, want 0 and $3:"
    cat "$TRW/out" "$TRW/err"
    return 1
}
# tr_refused <checkout> <var or -> <pattern> [<VAR=value>…]: exit 2, nothing
# on stdout, the reason on stderr.
tr_refused() {
    local dir=$1 var=$2 pattern=$3 status=0
    shift 3
    tr_print "$dir" "$var" "$@" || status=$?
    [ "$status" -eq 2 ] && [ ! -s "$TRW/out" ] && grep -q -- "$pattern" "$TRW/err" && return 0
    echo "exit $status, want 2 naming '$pattern':"
    cat "$TRW/out" "$TRW/err"
    return 1
}
# tr_setup <checkout> <var or -> <line> [<VAR=value>…]: the sourced helper's
# key (1), physical checkout (2), root (3) or exported CARGO_TARGET_DIR (4),
# in a shell of its own.
tr_setup() {
    local dir=$1 var=$2 line=$3 script='. "$1/scripts/target-root.sh" && target_root_setup "$1" &&
printf "%s\n%s\n%s\n" "$TARGET_ROOT_KEY" "$TARGET_ROOT_CHECKOUT" "$TARGET_ROOT" && sh -c "echo \"\$CARGO_TARGET_DIR\""'
    shift 3
    if [ "$var" = - ]; then
        env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET "$@" \
            bash -c "$script" sh "$dir" | sed -n "${line}p"
    else
        env -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET CARGO_TARGET_DIR="$var" "$@" \
            bash -c "$script" sh "$dir" | sed -n "${line}p"
    fi
}
# tr_setup_refused <checkout> <var or -> <pattern> [<VAR=value>…]: setup
# returns 2 with the reason, in a shell of its own.
tr_setup_refused() {
    local dir=$1 var=$2 pattern=$3 status=0
    shift 3
    if [ "$var" = - ]; then
        env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET "$@" \
            bash -c '. "$1/scripts/target-root.sh" && target_root_setup "$1"' sh "$dir" \
            >"$TRW/out" 2>"$TRW/err" || status=$?
    else
        env -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET CARGO_TARGET_DIR="$var" "$@" \
            bash -c '. "$1/scripts/target-root.sh" && target_root_setup "$1"' sh "$dir" \
            >"$TRW/out" 2>"$TRW/err" || status=$?
    fi
    [ "$status" -eq 2 ] && grep -q -- "$pattern" "$TRW/err" && return 0
    echo "setup exit $status, want 2 naming '$pattern':"
    cat "$TRW/out" "$TRW/err"
    return 1
}
tr_is() { # <want> <command…>: the command prints exactly the wanted line
    local want=$1 got
    shift
    got=$("$@") || { echo "exit non-zero: $*"; return 1; }
    [ "$got" = "$want" ] || { echo "got '$got', want '$want'"; return 1; }
}
tr_b() { # <dir>: a checkout B with a .git, a marked root and a file in it
    tr_checkout "$1"
    mkdir -p "$1/.git" "$1/target/debug"
    printf 'ref: refs/heads/b\n' >"$1/.git/HEAD"
    tr_phys "$1" >"$1/target/.checkout-path"
    printf 'built by B\n' >"$1/target/debug/b-bin"
}
tr_b_snap() { # <dir>: copies of B's .git HEAD, marker and file
    rm -rf "${TRW:?}/snap"
    mkdir -p "$TRW/snap"
    cp "$1/.git/HEAD" "$1/target/.checkout-path" "$1/target/debug/b-bin" "$TRW/snap/"
}
tr_b_intact() { # <dir>: B's .git, marker and file unchanged
    cmp -s "$1/.git/HEAD" "$TRW/snap/HEAD" && cmp -s "$1/target/.checkout-path" "$TRW/snap/.checkout-path" &&
        cmp -s "$1/target/debug/b-bin" "$TRW/snap/b-bin" || { echo "B changed: $1"; return 1; }
}
tr_no_command() { # <stderr file>: a refusal that prints no command at all
    ! grep -q '^    ' "$1" || { echo "a command was printed:"; cat "$1"; return 1; }
}
tr_printed_run() { # <stderr file>: run each printed command as printed, on the stub cargo
    local line
    while IFS= read -r line; do
        case "$line" in "    "*) ;; *) continue ;; esac
        env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET -u CARGO_BUILD_BUILD_DIR PATH="$TR_BIN:$PATH" \
            TR_CALLS="$TRW/calls" TR_WORK="$TRW" bash -c "${line#    }" >/dev/null 2>&1 || :
    done <"$1"
}
TR_A="$TRW/co a"
TR_B="$TRW/co-b"
tr_checkout "$TR_A"
tr_checkout "$TR_B"
ln -s "$TR_A" "$TRW/link"
TR_PA=$(tr_phys "$TR_A")
TR_KA=$(tr_key "$TR_A")
TR_V="$TRW/shared root"
# The key and the root, sourced.
expect green "target-root: the key is 16 hex digits of SHA-256 of the physical path (a space in it)" \
    tr_is "$TR_KA" tr_setup "$TR_A" - 1
expect green "target-root: a symlinked checkout is keyed by its physical path" \
    tr_is "$TR_KA" tr_setup "$TRW/link" - 1
expect green "target-root: a symlinked checkout's path is its physical one" \
    tr_is "$TR_PA" tr_setup "$TRW/link" - 2
expect green "target-root: unset, the root is the checkout's target" \
    tr_is "$TR_PA/target" tr_setup "$TR_A" - 3
expect green "target-root: setup exports CARGO_TARGET_DIR as the root" \
    tr_is "$TR_PA/target" tr_setup "$TR_A" - 4
expect green "target-root: an empty CARGO_TARGET_DIR counts as unset" \
    tr_is "$TR_PA/target" tr_setup "$TR_A" '' 3
expect green "target-root: a variable naming the checkout's target is that target" \
    tr_is "$TR_PA/target" tr_setup "$TR_A" "$TR_PA/target" 3
expect green "target-root: a relative variable is read from the checkout's root" \
    tr_is "$TR_PA/target" tr_setup "$TR_A" target 3
expect green "target-root: the checkout's target named through a symlink is that target" \
    tr_is "$TR_PA/target" tr_setup "$TR_A" "$TRW/link/target" 3
expect green "target-root: setup refuses a CARGO_TARGET_DIR naming another directory" \
    tr_setup_refused "$TR_A" "$TR_V" "CARGO_TARGET_DIR is '$TR_V', not this checkout's target $TR_PA/target"
expect green "target-root: setup refuses a relative CARGO_TARGET_DIR naming another directory" \
    tr_setup_refused "$TR_A" out/x "CARGO_TARGET_DIR is 'out/x', not this checkout's target"
expect green "target-root: --print refuses a CARGO_TARGET_DIR naming another directory, before cargo" \
    tr_refused "$TR_A" "$TR_V" "Unset it for this checkout (env -u CARGO_TARGET_DIR just check)"
expect green "target-root: that refusal ran no cargo and wrote nothing" \
    sh -c '[ ! -s "$1" ] && [ ! -e "$2" ]' sh "$TRW/calls" "$TR_A/target"
tr_readonly() { # the root and the export are read-only in the shell that set them
    env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET bash -c '
. "$1/scripts/target-root.sh" && target_root_setup "$1" || exit 1
for v in TARGET_ROOT TARGET_ROOT_KEY TARGET_ROOT_CHECKOUT CARGO_TARGET_DIR; do
    if (eval "$v=x") 2>/dev/null; then echo "$v is writable"; exit 1; fi
done
target_root_setup "$1" || { echo "a second setup of the same checkout refused"; exit 1; }' sh "$TR_A"
}
expect green "target-root: setup's root, key, checkout and CARGO_TARGET_DIR are read-only" tr_readonly
tr_second_other() { # a second setup in one shell refuses another checkout's root
    env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET bash -c '
. "$1/scripts/target-root.sh" && target_root_setup "$1" && target_root_setup "$2"' sh "$TR_A" "$TR_B"
}
expect_red_because "target-root: a second setup in one shell refuses another checkout's root" \
    "CARGO_TARGET_DIR is '$TR_PA/target', not this checkout's target" tr_second_other
tr_kept() { # <checkout> <name>
    env -u CARGO_TARGET_DIR bash -c '. "$1/scripts/target-root.sh" && target_root_setup "$1" &&
target_root_kept "$2"' sh "$1" "$2"
}
expect green "target-root: the fuzz gate's kept directory is <root>/fuzz-check-<key>" \
    tr_is "$TR_PA/target/fuzz-check-$TR_KA" tr_kept "$TR_A" fuzz-check
expect green "target-root: the ratchet's kept directory is <root>/lint-ratchet-<key>" \
    tr_is "$TR_PA/target/lint-ratchet-$TR_KA" tr_kept "$TR_A" lint-ratchet
expect_red_because "target-root: a kept directory the list does not name is refused" \
    "is not a kept directory" tr_kept "$TR_A" target
# Check 1: settings read without cargo, in setup and --print.
for tr_var in CARGO_BUILD_BUILD_DIR CARGO_BUILD_TARGET; do
    expect green "target-root: setup refuses a set $tr_var" \
        tr_setup_refused "$TR_A" - "$tr_var is set" "$tr_var=x86_64-unknown-linux-gnu"
    expect green "target-root: setup refuses $tr_var set empty" \
        tr_setup_refused "$TR_A" - "$tr_var is set ('')" "$tr_var="
    tr_set_print() { # <variable>: --print exits 2 before cargo, writing nothing
        tr_refused "$TR_A" - "$1 is set" "$1=x86_64-unknown-linux-gnu" && [ ! -s "$TRW/calls" ] &&
            [ ! -e "$TR_A/target" ] && ! grep -q 'cargo clean' "$TRW/err"
    }
    expect green "target-root: --print under $tr_var exits 2 before cargo, writing nothing" \
        tr_set_print "$tr_var"
done
TR_CFG="$TRW/configured"
tr_checkout "$TR_CFG"
mkdir -p "$TR_CFG/.cargo"
printf '[build]\ntarget = "x86_64-unknown-linux-gnu"\n' >"$TR_CFG/.cargo/config.toml"
expect green "target-root: setup refuses a cargo config's build.target" \
    tr_setup_refused "$TR_CFG" - "$(tr_phys "$TR_CFG")/.cargo/config.toml sets build.target"
expect green "target-root: --print refuses a cargo config's build.target, before cargo" \
    tr_refused "$TR_CFG" - "sets build.target ('x86_64-unknown-linux-gnu')"
rm -rf "${TR_CFG:?}/.cargo"
# Checks 3 and 4: a link or a file at the root; a checkout inside a root.
TR_LN="$TRW/linked"
tr_checkout "$TR_LN"
mkdir -p "$TRW/elsewhere"
ln -s "$TRW/elsewhere" "$TR_LN/target"
expect green "target-root: a link at the root is refused" \
    tr_refused "$TR_LN" - "$(tr_phys "$TR_LN")/target is a link (to $TRW/elsewhere)"
expect green "target-root: setup refuses a link at the root" \
    tr_setup_refused "$TR_LN" - "is a link"
TR_FILE="$TRW/file-root"
tr_checkout "$TR_FILE"
: >"$TR_FILE/target"
expect green "target-root: a root that is a file is refused" \
    tr_refused "$TR_FILE" - "target exists and is not a directory"
TR_IN="$TR_PA/target/inner"
mkdir -p "$TR_PA/target"
: >"$TR_A/Cargo.toml"
tr_checkout "$TR_IN"
expect green "target-root: a checkout inside another checkout's target is refused" \
    tr_refused "$TR_IN" - "lies at or under $TR_PA/target, a target directory beside"
rm -rf "${TR_PA:?}/target" "${TR_A:?}/Cargo.toml"
TR_MK="$TRW/marked root"
mkdir -p "$TR_MK"
printf '%s\n' "$TRW/gone" >"$TR_MK/.checkout-path"
tr_checkout "$TR_MK/co"
expect green "target-root: a checkout inside a directory holding a marker is refused" \
    tr_setup_refused "$TR_MK/co" - "lies at or under $TR_MK, a target root (it holds .checkout-path)"
expect_red_because "target-root: --print or --clean alone is the command" \
    "usage: scripts/target-root.sh --print | --clean" "$TR_A/scripts/target-root.sh"
# --print: the marker.
tr_first() { # an absent root is made, marked, tagged and printed; cargo asked once, unset
    tr_prints "$TR_A" - "$TR_PA/target" || return 1
    [ "$(cat "$TR_A/target/.checkout-path")" = "$TR_PA" ] || { echo "marker"; return 1; }
    [ "$(head -n 1 "$TR_A/target/CACHEDIR.TAG")" = "Signature: 8a477f597d28d172789f06886806bc55" ] ||
        { echo "tag"; return 1; }
    [ "$(cat "$TRW/calls")" = "$(printf 'call\tmetadata\t--no-deps\t--offline\t--format-version\t1\tCARGO_TARGET_DIR=<unset>')" ] ||
        { echo "calls:"; cat "$TRW/calls"; return 1; }
}
expect green "target-root: an absent root is marked with the physical path, tagged and printed" tr_first
tr_again() {
    tr_prints "$TR_A" "$TR_PA/target" "$TR_PA/target" || return 1
    [ "$(ls -A "$TR_A/target" | tr '\n' ' ')" = ".checkout-path CACHEDIR.TAG " ] ||
        { ls -A "$TR_A/target"; return 1; }
    [ "$(cat "$TRW/calls")" = "$(printf 'call\tmetadata\t--no-deps\t--offline\t--format-version\t1\tCARGO_TARGET_DIR=<unset>')" ] ||
        { echo "calls (cargo metadata runs with CARGO_TARGET_DIR removed):"; cat "$TRW/calls"; return 1; }
}
expect green "target-root: a root marked for this checkout is printed, the variable removed for cargo" tr_again
tr_kept_only() {
    local dir="$TRW/kept-only"
    tr_checkout "$dir"
    mkdir -p "$dir/target/fuzz-check-$(tr_key "$dir")/x" "$dir/target/lint-ratchet-$(tr_key "$dir")"
    : >"$dir/target/.checkout-path.tmp.123"
    tr_prints "$dir" - "$(tr_phys "$dir")/target" || return 1
    [ "$(cat "$dir/target/.checkout-path")" = "$(tr_phys "$dir")" ]
}
expect green "target-root: a root holding only this checkout's kept directories and a temporary is marked" \
    tr_kept_only
TR_OUT="$TRW/the outputs"
tr_checkout "$TR_OUT"
mkdir -p "$TR_OUT/target/debug"
expect green "target-root: outputs with no marker are refused, printing the one --clean command, quoted" \
    tr_refused "$TR_OUT" - "^    cd '$TR_OUT' && ./scripts/target-root.sh --clean$"
expect green "target-root: a refused root gets no marker" test ! -e "$TR_OUT/target/.checkout-path"
tr_one_command() { # <stderr file>: the refusal prints one command, its own checkout's --clean
    [ "$(grep -c '^    ' "$1")" -eq 1 ] && grep -q '^    cd .* && ./scripts/target-root.sh --clean$' "$1" &&
        ! grep -q 'cargo clean\|rm -rf' "$1" || { cat "$1"; return 1; }
}
expect green "target-root: that refusal prints no other command and no path to remove" \
    tr_one_command "$TRW/err"
tr_clean_line() { # the refusal's printed command, run as printed: one path to cargo, then marked
    local line
    tr_print "$TR_OUT" - && return 1
    line=$(sed -n 's/^    \(cd .* && \.\/scripts\/target-root\.sh --clean\)$/\1/p' "$TRW/err")
    [ -n "$line" ] || { echo "no printed --clean line"; cat "$TRW/err"; return 1; }
    : >"$TRW/calls"
    env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET PATH="$TR_BIN:$PATH" \
        TR_CALLS="$TRW/calls" TR_WORK="$TRW" bash -c "$line" >"$TRW/out" 2>"$TRW/err" ||
        { cat "$TRW/err"; return 1; }
    [ "$(sed -n 2p "$TRW/calls")" = "$(printf 'call\tclean\t--target-dir\t%s\tCARGO_TARGET_DIR=<unset>' "$TR_OUT/target")" ] ||
        { echo "cargo's argv (metadata, then the clean):"; cat "$TRW/calls"; return 1; }
    [ "$(cat "$TRW/out")" = "$TR_OUT/target" ] && [ ! -e "$TR_OUT/target/debug" ] &&
        [ "$(cat "$TR_OUT/target/.checkout-path")" = "$(tr_phys "$TR_OUT")" ]
}
expect green "target-root: the printed --clean gives cargo one path, its root, and marks it" \
    tr_clean_line
TR_OK="$TRW/other-key"
tr_checkout "$TR_OK"
mkdir -p "$TR_OK/target/fuzz-check-0123456789abcdef"
expect green "target-root: another checkout's kept directory is output no marker names" \
    tr_refused "$TR_OK" - "holds fuzz-check-0123456789abcdef but no marker"
cp -a "$TR_A" "$TRW/copy"
expect green "target-root: a checkout copied with its target is refused, naming both paths" \
    tr_refused "$TRW/copy" - "holds the outputs of the checkout at $TR_PA, not of this one at $(tr_phys "$TRW/copy")"
expect green "target-root: the copy's refusal prints its own --clean alone" tr_one_command "$TRW/err"
mv "$TRW/copy" "$TRW/moved"
expect green "target-root: a checkout moved with its target is refused" \
    tr_refused "$TRW/moved" - "a checkout copied or moved with its target"
# --clean: checks 1-5 first, then the walk and the marker, then cargo's clean.
TR_CL="$TRW/clean me"
tr_checkout "$TR_CL"
TR_PCL=$(tr_phys "$TR_CL")
expect green "target-root: --clean refuses a CARGO_TARGET_DIR naming another directory" \
    tr_clean_refused "$TR_CL" "$TR_V" "CARGO_TARGET_DIR is"
expect green "target-root: --clean refuses a set CARGO_BUILD_TARGET" \
    tr_clean_refused "$TR_CL" - "CARGO_BUILD_TARGET is set" CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu
expect green "target-root: --clean refuses cargo's build directory apart from the root" \
    tr_clean_refused "$TR_CL" - "cargo's build directory is" TR_META=split
tr_clean_absent() { # an absent root: nothing to remove, marked and printed
    local status=0
    tr_clean "$TR_CL" - || status=$?
    [ "$status" -eq 0 ] && [ "$(cat "$TRW/out")" = "$TR_PCL/target" ] &&
        [ "$(cat "$TR_CL/target/.checkout-path")" = "$TR_PCL" ] &&
        ! grep -q "$(printf 'call\tclean')" "$TRW/calls" || { cat "$TRW/out" "$TRW/err" "$TRW/calls"; return 1; }
}
expect green "target-root: --clean on an absent root marks it, removing nothing" tr_clean_absent
tr_clean_outputs() { # a marked root with outputs: cleaned by cargo, then marked again
    local status=0
    mkdir -p "$TR_CL/target/debug/deps"
    tr_clean "$TR_CL" - || status=$?
    [ "$status" -eq 0 ] && [ "$(cat "$TRW/out")" = "$TR_PCL/target" ] && [ ! -e "$TR_CL/target/debug" ] &&
        [ "$(ls -A "$TR_CL/target" | tr '\n' ' ')" = ".checkout-path CACHEDIR.TAG " ] &&
        [ "$(sed -n 2p "$TRW/calls")" = "$(printf 'call\tclean\t--target-dir\t%s\tCARGO_TARGET_DIR=<unset>' "$TR_PCL/target")" ] ||
        { cat "$TRW/out" "$TRW/err" "$TRW/calls"; ls -A "$TR_CL/target"; return 1; }
}
expect green "target-root: --clean removes the root through cargo's clean, then marks it" tr_clean_outputs
tr_walk_case() { # <name> <path under the root> <pattern>: --print names it, --clean refuses it
    local dir="$TRW/walk-$1"
    tr_checkout "$dir"
    mkdir -p "$dir/target/debug" "$(dirname "$dir/target/$2")"
    : >"$dir/target/$2"
    : >"$dir/target/debug/kept"
    tr_refused "$dir" - "$3" || return 1
    ! grep -q '^    ' "$TRW/err" || { echo "a command was printed:"; cat "$TRW/err"; return 1; }
    tr_clean_refused "$dir" - "$3" || return 1
    [ -e "$dir/target/$2" ] && [ -e "$dir/target/debug/kept" ]
}
expect green "target-root: a .git below the root is named, no command printed; --clean refuses it" \
    tr_walk_case below-git "debug/wt/.git" "holds $TRW/walk-below-git/target/debug/wt/.git, a work tree or a repository"
expect green "target-root: a .git at the root's top is refused too" \
    tr_walk_case top-git ".git" "holds $TRW/walk-top-git/target/.git, a work tree or a repository"
expect green "target-root: another root's marker below the root is named; --clean refuses it" \
    tr_walk_case below-marker "debug/x/.checkout-path" "another checkout's target root"
tr_clean_walk_marked() { # a marked root: --clean still walks before removing
    local dir="$TRW/walk-marked"
    tr_checkout "$dir"
    tr_prints "$dir" - "$(tr_phys "$dir")/target" || return 1
    mkdir -p "$dir/target/debug/b"
    : >"$dir/target/debug/b/.git"
    tr_clean_refused "$dir" - "a work tree or a repository" && [ -e "$dir/target/debug/b/.git" ]
}
expect green "target-root: --clean on a marked root refuses a .git below it" tr_clean_walk_marked
tr_sharer() { # W's target is a link to R, R's marker names W: --print names W, no command; --clean refuses
    local dir="$TRW/shared-r" other="$TRW/sharer w"
    tr_checkout "$dir"
    tr_checkout "$other"
    mkdir -p "$dir/target/debug"
    tr_phys "$other" >"$dir/target/.checkout-path"
    ln -s "$dir/target" "$other/target"
    tr_refused "$dir" - "is also the target of the checkout at $(tr_phys "$other")" || return 1
    ! grep -q '^    ' "$TRW/err" || { echo "a command was printed:"; cat "$TRW/err"; return 1; }
    tr_clean_refused "$dir" - "is also the target of the checkout at" || return 1
    [ -d "$dir/target/debug" ]
}
expect green "target-root: a root that a live checkout's linked target shares is named; --clean refuses" \
    tr_sharer
tr_gone_writer() { # the marker names a checkout that is gone: a copy's root, cleaned by --clean
    local dir="$TRW/gone-writer"
    tr_checkout "$dir"
    mkdir -p "$dir/target/debug"
    printf '%s\n' "$TRW/no-such-checkout" >"$dir/target/.checkout-path"
    tr_refused "$dir" - "holds the outputs of the checkout at $TRW/no-such-checkout" || return 1
    tr_one_command "$TRW/err" || return 1
    tr_clean "$dir" - && [ ! -e "$dir/target/debug" ] && [ "$(cat "$dir/target/.checkout-path")" = "$(tr_phys "$dir")" ]
}
expect green "target-root: a root a gone checkout marked prints --clean, which cleans and marks it" \
    tr_gone_writer
# --print: where cargo says the builds land, the variable removed.
TR_M="$TRW/meta"
tr_checkout "$TR_M"
tr_meta() { # <TR_META> <pattern>: refused before any marker or clean
    tr_refused "$TR_M" - "$2" TR_META="$1" && [ ! -e "$TR_M/target" ] && ! grep -q 'cargo clean' "$TRW/err"
}
expect green "target-root: a metadata reply without build_directory is refused" \
    tr_meta missing "names no target or build directory as a path"
expect green "target-root: a build_directory that is no path is refused" \
    tr_meta number "names no target or build directory as a path"
expect green "target-root: a build directory apart from the root is refused, naming it and the settings" \
    tr_meta split "cargo's build directory is $TRW/bd, not this checkout's target $(tr_phys "$TR_M")/target: a cargo config's build.target-dir or build-dir"
expect green "target-root: a target directory other than the root is refused" \
    tr_meta elsewhere "cargo's target directory is $TRW/td, not this checkout's target"
expect green "target-root: a failing cargo metadata is refused with its message" \
    tr_meta fail "failed to parse manifest"
# The pinned cargo itself: a config's build-dir, and CARGO_BUILD_TARGET_DIR.
TR_CF="$TRW/config"
tr_checkout "$TR_CF"
mkdir -p "$TR_CF/src" "$TR_CF/.cargo"
printf '[package]\nname = "tr-config"\nversion = "0.0.0"\nedition = "2021"\n\n[workspace]\n' >"$TR_CF/Cargo.toml"
printf 'fn main() {}\n' >"$TR_CF/src/main.rs"
cp rust-toolchain.toml "$TR_CF/rust-toolchain.toml"
tr_real() { # --print on the real cargo: <want status> [<VAR=value>…]
    local want=$1 status=0
    shift
    env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET "$@" \
        "$TR_CF/scripts/target-root.sh" --print >"$TRW/out" 2>"$TRW/err" || status=$?
    [ "$status" -eq "$want" ] || { echo "exit $status, want $want"; cat "$TRW/out" "$TRW/err"; return 1; }
}
tr_real_config() {
    printf '[build]\nbuild-dir = "bd"\n' >"$TR_CF/.cargo/config.toml"
    tr_real 2 && [ ! -s "$TRW/out" ] && [ ! -e "$TR_CF/target" ] &&
        grep -q "cargo's build directory is $(tr_phys "$TR_CF")/bd" "$TRW/err" &&
        grep -q "Read here: $(tr_phys "$TR_CF")/.cargo/config.toml" "$TRW/err"
}
expect green "target-root: on the pinned cargo a config's build-dir is refused, naming the file" \
    tr_real_config
rm -f "$TR_CF/.cargo/config.toml"
tr_real_build_target_dir() {
    tr_real 2 CARGO_BUILD_TARGET_DIR="$TRW/btd" && [ ! -e "$TR_CF/target" ] &&
        grep -q "cargo's target directory is $TRW/btd" "$TRW/err" &&
        grep -q "Read here: CARGO_BUILD_TARGET_DIR='$TRW/btd'" "$TRW/err"
}
expect green "target-root: on the pinned cargo CARGO_BUILD_TARGET_DIR is refused, naming it" \
    tr_real_build_target_dir
tr_real_target_dir() {
    printf '[build]\ntarget-dir = "elsewhere"\n' >"$TR_CF/.cargo/config.toml"
    tr_real 2 && [ ! -e "$TR_CF/target" ] &&
        grep -q "cargo's target directory is $(tr_phys "$TR_CF")/elsewhere" "$TRW/err"
}
expect green "target-root: on the pinned cargo a config's build.target-dir is refused" tr_real_target_dir
tr_real_clean_build_dir() { # --clean under a config's build-dir that holds B's outputs: refused, B intact
    local b="$TRW/real-b" status=0
    tr_b "$b"
    tr_b_snap "$b"
    printf '[build]\nbuild-dir = "%s"\n' "$(tr_phys "$b")/target" >"$TR_CF/.cargo/config.toml"
    env -u CARGO_TARGET_DIR -u CARGO_BUILD_BUILD_DIR -u CARGO_BUILD_TARGET \
        "$TR_CF/scripts/target-root.sh" --clean >"$TRW/out" 2>"$TRW/err" || status=$?
    rm -f "$TR_CF/.cargo/config.toml"
    [ "$status" -eq 2 ] && grep -q "cargo's build directory is $(tr_phys "$b")/target" "$TRW/err" ||
        { echo "exit $status"; cat "$TRW/err"; return 1; }
    tr_b_intact "$b"
}
expect green "target-root: on the pinned cargo --clean under a build-dir holding B's outputs keeps them" \
    tr_real_clean_build_dir
tr_real_plain() {
    tr_real 0 && [ "$(cat "$TRW/out")" = "$(tr_phys "$TR_CF")/target" ]
}
expect green "target-root: on the pinned cargo, nothing set, the root is printed (the control)" tr_real_plain
# ADR-0204 D5: the nesting and removal legs. Each leg's B is a fixture
# checkout whose .git, marker and a file of its root are copied before the leg
# and compared after it; no entry of another checkout may print or run a
# command that changes them, on the stub cargo whose clean really removes.
TR_NA="$TRW/nest a"
tr_checkout "$TR_NA"
: >"$TR_NA/Cargo.toml"
TR_PNA=$(tr_phys "$TR_NA")
TR_NB="$TRW/nest-b"
tr_b "$TR_NB"
tr_b_snap "$TR_NB"
tr_b_on() { # <B's CARGO_TARGET_DIR>: refused in setup, --print and --clean, nothing written under A
    local before
    before=$(ls -A "$TR_NA" "$TR_NA/target" 2>/dev/null)
    tr_setup_refused "$TR_NB" "$1" "CARGO_TARGET_DIR is" || return 1
    tr_refused "$TR_NB" "$1" "CARGO_TARGET_DIR is" && tr_no_command "$TRW/err" || return 1
    tr_clean_refused "$TR_NB" "$1" "CARGO_TARGET_DIR is" || return 1
    [ "$(ls -A "$TR_NA" "$TR_NA/target" 2>/dev/null)" = "$before" ] || { echo "written under A"; return 1; }
    tr_b_intact "$TR_NB"
}
expect green "target-root: B on A's target is refused in each entry, nothing written under A" \
    tr_b_on "$TR_PNA/target"
expect green "target-root: B on a path inside A's target is refused in each entry" tr_b_on "$TR_PNA/target/x"
expect green "target-root: B on A's checkout is refused in each entry" tr_b_on "$TR_PNA"
expect green "target-root: B on a parent two checkouts share is refused in each entry" tr_b_on "$TRW/two share"
tr_two_on_one() { # two checkouts given one variable: each refused
    tr_setup_refused "$TR_A" "$TR_V" "CARGO_TARGET_DIR is" && tr_setup_refused "$TR_NB" "$TR_V" "CARGO_TARGET_DIR is" &&
        tr_refused "$TR_A" "$TR_V" "CARGO_TARGET_DIR is" && tr_refused "$TR_NB" "$TR_V" "CARGO_TARGET_DIR is"
}
expect green "target-root: two checkouts given one variable are each refused" tr_two_on_one
tr_prints "$TR_NA" - "$TR_PNA/target" >/dev/null
expect green "target-root: B on another checkout's root is refused in each entry" tr_b_on "$TR_PNA/target"
rm -rf "${TR_PNA:?}/target"
# A root nested as the per-variable rule made it: <A>/target/checkout-<kB>,
# holding B's marker and outputs, in a root from before the rule (no marker).
tr_nested_by_hand() {
    local nest="$TR_PNA/target/checkout-$(tr_key "$TR_NB")"
    mkdir -p "$nest/debug"
    tr_phys "$TR_NB" >"$nest/.checkout-path"
    printf 'built by B\n' >"$nest/debug/b-bin"
    cp "$nest/.checkout-path" "$TRW/nest-marker"
    tr_refused "$TR_NA" - "holds $nest/.checkout-path, another checkout's target root" || return 1
    tr_no_command "$TRW/err" || return 1
    tr_clean_refused "$TR_NA" - "another checkout's target root" || return 1
    tr_printed_run "$TRW/err"
    cmp -s "$nest/.checkout-path" "$TRW/nest-marker" && [ -f "$nest/debug/b-bin" ] || { echo "B's nested root changed"; return 1; }
}
expect green "target-root: a root nested in A's is named, no command printed, and --clean keeps it" \
    tr_nested_by_hand
rm -rf "${TR_PNA:?}/target"
# The dead-root fixture: under a variable, X's root, with Y's live root nested
# inside it, X's checkout gone; Z on the variable is refused, printing no path.
tr_dead_root() {
    local v="$TRW/dead v" y="$TRW/dead-y" z="$TRW/dead z" nest
    tr_checkout "$z"
    tr_b "$y"
    nest="$v/checkout-0123456789abcdef/checkout-$(tr_key "$y")"
    mkdir -p "$nest/debug"
    printf '%s\n' "$TRW/gone-x" >"$v/checkout-0123456789abcdef/.checkout-path"
    tr_phys "$y" >"$nest/.checkout-path"
    printf 'y\n' >"$nest/debug/b-bin"
    tr_refused "$z" "$v" "CARGO_TARGET_DIR is" && tr_no_command "$TRW/err" || return 1
    tr_printed_run "$TRW/err"
    [ -f "$nest/.checkout-path" ] && [ -f "$nest/debug/b-bin" ] && ! grep -q "$v/checkout" "$TRW/err"
}
expect green "target-root: on a variable holding a dead root with a live one nested, nothing is printed to remove" \
    tr_dead_root
# B's checkout inside A's root, as git worktree add or a clone puts it.
tr_checkout_in_root() {
    local inner="$TR_PNA/target/debug/wt"
    tr_b "$inner"
    tr_b_snap "$inner"
    tr_setup_refused "$inner" - "lies at or under $TR_PNA/target, a target directory beside" || return 1
    tr_refused "$inner" - "lies at or under" && tr_no_command "$TRW/err" || return 1
    tr_clean_refused "$inner" - "lies at or under" || return 1
    tr_refused "$TR_NA" - "holds $inner/.git, a work tree or a repository" && tr_no_command "$TRW/err" || return 1
    tr_clean_refused "$TR_NA" - "a work tree or a repository" || return 1
    tr_b_intact "$inner"
}
expect green "target-root: B's checkout inside A's root is refused, A names it, and --clean keeps it" \
    tr_checkout_in_root
rm -rf "${TR_PNA:?}/target"
# The copy's gates build their own keyed directories.
expect green "target-root: a copied checkout's gates keep directories under its own key" \
    tr_is "$(tr_phys "$TRW/moved")/target/fuzz-check-$(tr_key "$TRW/moved")" tr_kept "$TRW/moved" fuzz-check
# The triple and the config read, in each entry: no command printed, nothing written.
TR_HOST=$(rustc -vV | sed -n "s/^host: //p")
TR_TP="$TRW/triple parent"
TR_T="$TR_TP/co"
tr_checkout "$TR_T"
mkdir -p "$TRW/cargo-home"
tr_triple() { # <label> <pattern> [<VAR=value>…]: setup, --print and --clean refuse it
    local label=$1 pattern=$2
    shift 2
    expect green "target-root: setup refuses $label" tr_setup_refused "$TR_T" - "$pattern" "$@"
    tr_triple_print() { tr_refused "$TR_T" - "$@" && tr_no_command "$TRW/err" && [ ! -e "$TR_T/target" ]; }
    expect green "target-root: --print refuses $label, writing nothing" tr_triple_print "$pattern" "$@"
    expect green "target-root: --clean refuses $label" tr_clean_refused "$TR_T" - "$pattern" "$@"
}
tr_triple "CARGO_BUILD_TARGET set to the host's triple" "CARGO_BUILD_TARGET is set" CARGO_BUILD_TARGET="$TR_HOST"
tr_triple "CARGO_BUILD_TARGET set empty" "CARGO_BUILD_TARGET is set ('')" CARGO_BUILD_TARGET=
mkdir -p "$TR_T/.cargo"
printf '[build]\ntarget = "%s"\n' "$TR_HOST" >"$TR_T/.cargo/config.toml"
tr_triple "build.target in the checkout's config.toml" "$TR_T/.cargo/config.toml sets build.target"
rm -f "$TR_T/.cargo/config.toml"
printf 'build.target = "%s"\n' "$TR_HOST" >"$TR_T/.cargo/config"
tr_triple "build.target as a dotted key in a config without extension" "$TR_T/.cargo/config sets build.target"
rm -f "$TR_T/.cargo/config"
mkdir -p "$TR_TP/.cargo"
printf '[build]\ntarget = ["%s"]\n' "$TR_HOST" >"$TR_TP/.cargo/config.toml"
tr_triple "build.target as an array in a directory above the checkout" "$TR_TP/.cargo/config.toml sets build.target"
rm -rf "${TR_TP:?}/.cargo"
printf '[build]\ntarget = "%s"\n' "$TR_HOST" >"$TRW/cargo-home/config.toml"
tr_triple "build.target in CARGO_HOME's config" "cargo-home/config.toml sets build.target" CARGO_HOME="$TRW/cargo-home"
printf 'include = ["extra.toml"]\n' >"$TRW/cargo-home/config.toml"
tr_triple "a config holding an include" "holds an include, which cargo follows" CARGO_HOME="$TRW/cargo-home"
printf '[build\n' >"$TRW/cargo-home/config.toml"
tr_triple "a config that cannot be parsed" "cannot be read or parsed" CARGO_HOME="$TRW/cargo-home"
rm -f "$TRW/cargo-home/config.toml"
expect green "target-root: the same checkout, nothing set, is printed (the triple legs' control)" \
    tr_prints "$TR_T" - "$(tr_phys "$TR_T")/target"
# target_root_cargo_fresh: a per-run build, cold. The recorder prints the
# variable and its arguments; "ran" says the command ran.
printf '%s\n' '#!/bin/sh' 'printf "ran CARGO_TARGET_DIR=%s FOO=%s argv=%s\n" "$CARGO_TARGET_DIR" "${FOO-<unset>}" "$*"' \
    >"$TR_BIN/recorder"
chmod +x "$TR_BIN/recorder"
TR_FR="$TRW/fresh"
mkdir -p "$TR_FR"
tr_fresh() { # <dir> <args…>: the function in a shell of its own, FOO=1 and a read-only variable
    env FOO=1 CARGO_TARGET_DIR="$TRW/caller" bash -c '. "$1/scripts/target-root.sh"; readonly CARGO_TARGET_DIR
shift; target_root_cargo_fresh "$@"' sh "$TR_A" "$@"
}
expect green "target-root: a fresh build on an absent directory runs there, under a read-only variable" \
    tr_is "ran CARGO_TARGET_DIR=$TR_FR/absent FOO=1 argv=a b" tr_fresh "$TR_FR/absent" "$TR_BIN/recorder" a b
mkdir -p "$TR_FR/empty"
expect green "target-root: a fresh build on an empty directory runs there" \
    tr_is "ran CARGO_TARGET_DIR=$TR_FR/empty FOO=1 argv=" tr_fresh "$TR_FR/empty" "$TR_BIN/recorder"
expect green "target-root: an env -u prefix stays in the argv, the variable set beside it" \
    tr_is "ran CARGO_TARGET_DIR=$TR_FR/absent FOO=<unset> argv=x" \
    tr_fresh "$TR_FR/absent" env -u FOO "$TR_BIN/recorder" x
mkdir -p "$TR_FR/full"
: >"$TR_FR/full/earlier"
tr_fresh_refused() { # <pattern> <dir> <args…>: exit 2, nothing ran
    local pattern=$1 status=0
    shift
    tr_fresh "$@" >"$TRW/out" 2>"$TRW/err" || status=$?
    [ "$status" -eq 2 ] && ! grep -q '^ran' "$TRW/out" && grep -q -- "$pattern" "$TRW/err" && return 0
    echo "exit $status, want 2 naming '$pattern':"
    cat "$TRW/out" "$TRW/err"
    return 1
}
expect green "target-root: a fresh build refuses a directory an earlier run filled, running nothing" \
    tr_fresh_refused "holds an earlier run's entries" "$TR_FR/full" "$TR_BIN/recorder"
: >"$TR_FR/file"
expect green "target-root: a fresh build refuses a file at its directory" \
    tr_fresh_refused "is not a directory" "$TR_FR/file" "$TR_BIN/recorder"
ln -s "$TR_FR/empty" "$TR_FR/link"
expect green "target-root: a fresh build refuses a link at its directory" \
    tr_fresh_refused "is not a directory" "$TR_FR/link" "$TR_BIN/recorder"
for tr_arg in --target-dir --target-dir=x --config --config=build.target-dir=x CARGO_TARGET_DIR=x; do
    expect green "target-root: a fresh build refuses an argv naming $tr_arg, which outranks its directory" \
        tr_fresh_refused "names '$tr_arg'" "$TR_FR/absent" "$TR_BIN/recorder" "$tr_arg"
done
expect green "target-root: a fresh build refuses an env -u CARGO_TARGET_DIR, which undoes its directory" \
    tr_fresh_refused "names 'CARGO_TARGET_DIR'" "$TR_FR/absent" env -u CARGO_TARGET_DIR "$TR_BIN/recorder"
expect green "target-root: a fresh build with no command is refused" \
    tr_fresh_refused "usage: target_root_cargo_fresh" "$TR_FR/absent"
# The helper's canaries: a copy with one rule removed fails its case.
tr_mutant() { # <name> <sed expression> <case function> <what the copy does>
    local dir="$TRW/mutant-$1"
    tr_checkout "$dir"
    sed "$2" "$SCRIPT_DIR/target-root.sh" >"$dir/scripts/target-root.sh"
    if cmp -s "$SCRIPT_DIR/target-root.sh" "$dir/scripts/target-root.sh"; then
        echo "selftest: the target-root mutant '$1' changed nothing" >&2
        exit 2
    fi
    expect red "target-root: the case is red on a helper that $4" "$3" "$dir"
}
tr_copy_case() { # <helper dir>: its copy, copied with a marked target, is refused
    tr_prints "$1" - "$(tr_phys "$1")/target" || return 1
    rm -rf "${1:?}-copy"
    cp -a "$1" "$1-copy"
    tr_refused "$1-copy" - "a checkout copied or moved with its target"
}
tr_outputs_case() { # <helper dir>: outputs with no marker are refused
    mkdir -p "$1/target/debug"
    tr_refused "$1" - "but no marker naming this checkout"
}
tr_split_case() { tr_refused "$1" - "cargo's build directory is" TR_META=split; }
tr_kept_case() { tr_is "$(tr_phys "$1")/target/fuzz-check-$(tr_key "$1")" tr_kept "$1" fuzz-check; }
tr_variable_case() { tr_refused "$1" "$TR_V" "CARGO_TARGET_DIR is"; }
tr_control() { # <name> <case function>: the case is green on the helper as it is
    local dir="$TRW/control-$1"
    tr_checkout "$dir"
    expect green "target-root: the $1 case is green on the helper as it is (the control)" "$2" "$dir"
}
tr_control marker tr_copy_case
tr_control outputs tr_outputs_case
tr_control build-dir tr_split_case
tr_control unkeyed tr_kept_case
tr_control variable tr_variable_case
tr_walk_mutant_case() { # <helper dir>: --clean refuses a root holding a work tree
    mkdir -p "$1/target/debug/wt"
    : >"$1/target/debug/wt/.git"
    tr_clean_refused "$1" - "a work tree or a repository" && [ -e "$1/target/debug/wt/.git" ]
}
tr_control walk tr_walk_mutant_case
tr_fresh_case() { # <helper dir>: its fresh build refuses a directory an earlier run filled
    local status=0
    env bash -c '. "$1/scripts/target-root.sh"; target_root_cargo_fresh "$2" "$3"' sh "$1" "$TR_FR/full" \
        "$TR_BIN/recorder" >"$TRW/out" 2>&1 || status=$?
    [ "$status" -eq 2 ] && ! grep -q '^ran' "$TRW/out"
}
tr_control fresh tr_fresh_case
tr_mutant fresh 's/^    if \[ -d "\$dir" \] && \[ -n "\$(ls -A "\$dir")" \]; then$/    if false; then/' tr_fresh_case \
    "builds in a directory an earlier run filled"
tr_mutant marker 's/^if writer is not None and writer != phys + "\\n":$/if False:/' tr_copy_case \
    "skips the marker's checkout"
tr_mutant walk 's/^            if name == ".git" or (name == MARKER and here != root):$/            if False:/' \
    tr_walk_mutant_case "removes a root that holds a work tree"
tr_mutant outputs 's/^        if others:$/        if False:/' tr_outputs_case "stamps a root holding outputs"
tr_mutant build-dir 's/^    if os.path.realpath(path) != os.path.realpath(root):$/    if False:/' tr_split_case \
    "skips the build directory"
tr_mutant unkeyed 's|"$TARGET_ROOT/$1-$TARGET_ROOT_KEY"|"$TARGET_ROOT/$1"|' tr_kept_case \
    "keeps an unkeyed directory"
tr_mutant variable 's/^    if named != os.path.realpath(root):$/    if False:/' tr_variable_case \
    "follows a CARGO_TARGET_DIR naming another directory"
}

# --------------------------------------------------- cargo output-layout census
group_target_dirs() {
# ADR-0204 D4: scripts/target_dirs.py on this tree, then on fixture
# repositories (each its own `git init`, its files untracked unless a case
# commits them), each with a table of its own. A plant is red for its own
# reason; a row that holds it is green.
TD="$SCRIPT_DIR/target_dirs.py"
TDW="$work/td"
mkdir -p "$TDW"
td() { python3 -I "$TD" "$@"; }
expect green "target-dirs: this tree is green" td .
expect_output "target-dirs: the OK line counts the files read, the lines and the rows by kind" \
    "target-dirs census OK: [0-9][0-9]* files git lists read .*; [0-9][0-9]* lines naming cargo's output layout in [0-9][0-9]* rows and 0 cargo config files below the checkout, each a row: export [0-9]" \
    td .
# A row of this tree's table removed: the tree is red on that row's lines.
td_row=$(grep -v '^#' scripts/target-dirs.tsv | grep -m1 $'\troot\t')
td_file=${td_row%%$'\t'*}
grep -vxF -- "$td_row" scripts/target-dirs.tsv >"$TDW/short.tsv"
expect_red_because "target-dirs: this tree with a root row removed is red on its line" \
    "^target-dirs census: $td_file:[0-9]*: names cargo's output layout and no row" td . "$TDW/short.tsv"
td_n=0
td_tree() { # <file> <text…>: a fixture repository holding <file> with each text a line, and an empty table
    local file=$1
    shift
    td_n=$((td_n + 1))
    TD_DIR="$TDW/t$td_n"
    mkdir -p "$TD_DIR/scripts" "$(dirname "$TD_DIR/$file")"
    git -C "$TD_DIR" init -q
    printf '# file\tkind\tcount\treason\ttext\n' >"$TD_DIR/scripts/target-dirs.tsv"
    printf '%s\n' "$@" >"$TD_DIR/$file"
}
td_row() { # <file> <kind> <count> <text>: a row of the fixture's table
    printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "a fixture row" "$4" >>"$TD_DIR/scripts/target-dirs.tsv"
}
# Each name, in each kind of file, is a line the table must hold.
TD_NAMES="target-dir build-dir --target build.target CARGO_TARGET_DIR CARGO_BUILD_TARGET CARGO_BUILD_BUILD_DIR --config CARGO_HOME .cargo/config"
td_every_name() { # <dir> <file> <count>: red on each of the file's lines, one a name, <count> in all
    local status=0 n name
    td "$1" >"$TDW/out" 2>&1 || status=$?
    [ "$status" -eq 1 ] || { echo "exit $status"; cat "$TDW/out"; return 1; }
    n=0
    for name in $TD_NAMES; do
        n=$((n + 1))
        grep -q "^target-dirs census: $2:$n: names cargo's output layout" "$TDW/out" ||
            { echo "line $n ($name) not red:"; cat "$TDW/out"; return 1; }
    done
    [ "$(grep -c "names cargo's output layout" "$TDW/out")" -eq "$3" ] || { cat "$TDW/out"; return 1; }
}
for td_kind in scripts/plant.sh justfile scripts/tool.py src/lib.rs .github/workflows/ci.yml Cargo.toml; do
    td_lines=()
    for td_name in $TD_NAMES; do td_lines+=("  x $td_name y"); done
    td_tree "$td_kind" "${td_lines[@]}"
    expect green "target-dirs: each name in $td_kind is red, each line named" \
        td_every_name "$TD_DIR" "$td_kind" 10
    for td_name in $TD_NAMES; do td_row "$td_kind" triple 1 "x $td_name y"; done
    expect green "target-dirs: each line in $td_kind held by its row (leading blanks trimmed) is green" td "$TD_DIR"
done
td_tree scripts/a.sh 'echo one'
expect green "target-dirs: a tree with no output-layout line and an empty table is green" td "$TD_DIR"
td_tree scripts/a.sh 'cargo build --target-dir "$x"'
td_row scripts/a.sh root 1 'cargo build --target-dir "$x"'
expect_red_because "target-dirs: a root row whose text does not hold \$TARGET_ROOT is red" \
    "a root row's text holds \$TARGET_ROOT" td "$TD_DIR"
td_tree scripts/a.sh 'cargo build --target-dir "$TARGET_ROOT/x"'
td_row scripts/a.sh root 1 'cargo build --target-dir "$TARGET_ROOT/x"'
expect green "target-dirs: a root row holding \$TARGET_ROOT is green" td "$TD_DIR"
td_tree scripts/a.sh 'env CARGO_TARGET_DIR="$d" cargo build'
td_row scripts/a.sh fresh 1 'env CARGO_TARGET_DIR="$d" cargo build'
expect_red_because "target-dirs: a fresh row whose text does not call target_root_cargo_fresh is red" \
    "a fresh row's text holds target_root_cargo_fresh" td "$TD_DIR"
td_tree scripts/a.sh 'x --target-dir y'
td_row scripts/a.sh comment 1 'x --target-dir y'
expect_red_because "target-dirs: a comment row whose text is not a comment is red" \
    "a comment row's text starts with # or //" td "$TD_DIR"
td_tree scripts/a.sh 'echo "$CARGO_TARGET_DIR"'
td_row scripts/a.sh export 1 'echo "$CARGO_TARGET_DIR"'
expect_red_because "target-dirs: an export row outside the justfile is red" "the export's rows are the justfile's" \
    td "$TD_DIR"
td_tree scripts/a.sh 'x --config y'
td_row scripts/a.sh maybe 1 'x --config y'
expect_red_because "target-dirs: a row of an unknown kind is red" "the kind 'maybe' is not one of" td "$TD_DIR"
td_tree scripts/a.sh 'x --config y' 'x --config y'
td_row scripts/a.sh triple 1 'x --config y'
expect_red_because "target-dirs: a row whose count is below its lines is red" \
    "the row matches 2 line(s), its count is 1" td "$TD_DIR"
td_tree scripts/a.sh 'x --config y'
td_row scripts/a.sh triple 2 'x --config y'
expect_red_because "target-dirs: a row whose count is above its lines is red" \
    "the row matches 1 line(s), its count is 2" td "$TD_DIR"
td_tree scripts/a.sh 'echo one'
td_row scripts/a.sh triple 1 'x --config y'
expect_red_because "target-dirs: a row that no line matches is red" "the row matches 0 line(s)" td "$TD_DIR"
td_tree scripts/a.sh 'x --config y'
td_row scripts/a.sh triple 1 'x --config y'
td_row scripts/a.sh triple 1 'x --config y'
expect_red_because "target-dirs: a second row for one line is red" "a second row for scripts/a.sh" td "$TD_DIR"
td_tree scripts/a.sh 'x --config y'
printf 'scripts/a.sh\ttriple\t1\t \tx --config y\n' >>"$TD_DIR/scripts/target-dirs.tsv"
expect_red_because "target-dirs: a row with no reason is red" "the row gives no reason" td "$TD_DIR"
td_tree scripts/a.sh 'x --config y'
printf 'scripts/a.sh\ttriple\t1\tx --config y\n' >>"$TD_DIR/scripts/target-dirs.tsv"
expect_red_because "target-dirs: a row of four fields is a scope error" "SCOPE ERROR: scripts/target-dirs.tsv:2: 4 fields" \
    td "$TD_DIR"
td_tree scripts/a.sh '    x --config y'
td_row scripts/a.sh triple 1 'x --config y'
printf '\t  x --config y  \n' >"$TD_DIR/scripts/a.sh"
expect green "target-dirs: a row's line re-indented is still held" td "$TD_DIR"
td_tree scripts/a.sh 'x --config y'
td_row scripts/a.sh triple 1 'x --config y'
printf 'x --config z\n' >>"$TD_DIR/scripts/a.sh"
expect_red_because "target-dirs: a new line beside a held one is red, naming it" \
    "scripts/a.sh:2: names cargo's output layout and no row" td "$TD_DIR"
# The files the census reads: untracked ones too; Markdown and the rule's own files not.
td_tree notes.txt 'try CARGO_TARGET_DIR=/x'
expect_red_because "target-dirs: an untracked file naming a setting is red" "notes.txt:1: names cargo's output layout" \
    td "$TD_DIR"
printf 'notes.txt\n' >"$TD_DIR/.gitignore"
expect green "target-dirs: the same file, git-ignored, is not read" td "$TD_DIR"
td_tree scripts/a.sh 'echo one'
printf 'set CARGO_TARGET_DIR\n' >"$TD_DIR/README.md"
printf 'CARGO_TARGET_DIR\n' >"$TD_DIR/scripts/target-root.sh"
printf 'CARGO_TARGET_DIR\n' >"$TD_DIR/scripts/check-scripts-selftest.sh"
printf 'CARGO_TARGET_DIR\n' >"$TD_DIR/scripts/target_dirs.py"
expect green "target-dirs: Markdown, the helper, the self-test and the census itself are not read" td "$TD_DIR"
# A cargo config file below the checkout, whatever its keys spell.
td_tree scripts/a.sh 'echo one'
mkdir -p "$TD_DIR/.cargo" "$TD_DIR/sub/.cargo"
printf '[alias]\nb = "build"\n' >"$TD_DIR/.cargo/config.toml"
expect_red_because "target-dirs: a .cargo/config.toml below the checkout is red" \
    "^target-dirs census: .cargo/config.toml: a cargo config file below the checkout" td "$TD_DIR"
td_row .cargo/config.toml config 1 ''
expect green "target-dirs: a config row naming it is green" td "$TD_DIR"
printf '[build]\njobs = 1\n' >"$TD_DIR/sub/.cargo/config"
expect_red_because "target-dirs: a .cargo/config in a subdirectory is red" \
    "^target-dirs census: sub/.cargo/config: a cargo config file below the checkout" td "$TD_DIR"
rm -f "$TD_DIR/sub/.cargo/config" "$TD_DIR/.cargo/config.toml"
expect_red_because "target-dirs: a config row whose file is gone is red" "a config file that git does not list" \
    td "$TD_DIR"
# Scope: a work tree's top, the table, a file to read.
mkdir -p "$TDW/not-a-repo/scripts"
printf '# file\tkind\tcount\treason\ttext\n' >"$TDW/not-a-repo/scripts/target-dirs.tsv"
expect_red_because "target-dirs: a directory that is not a work tree's top is a scope error" \
    "SCOPE ERROR: .* is not the top of a git work tree" td "$TDW/not-a-repo"
td_tree scripts/a.sh 'echo one'
rm -f "$TD_DIR/scripts/target-dirs.tsv"
expect_red_because "target-dirs: a missing table is a scope error" "SCOPE ERROR: cannot read scripts/target-dirs.tsv" \
    td "$TD_DIR"
td_tree README.md 'nothing'
expect_red_because "target-dirs: a tree whose every file is skipped is a scope error" \
    "SCOPE ERROR: git lists no file to read" td "$TD_DIR"
}

# ---------------------------------------------------------- nextest-config
group_nextest_config() {
# ADR-0207 D1-D3, ADR-0200 D4, D7: scripts/check-nextest-config.sh on fixture
# workspaces
# (each its own `git init`, its files untracked), each built green and given
# one plant. A plant is red for its own reason; a sanctioned shape is green.
NC="$SCRIPT_DIR/check-nextest-config.sh"
NCW="$work/nc"
mkdir -p "$NCW"
ncfg() { env INF_CHECK_ROOT="$1" "$NC"; }
nc_n=0
# nc_tree: a fresh fixture workspace in NC_DIR, green as built. Package fx has
# two test targets that both include tests/support/mod.rs; package fxd has a
# binary and two test targets, one run alone as a whole, its marker below an
# inner attribute that spans lines, the other holding a long test.
nc_tree() {
    nc_n=$((nc_n + 1))
    NC_DIR="$NCW/w$nc_n"
    NC_CFG="$NC_DIR/.config/nextest.toml"
    mkdir -p "$NC_DIR/.config" "$NC_DIR/.github/workflows" "$NC_DIR/crates/fx/src" \
        "$NC_DIR/crates/fx/tests/support" "$NC_DIR/bins/fxd/src" "$NC_DIR/bins/fxd/tests"
    git -C "$NC_DIR" init -q
    cp rust-toolchain.toml "$NC_DIR/"
    printf '[workspace]\nmembers = ["crates/fx", "bins/fxd"]\nresolver = "2"\n' >"$NC_DIR/Cargo.toml"
    printf '[package]\nname = "fx"\nversion = "0.0.0"\nedition = "2021"\n' >"$NC_DIR/crates/fx/Cargo.toml"
    printf '[package]\nname = "fxd"\nversion = "0.0.0"\nedition = "2021"\n' >"$NC_DIR/bins/fxd/Cargo.toml"
    : >"$NC_DIR/crates/fx/src/lib.rs"
    printf 'fn main() {}\n' >"$NC_DIR/bins/fxd/src/main.rs"
    cat >"$NC_DIR/crates/fx/tests/a.rs" <<'EOF'
mod support;

const TEXT: &str = "static FAKE: Mutex<()> in a string is no lock";

// nextest-group: serial
#[allow(dead_code)]
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_one() {
    let _ = (TEXT, &SERIAL, &support::SPAWN);
}

// nextest-alone: 1.5 s. A fixture test whose verdict reads what the lane
// shares.
#[test]
fn a_alone() {}
EOF
    cat >"$NC_DIR/crates/fx/tests/b.rs" <<'EOF'
mod support;

#[test]
fn b_one() {
    let _ = &support::SPAWN;
}
EOF
    cat >"$NC_DIR/crates/fx/tests/support/mod.rs" <<'EOF'
// nextest-in-process: a race between the threads of one test process
pub static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
EOF
    cat >"$NC_DIR/bins/fxd/tests/boot.rs" <<'EOF'
#![allow(
    clippy::disallowed_methods,
    reason = "a fixture: an inner attribute spanning lines"
)]
//! Creates a namespace with MEM-BUDGET 8mb and writes nothing to it.

// nextest-alone: 3.5 s. The whole target reads a per-user budget.
// nextest-stall-default: no write reaches a window, so no write parks.

const CREATE: [&[u8]; 2] = [b"MEM-BUDGET", b"8mb"];

#[test]
fn boots() {
    let _ = CREATE;
}
EOF
    cat >"$NC_DIR/bins/fxd/tests/tiered.rs" <<'EOF'
//! Writes a namespace with MEM-BUDGET 8mb and TAIL-STALL-TIMEOUT 60000.

const CREATE: [&[u8]; 4] = [b"MEM-BUDGET", b"8mb", b"TAIL-STALL-TIMEOUT", b"60000"];

#[test]
fn tiered() {
    let _ = CREATE;
}

// nextest-long: 20 s
#[test]
fn tiered_storm() {}
EOF
    cat >"$NC_CFG" <<'EOF'
nextest-version = { required = "0.9.150" }

[test-groups]
serial = { max-threads = 1 }

[profile.default]
leak-timeout = { period = "100ms", result = "fail" }

[[profile.default.overrides]]
filter = 'binary_id(fxd::boot)'
threads-required = "num-test-threads"
priority = 100

[[profile.default.overrides]]
filter = 'binary_id(fx::a) & test(=a_alone)'
threads-required = "num-test-threads"
priority = 100

[[profile.default.overrides]]
filter = 'binary_id(fx::a)'
test-group = "serial"
priority = 50

[[profile.default.overrides]]
filter = 'binary_id(fxd::tiered) & test(=tiered_storm)'
priority = 20
EOF
    cat >"$NC_DIR/.github/workflows/ci.yml" <<'EOF'
jobs:
  test:
    steps:
      - run: cargo install --locked cargo-nextest --version 0.9.150
      - run: cargo nextest run --workspace
EOF
}
# nc_sub <file> <old> <new>: every <old> in the file replaced; one must be there.
nc_sub() {
    python3 -I -c '
import pathlib, sys
path, old, new = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
text = path.read_text()
if old not in text:
    sys.exit(f"nc_sub: {old!r} is not in {path}")
path.write_text(text.replace(old, new))' "$@"
}
NC_GROUP=$'[[profile.default.overrides]]\nfilter = \'binary_id(fx::a)\'\n'
NC_LONG=$'[[profile.default.overrides]]\nfilter = \'binary_id(fxd::tiered) & test(=tiered_storm)\'\npriority = 20\n'

nc_tree
expect green "nextest-config: the fixture workspace is green" ncfg "$NC_DIR"
expect_output "nextest-config: the OK line counts each rule's population" \
    "nextest-config OK: nextest-version 0.9.150; 1 groups of one thread (serial), 1 group overrides at priority 50; 2 run alone at priority 100, first (fx::a/a_alone, fxd::boot); 1 long, each at its marker's seconds, 49 at most; 2 static locks, 1 grouped and 1 in-process; 2 test files name MEM-BUDGET, 1 state TAIL-STALL-TIMEOUT and 1 carry nextest-stall-default; no free-space read in 5 integration-test files and 1 library files they link; 1 cargo-nextest installs in CI at 0.9.150, each --locked, one in each of the 1 jobs that run it; [0-9]* .rs files of 6 test binaries read" \
    ncfg "$NC_DIR"
# The config holds its listed keys only, in their listed shapes.
nc_tree
nc_sub "$NC_CFG" 'nextest-version =' $'experimental = ["setup-scripts"]\nnextest-version ='
expect_red_because "nextest-config: a top-level key outside the list is red" \
    "top-level key .experimental. is outside the list" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'leak-timeout =' $'retries = 2\nleak-timeout ='
expect_red_because "nextest-config: a retry setting in the profile is red" \
    "profile key .retries. is outside the list" ncfg "$NC_DIR"
nc_tree
printf '\n[profile.ci]\nfail-fast = false\n' >>"$NC_CFG"
expect_red_because "nextest-config: a second profile is red" \
    "holds a profile other than .default." ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'serial = { max-threads = 1 }' 'serial = { max-threads = 2 }'
expect_red_because "nextest-config: a group of two threads is red" \
    "test group .serial. is not" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'result = "fail"' 'result = "pass"'
expect_red_because "nextest-config: a leak that passes is red" "leak-timeout. is not" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" '{ required = "0.9.150" }' '"0.9.150"'
expect_red_because "nextest-config: a version floor that is a bare string is red" \
    "nextest-version. is not" ncfg "$NC_DIR"
# The overrides: their two shapes, their priorities, their order.
nc_tree
nc_sub "$NC_CFG" 'priority = 100' 'priority = 50'
expect_red_because "nextest-config: a run-alone override without priority 100 is red (canary)" \
    "a run-alone override sets priority 100, not 50" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'priority = 50' 'priority = 100'
expect_red_because "nextest-config: a group's override without priority 50 is red (canary)" \
    "a group's override sets priority 50, not 100" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" $'\n'"$NC_GROUP"$'test-group = "serial"\npriority = 50\n' ''
nc_sub "$NC_CFG" $'\n[[profile.default.overrides]]\nfilter = \'binary_id(fxd::boot)\'' \
    $'\n'"$NC_GROUP"$'test-group = "serial"\npriority = 50\n\n[[profile.default.overrides]]\nfilter = \'binary_id(fxd::boot)\''
expect_red_because "nextest-config: a run-alone override after a group's is red (canary)" \
    "run-alone override 3 comes after group override 1" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'priority = 50' $'priority = 50\nslow-timeout = "60s"'
expect_red_because "nextest-config: an override with a timeout is red" "holds keys" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'test-group = "serial"' 'test-group = ["serial"]'
expect_red_because "nextest-config: a list-valued test group is red, not a traceback" \
    "override 3: test-group has the wrong type" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'priority = 50' 'priority = 50.0'
expect_red_because "nextest-config: a priority that is no integer is red" \
    "override 3: priority has the wrong type" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'threads-required = "num-test-threads"' 'threads-required = "num-cpus"'
expect_red_because "nextest-config: a run-alone override short of every slot is red" \
    'threads-required. is not "num-test-threads"' ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" "filter = 'binary_id(fxd::boot)'" "filter = 'test(boots)'"
expect_red_because "nextest-config: a filter in none of the forms is red" "is in none of the forms" \
    ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'binary_id(fxd::boot)' 'binary_id(fxd::gone)'
expect_red_because "nextest-config: a filter naming no test binary is red" \
    "filter names test binary .fxd::gone., which cargo metadata lacks" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" "filter = 'binary_id(fx::a)'" "filter = 'package(nope)'"
expect_red_because "nextest-config: a filter naming no package is red" \
    "filter names package .nope., which cargo metadata lacks" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" "filter = 'binary_id(fx::a)'" "filter = 'package(fx)'"
expect_red_because "nextest-config: a group override selecting a binary that holds no such lock is red" \
    "override 3 puts .fx::b. in group .serial., and no lock that binary holds" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/a.rs" $'// nextest-group: serial\n#[allow(dead_code)]\nstatic SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());\n' ''
nc_sub "$NC_DIR/crates/fx/tests/a.rs" '(TEXT, &SERIAL, &support::SPAWN)' '(TEXT, &support::SPAWN)'
expect_red_because "nextest-config: a group override left behind by its lock is red" \
    "override 3 puts .fx::a. in group .serial., and no lock that binary holds" ncfg "$NC_DIR"
# The static locks: each marked, each group's marker paired with an override
# that selects every binary compiling the file.
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/a.rs" '// nextest-group: serial' '// the fixture lock'
expect_red_because "nextest-config: an unmarked static lock is red" "a.rs:7: a static lock with no" ncfg "$NC_DIR"
nc_tree
printf 'static WAKE: std::sync::Condvar = std::sync::Condvar::new();\n' >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: an unmarked static Condvar is red" "b.rs:7: a static lock with no" \
    ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/support/mod.rs" \
    '// nextest-in-process: a race between the threads of one test process' '// nextest-group: serial'
expect_red_because "nextest-config: a grouped lock in an included file binds each binary that compiles it" \
    "test binary .fx::b. compiles this lock, and no override puts it in group .serial." ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/a.rs" 'nextest-group: serial' 'nextest-group: nogroup'
expect_red_because "nextest-config: a lock's marker naming no group is red" \
    "names test group .nogroup., which .test-groups. lacks" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'serial = { max-threads = 1 }' $'serial = { max-threads = 1 }\nspare = { max-threads = 1 }'
expect_red_because "nextest-config: a group no marker names is red" \
    "test group .spare. is named by no lock's" ncfg "$NC_DIR"
nc_tree
printf '// nextest-group: serial\nconst NOT_A_LOCK: u8 = 0;\n' >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: a group marker on no static lock is red" \
    "b.rs:7: a .nextest-group:. marker on no static lock" ncfg "$NC_DIR"
# The run-alone markers and their overrides.
nc_tree
nc_sub "$NC_CFG" 'binary_id(fx::a) & test(=a_alone)' 'binary_id(fx::b) & test(=b_one)'
expect_red_because "nextest-config: a marked test no run-alone override selects is red" \
    "test .a_alone. is marked to run alone, and no run-alone override selects it" ncfg "$NC_DIR"
expect_red_because "nextest-config: a run-alone override no marker names is red" \
    "runs .fx::b. test .b_one. alone, and no .nextest-alone:. marker names it" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/a.rs" '1.5 s. ' ''
expect_red_because "nextest-config: a run-alone reason without its time is red" \
    "gives no time in seconds" ncfg "$NC_DIR"
nc_tree
printf '// nextest-alone: 2 s. above no test\nconst NOTHING: u8 = 0;\n' >>"$NC_DIR/crates/fx/tests/a.rs"
expect_red_because "nextest-config: a run-alone marker above no test, below the leading block, is red" \
    "a.rs:18: a .nextest-alone:. marker neither above a .#.test.. nor" ncfg "$NC_DIR"
nc_tree
printf 'mod inner {\n    // nextest-alone: 0.5 s. a test in an inline module\n    #[test]\n    fn deep() {}\n}\n' \
    >>"$NC_DIR/crates/fx/tests/b.rs"
nc_sub "$NC_CFG" $'\n'"$NC_GROUP" $'\n[[profile.default.overrides]]\nfilter = \'binary_id(fx::b) & test(=inner::deep)\'\nthreads-required = "num-test-threads"\npriority = 100\n\n'"$NC_GROUP"
expect green "nextest-config: a run-alone test in an inline module is named by its module path" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/boot.rs" $'#![allow(\n    clippy::disallowed_methods,\n' $'#![allow(\n    clippy::disallowed_methods,\n    clippy::disallowed_types,\n'
expect green "nextest-config: a leading marker below an inner attribute of four lines binds" ncfg "$NC_DIR"
# The long markers and their overrides: one test each, at the marker's
# seconds, 49 at most, on no test another override selects.
nc_long() { # <marker text> <priority>: the fixture's long test so marked, its override at <priority>
    nc_tree
    nc_sub "$NC_DIR/bins/fxd/tests/tiered.rs" '// nextest-long: 20 s' "// nextest-long: $1"
    nc_sub "$NC_CFG" 'priority = 20' "priority = $2"
}
nc_long '15 s' 15
expect green "nextest-config: a long marker of 15 s at priority 15 is green" ncfg "$NC_DIR"
nc_long '14 s' 14
expect_red_because "nextest-config: a long marker under 15 s is red" \
    "tiered.rs:10: a .nextest-long:. marker reads .<n> s., n an integer of 15 or more, not '14 s'" ncfg "$NC_DIR"
nc_long '60 s' 49
expect green "nextest-config: a long marker over 49 s at priority 49 is green" ncfg "$NC_DIR"
nc_long '60 s' 60
expect_red_because "nextest-config: a long override above 49 is red (canary)" \
    "override 4 starts .fxd::tiered. test .tiered_storm. at priority 60; its marker's 60 s asks 49" ncfg "$NC_DIR"
nc_long '20 s' 21
expect_red_because "nextest-config: a long override at a priority not its marker's is red (canary)" \
    "override 4 starts .fxd::tiered. test .tiered_storm. at priority 21; its marker's 20 s asks 20" ncfg "$NC_DIR"
for nc_text in '20' '20.5 s' '020 s' '20 s, a storm'; do
    nc_long "$nc_text" 20
    expect_red_because "nextest-config: a long marker reading '$nc_text' is red" \
        "tiered.rs:10: a .nextest-long:. marker reads .<n> s." ncfg "$NC_DIR"
done
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/tiered.rs" $'// nextest-long: 20 s\n' ''
expect_red_because "nextest-config: a long override no marker names is red (canary)" \
    "override 4 starts .fxd::tiered. test .tiered_storm. early, and no .nextest-long:. marker" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" $'\n'"$NC_LONG" ''
expect_red_because "nextest-config: a long marker no override selects is red (canary)" \
    "tiered.rs:10: .fxd::tiered. test .tiered_storm. is marked long, and no long override selects it" \
    ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" "'binary_id(fxd::tiered) & test(=tiered_storm)'" "'binary_id(fxd::tiered)'"
expect_red_because "nextest-config: a long override on a whole binary is red" \
    "override 4: a long override's filter is one test" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" "'binary_id(fxd::tiered) & test(=tiered_storm)'" "'package(fxd) & test(=tiered_storm)'"
expect_red_because "nextest-config: a long override on a package's test is red" \
    "override 4: a long override's filter is one test" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'priority = 20' $'priority = 20\nslow-timeout = "60s"'
expect_red_because "nextest-config: a long override with a third key is red" "override 4 holds keys" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'priority = 20' $'priority = 20\ntest-group = "serial"'
expect_red_because "nextest-config: a long override with a test group is a group's, red at its priority" \
    "override 4: a group's override sets priority 50, not 20" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" 'priority = 20' $'priority = 20\nthreads-required = "num-test-threads"'
expect_red_because "nextest-config: a long override with every slot is a run-alone one, red at its priority" \
    "override 4: a run-alone override sets priority 100, not 20" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/boot.rs" $'#[test]\nfn boots()' $'// nextest-long: 20 s\n#[test]\nfn boots()'
printf "\n[[profile.default.overrides]]\nfilter = 'binary_id(fxd::boot) & test(=boots)'\npriority = 20\n" >>"$NC_CFG"
expect_red_because "nextest-config: a long override on a test of a binary run alone is red" \
    "long override 5 selects .fxd::boot. test .boots., which override 1 also selects" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/a.rs" $'// shares.\n#[test]' $'// shares.\n// nextest-long: 20 s\n#[test]'
printf "\n[[profile.default.overrides]]\nfilter = 'binary_id(fx::a) & test(=a_alone)'\npriority = 20\n" >>"$NC_CFG"
expect_red_because "nextest-config: a long override on a run-alone test is red" \
    "long override 5 selects .fx::a. test .a_alone., which override 2 also selects" ncfg "$NC_DIR"
nc_tree
printf '\n%s' "$NC_LONG" >>"$NC_CFG"
expect_red_because "nextest-config: two long overrides on one test are red" \
    "long override 4 selects .fxd::tiered. test .tiered_storm., which override 5 also selects" ncfg "$NC_DIR"
# A test joins a set by its marker and its override, and either half alone is
# red (above): a binary may hold a run-alone test and a long one.
nc_tree
printf '\n// nextest-alone: 2 s. Reads a live rate.\n#[test]\nfn b_alone() {}\n\n// nextest-long: 30 s\n#[test]\nfn b_long() {}\n' \
    >>"$NC_DIR/crates/fx/tests/b.rs"
nc_sub "$NC_CFG" $'\n'"$NC_GROUP" $'\n[[profile.default.overrides]]\nfilter = \'binary_id(fx::b) & test(=b_alone)\'\nthreads-required = "num-test-threads"\npriority = 100\n\n'"$NC_GROUP"
printf "\n[[profile.default.overrides]]\nfilter = 'binary_id(fx::b) & test(=b_long)'\npriority = 30\n" >>"$NC_CFG"
expect green "nextest-config: a long test beside a run-alone test in one binary is green" ncfg "$NC_DIR"
nc_tree
printf '\n// nextest-long: 20 s\nconst NOT_A_TEST: u8 = 0;\n' >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: a long marker above no test is red" \
    "b.rs:8: a .nextest-long:. marker above no .#.test.. binds nothing" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/b.rs" 'mod support;' $'// nextest-long: 20 s\n\nmod support;'
expect_red_because "nextest-config: a long marker in a file's leading comments binds nothing" \
    "b.rs:1: a .nextest-long:. marker above no .#.test.. binds nothing" ncfg "$NC_DIR"
nc_tree
mkdir -p "$NC_DIR/crates/fx/tests/orphan"
printf '// nextest-long: 20 s\n#[test]\nfn lost() {}\n' >"$NC_DIR/crates/fx/tests/orphan/lost.rs"
expect_red_because "nextest-config: a long marker in a file no binary compiles is red" \
    "lost.rs:1: a .nextest-long:. marker in a file no test binary compiles" ncfg "$NC_DIR"
nc_tree # fx::b and fxd::tiered both compile crates/fx/tests/common/mod.rs
mkdir -p "$NC_DIR/crates/fx/tests/common"
printf '// nextest-long: 20 s\n#[test]\nfn storm() {}\n' >"$NC_DIR/crates/fx/tests/common/mod.rs"
printf '\nmod common;\n' >>"$NC_DIR/crates/fx/tests/b.rs"
printf '\n#[path = "../../../crates/fx/tests/common/mod.rs"]\nmod common;\n' >>"$NC_DIR/bins/fxd/tests/tiered.rs"
printf "\n[[profile.default.overrides]]\nfilter = 'binary_id(fx::b) & test(=common::storm)'\npriority = 20\n" >>"$NC_CFG"
expect_red_because "nextest-config: a long test in an included file binds each binary under its path" \
    "common/mod.rs:1: .fxd::tiered. test .common::storm. is marked long, and no long override" ncfg "$NC_DIR"
printf "\n[[profile.default.overrides]]\nfilter = 'binary_id(fxd::tiered) & test(=common::storm)'\npriority = 20\n" \
    >>"$NC_CFG"
expect green "nextest-config: its long overrides name it by its module path in each binary" ncfg "$NC_DIR"
# A library's statics reach every test binary that links it: its package's,
# and those of each workspace package that depends on it.
nc_lib_lock() { # fx's library holds a grouped lock; the group override names <filter>
    nc_tree # with no long test: a group here may take a whole package
    nc_sub "$NC_DIR/bins/fxd/tests/tiered.rs" $'// nextest-long: 20 s\n' ''
    nc_sub "$NC_CFG" $'\n'"$NC_LONG" ''
    printf '// nextest-group: serial\npub static SHARED: std::sync::Mutex<()> = std::sync::Mutex::new(());\n' \
        >"$NC_DIR/crates/fx/src/lib.rs"
    printf "\n[[profile.default.overrides]]\nfilter = '%s'\ntest-group = \"serial\"\npriority = 50\n" "$1" \
        >>"$NC_CFG"
}
nc_lib_lock 'binary_id(fx)'
expect_red_because "nextest-config: a library's lock grouped for the library's own binary only is red" \
    "lib.rs:2: test binary .fx::b. compiles this lock, and no override puts it in group .serial." \
    ncfg "$NC_DIR"
nc_lib_lock 'package(fx)'
expect green "nextest-config: a library's lock grouped for its whole package is green" ncfg "$NC_DIR"
printf '\n[dev-dependencies]\nfx = { path = "../../crates/fx" }\n' >>"$NC_DIR/bins/fxd/Cargo.toml"
expect_red_because "nextest-config: a library's lock reaches a package that depends on it" \
    "lib.rs:2: test binary .fxd::tiered. compiles this lock" ncfg "$NC_DIR"
printf "\n[[profile.default.overrides]]\nfilter = 'package(fxd)'\ntest-group = \"serial\"\npriority = 50\n" \
    >>"$NC_CFG"
expect green "nextest-config: grouped in the dependent package too, it is green" ncfg "$NC_DIR"
nc_lib_lock 'package(fx)'
mkdir -p "$NC_DIR/crates/mid/src"
printf '[package]\nname = "mid"\nversion = "0.0.0"\nedition = "2021"\n\n[lib]\ntest = false\n\n[dependencies]\nfx = { path = "../fx" }\n' \
    >"$NC_DIR/crates/mid/Cargo.toml"
: >"$NC_DIR/crates/mid/src/lib.rs"
printf '[workspace]\nmembers = ["crates/fx", "crates/mid", "bins/fxd"]\nresolver = "2"\n' >"$NC_DIR/Cargo.toml"
printf '\n[dev-dependencies]\nmid = { path = "../../crates/mid" }\n' >>"$NC_DIR/bins/fxd/Cargo.toml"
expect_red_because "nextest-config: a library's lock reaches through a normal dependency's dependents" \
    "lib.rs:2: test binary .fxd::tiered. compiles this lock" ncfg "$NC_DIR"
nc_tree
printf '/// Free space, which `statvfs` would read, is no oracle here.\npub fn held() -> u64 { 0 }\n' \
    >"$NC_DIR/crates/fx/src/lib.rs"
expect green "nextest-config: a library's comment may name a free-space call" ncfg "$NC_DIR"
printf 'pub fn free() -> &'"'"'static str { "statvfs" }\n' >>"$NC_DIR/crates/fx/src/lib.rs"
expect_red_because "nextest-config: a free-space read in a library integration tests link is red" \
    "lib.rs:3: reads .statvfs. in a library integration tests link" ncfg "$NC_DIR"
# Modules resolve as rustc resolves them: a `#[path]` file's own `mod` sits
# beside it, and a declaration that resolves to no file is a scope error.
nc_tree
mkdir -p "$NC_DIR/crates/fx/tests/inc"
printf '#[path = "inc/part.rs"]\nmod part;\n' >>"$NC_DIR/crates/fx/tests/b.rs"
printf 'pub mod nested;\n' >"$NC_DIR/crates/fx/tests/inc/part.rs"
printf 'pub fn free() -> &'"'"'static str { "statvfs" }\n' >"$NC_DIR/crates/fx/tests/inc/nested.rs"
expect_red_because "nextest-config: a #[path] file's own module is read where rustc reads it" \
    "inc/nested.rs:1: names .statvfs. in an integration-test target" ncfg "$NC_DIR"
nc_tree
mkdir -p "$NC_DIR/crates/fx/tests/inc"
printf '#[path = "inc/part.rs"]\nmod part;\n' >>"$NC_DIR/crates/fx/tests/a.rs"
printf 'pub mod nested;\n' >"$NC_DIR/crates/fx/tests/inc/part.rs"
printf '// nextest-group: serial\npub static DEEP: std::sync::Mutex<()> = std::sync::Mutex::new(());\n' \
    >"$NC_DIR/crates/fx/tests/inc/nested.rs"
expect green "nextest-config: a grouped lock in a #[path] file's own module binds its binary" ncfg "$NC_DIR"
nc_tree
printf 'mod missing;\n' >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: a module that resolves to no file is a scope error" \
    "SCOPE ERROR: a module this gate cannot find would go unread: crates/fx/tests/b.rs:7: .mod missing;." \
    ncfg "$NC_DIR"
# One marker spelling: a comment that starts like a marker in another is red on
# its line; a marker named in prose is no marker.
nc_spelled() { # <label> <comment line>: the line above a new test in b.rs is red
    nc_tree
    printf '\n%s\n#[test]\nfn b_marked() {}\n' "$2" >>"$NC_DIR/crates/fx/tests/b.rs"
    expect_red_because "nextest-config: a marker $1 is red" \
        "b.rs:8: a comment spelled like a marker" ncfg "$NC_DIR"
}
nc_spelled "without its colon" '// nextest-alone - 2 s. The verdict reads a live rate.'
nc_spelled "in capitals" '// Nextest-Alone: 2 s. The verdict reads a live rate.'
nc_spelled "with a space before its colon" '// nextest-alone : 2 s. The verdict reads a live rate.'
nc_spelled "in a block comment" '/* nextest-alone: 2 s. The verdict reads a live rate. */'
nc_spelled "in a doc comment" '/// nextest-alone: 2 s. The verdict reads a live rate.'
nc_spelled "with a word outside the five" '// nextest-solo: 2 s. The verdict reads a live rate.'
nc_spelled "with a longer word" '// nextest-run-alone: 2 s. The verdict reads a live rate.'
nc_spelled "with a dot" '// nextest.alone: 2 s. The verdict reads a live rate.'
nc_spelled "long without its colon" '// nextest-long 20 s'
nc_spelled "long with a space" '// nextest long: 20 s'
nc_spelled "long with a longer word" '// nextest-longest: 20 s'
nc_spelled "long with an underscore" '// nextest_long: 20 s'
nc_tree
printf '\n/// nextest runs each test in a process of its own.\n#[test]\nfn b_prose() {}\n' \
    >>"$NC_DIR/crates/fx/tests/b.rs"
expect green "nextest-config: prose that starts with the runner's name is no marker" ncfg "$NC_DIR"
nc_tree
printf '\n#[test] // nextest-alone: 2 s. The verdict reads a live rate.\nfn b_marked() {}\n' \
    >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: a marker beside code is red" \
    "b.rs:8: a comment spelled like a marker" ncfg "$NC_DIR"
nc_tree
printf '\n/*\n// nextest-alone: 2 s. The verdict reads a live rate.\n*/\n#[test]\nfn b_marked() {}\n' \
    >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: a marker's line inside a block comment is red" \
    "b.rs:9: a comment spelled like a marker" ncfg "$NC_DIR"
nc_tree
printf '\n// The whole target would need a `nextest-alone: <reason>` marker to run alone.\nfn helper() {}\n' \
    >>"$NC_DIR/crates/fx/tests/b.rs"
expect green "nextest-config: a marker named in prose is no marker" ncfg "$NC_DIR"
# D4: CI installs the floor, --locked.
nc_tree
nc_sub "$NC_DIR/.github/workflows/ci.yml" '--locked ' ''
expect_red_because "nextest-config: an install without --locked is red" \
    "a cargo-nextest install names .--locked." ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/.github/workflows/ci.yml" '--version 0.9.150' '--version 0.9.149'
expect_red_because "nextest-config: an install of another version is red" \
    "ci.yml:4: a cargo-nextest install names .--locked. and .--version 0.9.150." ncfg "$NC_DIR"
nc_tree
printf '  other:\n    steps:\n      - run: cargo nextest run -p fx\n' >>"$NC_DIR/.github/workflows/ci.yml"
expect_red_because "nextest-config: a job that runs nextest with no install is red" \
    "ci.yml:8: job .other. runs cargo nextest and installs no cargo-nextest" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/.github/workflows/ci.yml" '      - run: cargo install' '      # - run: cargo install'
printf '  other:\n    steps:\n      - run: cargo install --locked cargo-nextest --version 0.9.150\n' \
    >>"$NC_DIR/.github/workflows/ci.yml"
expect_red_because "nextest-config: a commented-out install installs nothing" \
    "ci.yml:5: job .test. runs cargo nextest and installs no cargo-nextest" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/.github/workflows/ci.yml" $'      - run: cargo install --locked cargo-nextest --version 0.9.150\n' ''
expect_red_because "nextest-config: no install in any workflow is a scope error" \
    "SCOPE ERROR: no cargo-nextest install in any workflow" ncfg "$NC_DIR"
# D7: no free-space read; each MEM-BUDGET file states its stall bound or why
# no write parks.
nc_tree
printf 'fn free() -> &'"'"'static str { "statvfs" }\n' >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: a free-space read in an integration test is red" \
    "b.rs:7: names .statvfs. in an integration-test target" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/boot.rs" $'// nextest-stall-default: no write reaches a window, so no write parks.\n' ''
expect_red_because "nextest-config: a MEM-BUDGET file with neither the key nor the marker is red" \
    "boot.rs: names MEM-BUDGET, and neither states TAIL-STALL-TIMEOUT" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/b.rs" 'mod support;' $'// nextest-stall-default: a file naming no budget\n\nmod support;'
expect_red_because "nextest-config: a stall marker in a file naming no budget is red" \
    "b.rs:1: a .nextest-stall-default:. marker binds nothing" ncfg "$NC_DIR"
# The stall rule reads code: a key in a comment, or a stall key named inside
# prose, neither makes a budget file nor states its bound.
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/boot.rs" $'// nextest-stall-default: no write reaches a window, so no write parks.\n' \
    $'// Runs at the product\'s TAIL-STALL-TIMEOUT default.\n'
expect_red_because "nextest-config: a stall key named in a comment states no bound" \
    "boot.rs: names MEM-BUDGET, and neither states TAIL-STALL-TIMEOUT" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/boot.rs" $'// nextest-stall-default: no write reaches a window, so no write parks.\n' \
    $'// Pass b"TAIL-STALL-TIMEOUT" to state the bound.\n'
expect_red_because "nextest-config: the stall key's literal inside a comment states no bound" \
    "boot.rs: names MEM-BUDGET, and neither states TAIL-STALL-TIMEOUT" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/tiered.rs" 'b"TAIL-STALL-TIMEOUT", b"60000"' 'b"8mb at the TAIL-STALL-TIMEOUT default", b""'
expect_red_because "nextest-config: a stall key inside prose states no bound" \
    "tiered.rs: names MEM-BUDGET, and neither states TAIL-STALL-TIMEOUT" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/tiered.rs" 'b"MEM-BUDGET", b"8mb", ' ''
expect green "nextest-config: a file naming MEM-BUDGET only in a comment is no budget file" ncfg "$NC_DIR"
nc_sub "$NC_DIR/bins/fxd/tests/tiered.rs" '//! Writes' $'// nextest-stall-default: no write parks.\n//! Writes'
expect_red_because "nextest-config: so a stall marker there binds nothing" \
    "tiered.rs:1: a .nextest-stall-default:. marker binds nothing" ncfg "$NC_DIR"
# Each remaining rule, planted once.
nc_tree
nc_sub "$NC_CFG" 'test-group = "serial"' 'test-group = "nogroup"'
expect_red_because "nextest-config: a group override naming no group is red" \
    "override 3 names test group .nogroup., which .test-groups. lacks" ncfg "$NC_DIR"
nc_tree
printf 'static TABLE: std::sync::RwLock<()> = std::sync::RwLock::new(());\n' >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: an unmarked static RwLock is red" "b.rs:7: a static lock with no" \
    ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_CFG" "filter = 'binary_id(fx::a)'" "filter = 'binary_id(fx::a) & test(=a_one)'"
expect_red_because "nextest-config: a one-test group override does not group a lock's binary" \
    "a.rs:7: test binary .fx::a. compiles this lock, and no override puts it in group" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/crates/fx/tests/a.rs" '// nextest-group: serial' '// nextest-group: the serial one'
expect_red_because "nextest-config: a group marker naming no group word is red" \
    "a.rs:5: .nextest-group:. names no group" ncfg "$NC_DIR"
nc_tree
printf '// nextest-in-process:\npub static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());\n' \
    >"$NC_DIR/crates/fx/tests/support/mod.rs"
expect_red_because "nextest-config: an in-process marker without its reason is red" \
    "mod.rs:1: .nextest-in-process:. gives no reason" ncfg "$NC_DIR"
nc_tree
mkdir -p "$NC_DIR/crates/fx/tests/orphan"
printf '// nextest-group: serial\npub static LOST: std::sync::Mutex<()> = std::sync::Mutex::new(());\n' \
    >"$NC_DIR/crates/fx/tests/orphan/lost.rs"
expect_red_because "nextest-config: a grouped lock in a file no binary compiles is red" \
    "lost.rs:2: a grouped lock in a file no test binary compiles" ncfg "$NC_DIR"
nc_tree
mkdir -p "$NC_DIR/crates/fx/tests/orphan"
printf '// nextest-alone: 2 s. A test no binary compiles.\n#[test]\nfn lost() {}\n' \
    >"$NC_DIR/crates/fx/tests/orphan/lost.rs"
expect_red_because "nextest-config: a run-alone marker in a file no binary compiles is red" \
    "lost.rs:1: a .nextest-alone:. marker in a file no test binary compiles" ncfg "$NC_DIR"
for nc_word in statfs f_bavail f_bfree; do
    nc_tree
    printf 'fn free() -> &'"'"'static str { "%s" }\n' "$nc_word" >>"$NC_DIR/crates/fx/tests/b.rs"
    expect_red_because "nextest-config: $nc_word in an integration test is red" \
        "b.rs:7: names .$nc_word. in an integration-test target" ncfg "$NC_DIR"
done
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/boot.rs" $'// nextest-stall-default: no write reaches a window, so no write parks.\n' ''
printf '\n// nextest-stall-default: no write reaches a window, so no write parks.\nfn helper() {}\n' \
    >>"$NC_DIR/bins/fxd/tests/boot.rs"
expect_red_because "nextest-config: a stall marker below the file's first item binds nothing" \
    "boot.rs:16: a .nextest-stall-default:. marker binds nothing" ncfg "$NC_DIR"
nc_tree
nc_sub "$NC_DIR/bins/fxd/tests/boot.rs" 'nextest-stall-default: no write reaches a window, so no write parks.' \
    'nextest-stall-default:'
expect_red_because "nextest-config: a stall marker without its reason binds nothing" \
    "boot.rs:8: a .nextest-stall-default:. marker binds nothing" ncfg "$NC_DIR"
nc_tree
printf '\n// nextest-alone: 2 s. A test in an included module.\n#[test]\nfn deep() {}\n' \
    >>"$NC_DIR/crates/fx/tests/support/mod.rs"
expect_red_because "nextest-config: a run-alone test in an included file binds each binary under its path" \
    "test .support::deep. is marked to run alone" ncfg "$NC_DIR"
nc_sub "$NC_CFG" $'\n'"$NC_GROUP" $'\n[[profile.default.overrides]]\nfilter = \'binary_id(fx::a) & test(=support::deep)\'\nthreads-required = "num-test-threads"\npriority = 100\n\n[[profile.default.overrides]]\nfilter = \'binary_id(fx::b) & test(=support::deep)\'\nthreads-required = "num-test-threads"\npriority = 100\n\n'"$NC_GROUP"
expect green "nextest-config: its overrides name it by its module path in each binary" ncfg "$NC_DIR"
nc_tree
printf 'const CREATE: [&[u8]; 2] = [b"MEM-BUDGET", b"8mb"];\n' >>"$NC_DIR/crates/fx/tests/b.rs"
expect_red_because "nextest-config: the stall rule reads crates/*/tests" \
    "b.rs: names MEM-BUDGET, and neither states TAIL-STALL-TIMEOUT" ncfg "$NC_DIR"
nc_tree
mkdir -p "$NC_DIR/tests"
printf 'const CREATE: [&[u8]; 2] = [b"MEM-BUDGET", b"8mb"];\n' >"$NC_DIR/tests/loose.rs"
expect_red_because "nextest-config: the stall rule reads tests/" \
    "tests/loose.rs: names MEM-BUDGET, and neither states TAIL-STALL-TIMEOUT" ncfg "$NC_DIR"
nc_tree
printf '\nconst DOC: &str = "\nstatic FAKE: std::sync::Mutex<()> = std::sync::Mutex::new(());\n";\n' \
    >>"$NC_DIR/crates/fx/tests/b.rs"
expect green "nextest-config: a string's line that reads as a lock is no lock" ncfg "$NC_DIR"
# Scope: what the gate reads must exist.
nc_tree
rm -f "$NC_CFG"
expect_red_because "nextest-config: no config is a scope error" \
    "SCOPE ERROR: .config/nextest.toml is missing" ncfg "$NC_DIR"
nc_tree
rm -f "$NC_DIR/.github/workflows/ci.yml"
expect_red_because "nextest-config: no workflow is a scope error" \
    "SCOPE ERROR: no workflow under .github/workflows" ncfg "$NC_DIR"
nc_tree
rm -f "$NC_DIR/Cargo.toml"
expect_red_because "nextest-config: no workspace is a scope error" \
    "SCOPE ERROR: cargo metadata exited" ncfg "$NC_DIR"
nc_tree
printf 'autotests = false\nautobins = false\n\n[lib]\ntest = false\n' >>"$NC_DIR/crates/fx/Cargo.toml"
printf '[workspace]\nmembers = ["crates/fx"]\nresolver = "2"\n' >"$NC_DIR/Cargo.toml"
expect_red_because "nextest-config: a workspace with no test binary is a scope error" \
    "SCOPE ERROR: cargo metadata lists no test binary" ncfg "$NC_DIR"
nc_tree
printf '*.rs\n' >"$NC_DIR/.gitignore"
expect_red_because "nextest-config: no .rs file git lists is a scope error" \
    "SCOPE ERROR: git lists no .rs file" ncfg "$NC_DIR"
nc_tree
printf '\n[[test]]\nname = "ghost"\npath = "tests/ghost.rs"\n' >>"$NC_DIR/crates/fx/Cargo.toml"
expect_red_because "nextest-config: a crate root that is no file is a scope error" \
    "SCOPE ERROR: a module this gate cannot find would go unread: crates/fx/tests/ghost.rs" ncfg "$NC_DIR"
}

selftest_main "$SCRIPT_DIR/check-scripts-selftest.sh" "$@"
