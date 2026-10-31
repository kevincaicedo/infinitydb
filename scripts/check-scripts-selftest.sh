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
# cases, which run `cargo clippy` on the pinned toolchain.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "$SCRIPT_DIR/.."

work=$(mktemp -d)
[ -n "$work" ] && [ -d "$work" ] || { echo "selftest: mktemp failed" >&2; exit 2; }
# Every rm below is guarded: a variable that came back empty would turn
# `rm -rf "$x/…"` into a delete at the filesystem root.
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT
pass=0
fail=0

# A gate that never ran gives no verdict (ADR-0106 D1): counted as red, a
# tool the runner lacked passed every planted red of the gate that needed it.
# Each expect* refuses two shapes, failing the case with the cause.
# not_runnable <label> <command>: a path that is no executable file, or a name
# the shell cannot find, is refused before it runs — bash 3.2 under `set -e`
# reports a failed exec of a path as exit 1, which reads as red.
not_runnable() {
    case $2 in
    */*) [ -f "$2" ] && [ -x "$2" ] && return 1 ;;
    *) command -v -- "$2" >/dev/null && return 1 ;;
    esac
    fail=$((fail + 1))
    echo "SELFTEST FAIL: '$2' is not a runnable command, so it gives no verdict — $1"
}
# no_verdict <status> <label> <log>: a command that ran and exited 126 or 127
# — env refusing a gate it cannot execute or find, or a gate that died on a
# tool the runner lacks (both shells keep those codes) — is not a verdict.
no_verdict() {
    case $1 in 126 | 127) ;; *) return 1 ;; esac
    fail=$((fail + 1))
    echo "SELFTEST FAIL: exit $1 means a command could not be executed or found, not a gate verdict — $2"
    sed 's/^/    | /' "$3"
}

# expect <red|green> <label> <command…>: runs the command with stdout+stderr
# captured; a mismatch prints the captured output.
expect() {
    local want=$1 label=$2
    shift 2
    local log="$work/log" status=0
    not_runnable "$label" "$1" && return 0
    "$@" >"$log" 2>&1 || status=$?
    no_verdict "$status" "$label" "$log" && return 0
    if { [ "$want" = red ] && [ "$status" -ne 0 ]; } || { [ "$want" = green ] && [ "$status" -eq 0 ]; }; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "SELFTEST FAIL: expected $want, got exit $status — $label"
        sed 's/^/    | /' "$log"
    fi
}

# expect_output <label> <pattern> <command…>: the command's output must
# contain the pattern (a scope disclosure, an allowed-site line).
expect_output() {
    local label=$1 pattern=$2
    shift 2
    local log="$work/log" status=0
    not_runnable "$label" "$1" && return 0
    "$@" >"$log" 2>&1 || status=$?
    no_verdict "$status" "$label" "$log" && return 0
    if grep -q -- "$pattern" "$log"; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "SELFTEST FAIL: output lacks '$pattern' — $label"
        sed 's/^/    | /' "$log"
    fi
}

# expect_red_because <label> <pattern> <command…>: red, and for its own
# reason — a plant that is red for another reason is not evidence.
expect_red_because() {
    local label=$1 pattern=$2 status=0
    shift 2
    local log="$work/log"
    not_runnable "$label" "$1" && return 0
    "$@" >"$log" 2>&1 || status=$?
    no_verdict "$status" "$label" "$log" && return 0
    if [ "$status" -ne 0 ] && grep -q -- "$pattern" "$log"; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "SELFTEST FAIL: expected red naming '$pattern', got exit $status — $label"
        sed 's/^/    | /' "$log"
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
# `init`; the canary is this file with a raw reset appended.
fx_only() {
    ! grep -n 'git -C "\$' "$1" | grep -v -e ' init -q$' -e '^[0-9]*:    git -C "\$root" "\$@"$'
}
expect green "self-test: every git command on a fixture root goes through fx_git" \
    fx_only "$SCRIPT_DIR/check-scripts-selftest.sh"
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
    work="$work/nv" && mkdir -p "$work" && : >"$work/noexec" && pass=0 && fail=0
    printf '#!/usr/bin/env bash\nset -e\ninf-selftest-no-such-tool\n' >"$work/tool-missing"
    chmod +x "$work/tool-missing"
    expect red "a gate name the shell cannot find" inf-selftest-no-such-gate >"$work/o1"
    expect_red_because "a gate path that does not exist" "" "$work/no-such-gate" >"$work/o2"
    expect_output "a gate path that is not executable" "" "$work/noexec" >"$work/o3"
    expect red "a gate that dies on a missing tool" "$work/tool-missing" >"$work/o4"
    expect red "env refusing a gate it cannot execute" env "$work/noexec" >"$work/o5"
    cat "$work"/o[1-5]
    grep -qF "'inf-selftest-no-such-gate' is not a runnable command" "$work/o1" &&
        grep -qF "'$work/no-such-gate' is not a runnable command" "$work/o2" &&
        grep -qF "'$work/noexec' is not a runnable command" "$work/o3" &&
        grep -qF "exit 127 means a command could not be executed or found" "$work/o4" &&
        grep -qF "exit 126 means a command could not be executed or found" "$work/o5" &&
        [ "$pass" -eq 0 ] && [ "$fail" -eq 5 ]
)
expect green "self-test: a gate that never ran is never counted as a verdict (canary)" nv_canary

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

# ---------------------------------------------------------------- deny-list
DENY=./scripts/check-cell-denylist.sh

IFS= read -r -d '' body <<'EOF' || true
pub fn ok() -> u64 { 1 }
EOF
root=$(printf '%s' "$body" | fixture clean)
expect green "deny-list: clean crate" env INF_CHECK_ROOT="$root" $DENY
expect_output "deny-list: scope line discloses the scan" "1 crates, 1 files, 1 lines scanned" env INF_CHECK_ROOT="$root" $DENY

# The P1 shape: the configured set resolves to nothing.
mkdir -p "$work/empty/crates"
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

# --------------------------------------------------------------- panic policy
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

expect red "panic-policy: no crates at all is a failure, not OK" env INF_CHECK_ROOT="$work/empty" $PANIC

# ------------------------------------------------------------ reply outcome
# ADR-0190 D4: no production site of crates/inf-server/src reads a reply's
# outcome from its bytes. Each fixture is one inf-server lib.rs (and a
# test-only modfile where the case needs one).
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
expect red "reply-outcome: a missing directory is a scope error" env INF_CHECK_ROOT="$work/empty" $REPLY

# ---------------------------------------------------------------- run-sweep
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

# ------------------------------------------------------ shipping features
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

# ------------------------------------------------ planted-bug canary driver
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
echo "inf_canary_fixture loom fake the_row" >"$work/canary-short-loom-row"
expect red "canaries: a loom row without its witness is a scope failure" env INF_CANARY_ROWS_FILE="$work/canary-short-loom-row" INF_CANARY_CARGO="$dir/cargo" $CANARY

# --------------------------------------------------- release-assert inventory
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

# ---------------------------------------------------------------- clock ban
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

# ------------------------------------------------------- waker atomics (D8)
# ADR-0106 second amendment (review 2026-08-30 F-L20-03). The gate's three
# defects were all in the scan, so the fixtures ARE synthetic asm: a
# `.Lfunc_beginN:` on the body's first line (the old awk reset there and
# scanned two lines / zero instructions per waker), an x86 `lock` prefix in
# its own tab-separated field (the old mnemonic set anchored at the line
# start and could not match it), and the aarch64 pair. The cargo-driven
# probe is fixture-skipped and disclosed; it always runs on the real tree,
# and its verdict logic is pinned below on fixtures of its asm.
WAKER=./scripts/check-waker-atomics.sh
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
expect_output "waker: the scope line discloses instructions actually scanned" "4 wakers + 0 called bodies, 8 instruction lines scanned" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER

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
expect_output "waker: Mach-O bodies are scanned, not swallowed by Lfunc_begin" "4 wakers + 0 called bodies, 8 instruction lines scanned, 0 unresolved" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
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
    asm=$( { waker_body waker_wake 'movq\t%rdi, %rax' "$insn" 'retq'
             for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
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
asm=$( { waker_body waker_wake 'jmpq\t*%rax' 'callq\t*ext_fn@GOTPCREL(%rip)' 'retq'
         for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
         waker_vtable; } | waker_asm waker-ijump )
expect_output "waker: an indirect jump is counted on the scope line" \
    "1 unresolved edges, 1 indirect jumps" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
expect_output "waker: a call through the GOT is a direct, disclosed edge" \
    "unfollowed callees (no body in this asm): ext_fn" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
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
      printf '\t.size\t%s, 32\n' "$(psym WAKER_VTABLE)"; } | waker_asm "$name"
}
tree=$( { for f in waker_clone waker_wake waker_wake_by_ref waker_drop; do waker_body "$f" 'ret'; done
          waker_vtable; } | waker_asm waker-tree )
pasm=$(probe_fixture waker-probe-arm64 'bl\t__aarch64_cas8_acq_rel' 'bl\t__aarch64_ldadd8_acq_rel' \
    'stlr\tx9, [x8]' 'bl\t__aarch64_ldadd8_acq_rel')
expect_output "waker probe: the arm64 Linux shape is 3 plants red, 2 controls green" \
    "probe: fixture asm, 3 of 3 planted wakers red, 2 of 2 controls green" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
expect green "waker probe: the arm64 Linux shape passes" env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" $WAKER
# The controls are exact: a probe copy with both control lines deleted is red.
mkdir -p "$work/wk/scripts"
cp scripts/check-waker-atomics.sh scripts/asm-waker-scan.awk scripts/probe-target.sh "$work/wk/scripts/"
cp -R scripts/waker-atomics-probe "$work/wk/scripts/"
sed -i.bak '/WAKER-PROBE: expect-clean/d; /WAKER-PROBE: expect-unscanned/d' \
    "$work/wk/scripts/waker-atomics-probe/src/lib.rs"
expect_red_because "waker probe: a probe whose controls were deleted is red" \
    "probe declares 0 clean and 0 off-path controls (expected exactly 1 and 1)" \
    env INF_WAKER_ASM="$tree" INF_WAKER_PROBE_ASM="$pasm" "$work/wk/scripts/check-waker-atomics.sh"
pasm=$(PSYM=v0 probe_fixture waker-probe-v0 'bl\t__aarch64_cas8_acq_rel' 'bl\t__aarch64_ldadd8_acq_rel' \
    'stlr\tx9, [x8]' 'bl\t__aarch64_ldadd8_acq_rel')
expect_output "waker probe: v0-mangled names match their own symbols" \
    "probe: fixture asm, 3 of 3 planted wakers red, 2 of 2 controls green" \
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

# ------------------------------------------------------- fault points (D9)
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

# ---------------------------------------------------- fsync fail-stop (D10)
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
    pass=$((pass + $(sed -n 's/^Ran \([0-9]*\) tests.*/\1/p' "$inventory_dag_log")))
else
    fail=$((fail + 1))
    echo "SELFTEST FAIL: inventory / dep-DAG fixtures"
    sed 's/^/    | /' "$inventory_dag_log"
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

# ------------------------------------------------- unsafe roots (batch 44)
# ADR-0121 (review 2026-08-30 F-L17-09): `inf-alloc` and `inf-runtime` — the
# two named audited leaves — carried no root attribute at all, so a new
# unsafe block anywhere in them produced no lint and no diff signal.
ROOTS=./scripts/check-unsafe-roots.sh
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

# ---------------------------------------------- safety inventory (batch 44)
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

# --------------------------------------------- style limits (ADR-0125, batch 64)
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
# ------------------------------------------------------------ lint-ratchet
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

# ---------------------------------------------------------- doc artifacts
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
LS_GATE="$work/gate"
mkdir -p "$LS_GATE/crates"
cp -R "$SCRIPT_DIR" "$LS_GATE/scripts"
cp rust-toolchain.toml clippy.toml "$LS_GATE/"
# The probe's slot plants depend on the engine's inf-foundation (ADR-0144
# A2): the gate copy resolves it under its own root, with a workspace whose
# members are that crate alone (its manifest inherits from the workspace).
cp -R crates/inf-foundation "$LS_GATE/crates/inf-foundation"
rm -rf "$LS_GATE/crates/inf-foundation/fuzz"
sed 's|^members = .*|members = ["crates/*"]|' Cargo.toml >"$LS_GATE/Cargo.toml"
grep -q '^members = \["crates/\*"\]$' "$LS_GATE/Cargo.toml" ||
    { echo "selftest: the gate copy's workspace members were not rewritten" >&2; exit 2; }
LINTSCOPES="$LS_GATE/scripts/check-lint-scopes.sh"
ls_gate() { # <maximum> <backing sites>
    sed -i.bak -e "s/^CONTAINER_EXEMPTIONS_MAX=[0-9][0-9]*\$/CONTAINER_EXEMPTIONS_MAX=$1/" \
        -e "s/^CAPPED_BACKING_SITES=[0-9][0-9]*\$/CAPPED_BACKING_SITES=$2/" "$LINTSCOPES"
    rm -f "$LINTSCOPES.bak"
    grep -q "^CONTAINER_EXEMPTIONS_MAX=$1\$" "$LINTSCOPES" && grep -q "^CAPPED_BACKING_SITES=$2\$" "$LINTSCOPES" ||
        { echo "ls_gate: the gate's constants are not where the copy sets them" >&2; exit 2; }
}
ls_gate 0 0
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
root=$(ls_root ls-containers)
printf '%s\npub mod a;\n' "$LS_ATTR" >"$root/crates/fake/src/lib.rs"
printf '# gate\nCONTAINER_EXEMPTIONS_MAX=%s\n' "$LC_MAX" >"$root/scripts/check-lint-scopes.sh"
lc_source
lc_table
ls_commit "$root" containers
fx_git "$root" branch -q -f base-tip HEAD
ls_gate "$LC_MAX" 0
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
# ADR-0163 A1: a row moves later only up to its record's date on the gate's
# CONTAINER_EXPIRY_MOVES line (a ceiling); the line's own dates only move
# earlier, and a record joins it only by the ADR that changes the rule,
# against HEAD, the base tip and the commit that introduced the line.
# lc_moves edits the working gate's line, as ls_gate edits its maximum; the
# fixture's rows sit at 2099-12-31 (struct Exempt is T, fn Owner::build E).
lc_moves() { # <entries>
    sed -i.bak "s/^CONTAINER_EXPIRY_MOVES=\".*\"\$/CONTAINER_EXPIRY_MOVES=\"$1\"/" "$LINTSCOPES"
    rm -f "$LINTSCOPES.bak"
    grep -q "^CONTAINER_EXPIRY_MOVES=\"$1\"\$" "$LINTSCOPES" ||
        { echo "lc_moves: the gate's move line is not where the copy sets it" >&2; exit 2; }
}
LC_MOVES=$(sed -n 's/^CONTAINER_EXPIRY_MOVES="\(.*\)"$/\1/p' "$LINTSCOPES")
[ -n "$LC_MOVES" ] || { echo "the gate spells no CONTAINER_EXPIRY_MOVES line" >&2; exit 2; }
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
lc_moves "$LC_MOVES"
lc_reset HEAD~1
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

# the probe's judge: a plant that compiles clean, and a plant that fails
# for an unrelated reason, are both red (ADR-0144 D5's probe rule).
ls_gate 0 0
root=$(ls_root ls-probe)
ls_probe() { env INF_CHECK_ROOT="$root" INF_LINT_BASE_REF=base-tip INF_LINT_PROBE=on INF_LINT_PROBE_SRC="$1" "$LINTSCOPES"; }
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
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-config"
sed 's|std::fs::set_times|std::fs::File::set_times|' clippy.toml >"$work/probe-unstable-config/clippy.toml"
expect red "lint-scopes: changing the configured unstable path is red" ls_probe "$work/probe-unstable-config"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-feature"
sed -i.bak 's|E0658 fs_set_times |E0658 wrong_feature |' "$work/probe-unstable-feature/unstable/set_times.rs"
expect red "lint-scopes: the unstable feature must match its census" ls_probe "$work/probe-unstable-feature"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-unstable-extra"
cp "$work/probe-unstable-extra/unstable/set_times.rs" "$work/probe-unstable-extra/unstable/extra.rs"
expect red "lint-scopes: the unstable census cannot grow" ls_probe "$work/probe-unstable-extra"

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

# ADR-0144 A2: the slot plants draw `await_holding_invalid_type` naming their
# configured path under the production config (the green case above): the
# slot, and the answer that carries it, since the lint matches the outermost
# type of a live local. A config without either slot type, a plant whose slot
# or answer is consumed before the await, and a discarded publish that is not
# discarded are each red for their own reason.
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-await-unconfigured"
grep -v 'path = "inf_foundation::bounded::DequeSlot"' clippy.toml >"$work/probe-await-unconfigured/clippy.toml"
expect_red_because "lint-scopes: a config without the slot type" \
    "config needs 6 distinct slot type await-holding-invalid-types, found 5" ls_probe "$work/probe-await-unconfigured"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-await-unconfigured-answer"
grep -v 'path = "inf_foundation::bounded::Reserved"' clippy.toml >"$work/probe-await-unconfigured-answer/clippy.toml"
expect_red_because "lint-scopes: a config without the answer that carries the slot" \
    "plant path inf_foundation::bounded::Reserved has no config entry" ls_probe "$work/probe-await-unconfigured-answer"
cp -R "$SCRIPT_DIR/lint-scope-probe" "$work/probe-await-consumed"
sed -i.bak 's|            yield_once().await;|            slot.publish(0);|; s|            slot.publish(1);|            yield_once().await;|' "$work/probe-await-consumed/src/awaits.rs"
expect_red_because "lint-scopes: a slot consumed before the await draws nothing on the plant" \
    "did NOT draw clippy::await_holding_invalid_type inf_foundation::bounded::DequeSlot" ls_probe "$work/probe-await-consumed"
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

# ------------------------------------------------- fuzz targets (ADR-0198)
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
# `#[path]` —, the five probe manifests, a `check` recipe running their gates,
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
    printf 'check:\n    ./scripts/check-clock-ban.sh\n    ./scripts/check-lint-scopes.sh\n' >"$root/justfile"
    printf '    ./scripts/check-fuzz-targets.sh\n    ./scripts/check-waker-atomics.sh\n' >>"$root/justfile"
    git -C "$root" init -q
    fz_answer "$root" crates/a/fuzz/Cargo.toml t_one
    fz_answer "$root" bins/b/fuzz/Cargo.toml b_parse
    fz_depinfo "$root" b_parse fuzz_targets/b_parse.rs fuzz_targets/../../src/parse.rs
    echo "$root"
}
fz_run() { # <root> [args…]: the gate on a fixture root, its probe off
    env INF_CHECK_ROOT="$1" INF_FUZZ_CHECK_PROBE=off INF_FUZZ_CARGO="$FZ_CARGO" FZ_STUB="$1.stub" \
        "$FUZZ" "${@:2}"
}
fz_probe_run() { # <root>: the gate with its probe on
    env INF_CHECK_ROOT="$1" INF_FUZZ_CARGO="$FZ_CARGO" FZ_STUB="$1.stub" "$FUZZ"
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

root=$(fz_root clean)
fz_once "$root.log" fz_run "$root"
expect green "fuzz-targets: a clean two-target root" fz_replay "$root.log"
expect_output "fuzz-targets: the OK line counts what it compiled and read" \
    "fuzz-targets OK: 2 fuzz workspaces, 2 of 2 targets compiled (2 .*bin.* rows, 2 files)" \
    fz_replay "$root.log"
expect_output "fuzz-targets: the OK line discloses the probe off in fixture mode" \
    "probe: skipped (fixture mode, INF_FUZZ_CHECK_PROBE=off); 5 probe manifests, each run by its gate" \
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
# caller set.
fz_calls_ok() { # <stub dir>
    python3 - "$1/calls" <<'PY'
import sys
lines = open(sys.argv[1], encoding="utf-8").read().split("\n")
calls = [lines[i:i + 3] for i in range(0, len(lines) - 1, 3)]
want = {"bins/b/fuzz/Cargo.toml", "crates/a/fuzz/Cargo.toml"}
rest = ["--bins", "--locked", "--keep-going", "--message-format", "json", "--color", "never",
        "--target-dir", "target/fuzz-check"]
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
fz_argv_run() { # <gate> <root>: the gate under a caller's own RUSTFLAGS
    env RUSTFLAGS=-Awarnings CARGO_ENCODED_RUSTFLAGS=-Awarnings INF_CHECK_ROOT="$2" \
        INF_FUZZ_CHECK_PROBE=off INF_FUZZ_CARGO="$FZ_CARGO" FZ_STUB="$2.stub" "$1"
}
root=$(fz_root argv)
expect green "fuzz-targets: green under a caller's RUSTFLAGS and CARGO_ENCODED_RUSTFLAGS" \
    fz_argv_run "$FUZZ" "$root"
expect green "fuzz-targets: each workspace's call carries the gate's flags and environment" \
    fz_calls_ok "$root.stub"
# The call's canaries: a copy of the gate with one part of its call dropped
# turns the case red. Each copy runs on the argv root, its calls cleared.
FZ_MROOT=$root
fz_mutant() { # <name> <sed expression> <what the copy drops>
    local dir
    dir="$work/fz-gate-$1"
    rm -rf "${dir:?}"
    mkdir -p "$dir"
    cp "$SCRIPT_DIR/check-fuzz-targets.sh" "$SCRIPT_DIR/fuzz_targets.py" "$SCRIPT_DIR/lint_scope_table.py" \
        "$SCRIPT_DIR/strip-test-modules.awk" "$dir/"
    fz_sed "$2" "$dir/check-fuzz-targets.sh"
    chmod +x "$dir/check-fuzz-targets.sh"
    if cmp -s "$SCRIPT_DIR/check-fuzz-targets.sh" "$dir/check-fuzz-targets.sh"; then
        echo "selftest: the fuzz-targets mutant '$1' changed nothing" >&2
        exit 2
    fi
    rm -f "${FZ_MROOT:?}.stub/calls"
    fz_argv_run "$dir/check-fuzz-targets.sh" "$FZ_MROOT" >/dev/null 2>&1 || true
    expect red "fuzz-targets: the call's check is red on a gate that $3" \
        fz_calls_ok "$FZ_MROOT.stub"
}
fz_mutant bins 's/"check", "--bins", /"check", /' "drops --bins"
fz_mutant locked 's/"--locked", //' "drops --locked"
fz_mutant keep-going 's/"--keep-going", //' "drops --keep-going"
fz_mutant format 's/"--message-format", "json",/"--message-format", "short",/' "reads cargo's short format"
fz_mutant color 's/"--color", "never", //' "drops --color never"
fz_mutant target-dir 's|"--target-dir", "target/fuzz-check"|"--target-dir", "target"|' \
    "shares the root's target directory"
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
    "$work/fz-k1-block.log"
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
# directories; a red census prints nothing on stdout and exits 1.
fz_matrix_is() { # <root> <mode> <expected stdout>
    local got
    got=$(env INF_CHECK_ROOT="$1" "$FUZZ" --matrix "$2") || return 1
    [ "$got" = "$3" ] || { echo "matrix: $got"; return 1; }
}
fz_matrix_red() { # <root>
    local got status=0
    got=$(env INF_CHECK_ROOT="$1" "$FUZZ" --matrix nightly 2>/dev/null) || status=$?
    [ "$status" -eq 1 ] && [ -z "$got" ]
}
root=$(fz_root matrix)
for kind in corpora seeds; do
    mkdir -p "$root/crates/a/fuzz/$kind/t_one"
    printf 'x' >"$root/crates/a/fuzz/$kind/t_one/seed"
done
FZ_ROWS='{"include":[{"dir":"bins/b","target":"b_parse","seeds":""},{"dir":"crates/a","target":"t_one","seeds":"fuzz/corpora/t_one fuzz/seeds/t_one"}]}'
expect green "fuzz-targets: --matrix nightly prints every target with its seeds" \
    fz_matrix_is "$root" nightly "$FZ_ROWS"
expect green "fuzz-targets: --matrix smoke prints the same rows" fz_matrix_is "$root" smoke "$FZ_ROWS"
fz_matrix_knob() { # <root> <rows>: the matrix mode compiles nothing, so it reads no probe knob
    local got
    got=$(env INF_CHECK_ROOT="$1" INF_FUZZ_CHECK_PROBE=no "$FUZZ" --matrix nightly) || return 1
    [ "$got" = "$2" ]
}
expect green "fuzz-targets: the matrix mode reads no probe knob" fz_matrix_knob "$root" "$FZ_ROWS"
mkdir -p "$root/crates/a/fuzz/corpora/gone"
printf 'x' >"$root/crates/a/fuzz/corpora/gone/seed"
expect green "fuzz-targets: a red census prints no matrix and exits 1" fz_matrix_red "$root"
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
expect_red_because "fuzz-targets: an unknown matrix" "usage: check-fuzz-targets.sh" \
    env INF_CHECK_ROOT="$root" "$FUZZ" --matrix weekly

# ----------------------------------------------------------------- verdict
if [ "$fail" -ne 0 ]; then
    echo "check-scripts self-test FAILED: $fail of $((pass + fail)) cases"
    exit 1
fi
echo "check-scripts self-test OK ($pass cases: deny-list, panic-policy, reply-outcome, run-sweep, shipping-features, sim-canaries, release-asserts, clock-ban, waker-atomics, fault-points, fsync-fail-stop, unsafe-roots, safety-inventory, file-length, line-width, lint-ratchet, doc-artifacts, parent-doc-gates, arith-spellings, lint-scopes, fuzz-targets each exiting non-zero on its planted case)"
