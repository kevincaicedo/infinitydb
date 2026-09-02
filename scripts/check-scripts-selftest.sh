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
# runner, with a stub simulator). Shell only; no cargo.
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

# expect <red|green> <label> <command…>: runs the command with stdout+stderr
# captured; a mismatch prints the captured output.
expect() {
    local want=$1 label=$2
    shift 2
    local log="$work/log" status=0
    "$@" >"$log" 2>&1 || status=$?
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
    local log="$work/log"
    "$@" >"$log" 2>&1 || true
    if grep -q -- "$pattern" "$log"; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "SELFTEST FAIL: output lacks '$pattern' — $label"
        sed 's/^/    | /' "$log"
    fi
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

# ---------------------------------------------------------------- deny-list
DENY=./scripts/check-cell-denylist.sh

root=$(fixture clean <<'EOF'
pub fn ok() -> u64 { 1 }
EOF
)
expect green "deny-list: clean crate" env INF_CHECK_ROOT="$root" $DENY
expect_output "deny-list: scope line discloses the scan" "1 crates, 1 files, 1 lines scanned" env INF_CHECK_ROOT="$root" $DENY

# The P1 shape: the configured set resolves to nothing.
mkdir -p "$work/empty/crates"
expect red "deny-list: no crates at all is a failure, not OK" env INF_CHECK_ROOT="$work/empty" $DENY

# A stale exclusion (a path that evaporated) is a failure.
root=$(fixture stale <<'EOF'
pub fn ok() {}
EOF
)
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
root=$(fixture testmod <<'EOF'
pub fn ok() {}

#[cfg(test)]
mod tests {
    fn scratch() -> u128 {
        std::time::SystemTime::now().elapsed().unwrap().as_nanos()
    }
}
EOF
)
expect green "deny-list: wall clock inside #[cfg(test)] mod tests is stripped" env INF_CHECK_ROOT="$root" $DENY

root=$(fixture loommod <<'EOF'
pub fn ok() {}

#[cfg(all(test, not(loom)))]
mod tests {
    fn t() { std::thread::spawn(|| {}); }
}
EOF
)
expect green "deny-list: #[cfg(all(test, …))] module is stripped" env INF_CHECK_ROOT="$root" $DENY

# `any(test, feature)` is NOT test-only: it compiles under the feature.
root=$(fixture anymod <<'EOF'
#[cfg(any(test, feature = "probe"))]
mod probe {
    pub fn t() { std::thread::spawn(|| {}); }
}
EOF
)
expect red "deny-list: #[cfg(any(test, feature))] module is scanned" env INF_CHECK_ROOT="$root" $DENY

# The P1c shape, applied here: an inline #[cfg(test)] item must not swallow
# the rest of the file.
root=$(fixture inline <<'EOF'
pub struct S;
impl S {
    #[cfg(test)]
    pub fn peek(&self) -> u8 { 0 }
}
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
)
expect red "deny-list: a violation after an inline #[cfg(test)] item is still seen" env INF_CHECK_ROOT="$root" $DENY

# Sanctioned sites: the marker with a reason, on the line or the one above.
root=$(fixture allowsame <<'EOF'
pub fn t() -> std::time::Instant { std::time::Instant::now() } // denylist-allow: fixture reason
EOF
)
expect green "deny-list: marker with a reason on the same line" env INF_CHECK_ROOT="$root" $DENY
expect_output "deny-list: allowed sites are listed" "allowed crates/fake/src/lib.rs:1: fixture reason" env INF_CHECK_ROOT="$root" $DENY

root=$(fixture allowabove <<'EOF'
// denylist-allow: fixture reason on the line above
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
)
expect green "deny-list: marker with a reason on the line above" env INF_CHECK_ROOT="$root" $DENY

root=$(fixture allowbare <<'EOF'
pub fn t() -> std::time::Instant { std::time::Instant::now() } // denylist-allow
EOF
)
expect red "deny-list: a bare marker without a reason fails" env INF_CHECK_ROOT="$root" $DENY

root=$(fixture allowfar <<'EOF'
// denylist-allow: two lines up does not count
//
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
)
expect red "deny-list: a marker two lines above does not apply" env INF_CHECK_ROOT="$root" $DENY

# A `mod name;` under #[cfg(test)] makes the named file test-only.
root=$(fixture modfile <<'EOF'
#[cfg(test)]
mod scratch;
pub fn ok() {}
EOF
)
echo 'pub fn t() -> std::time::Instant { std::time::Instant::now() }' >"$root/crates/fake/src/scratch.rs"
expect green "deny-list: a #[cfg(test)] mod file is test-only" env INF_CHECK_ROOT="$root" $DENY

# A test module whose closing brace never comes back to its indent would
# blank the rest of the file: that is a scope error, not a pass.
root=$(fixture unterminated <<'EOF'
#[cfg(test)]
mod tests {
    fn t() {}
  }
pub fn t() -> std::time::Instant { std::time::Instant::now() }
EOF
)
expect red "deny-list: an unterminated test module is a scope error" env INF_CHECK_ROOT="$root" $DENY

# --------------------------------------------------------------- panic policy
PANIC=./scripts/check-panic-policy.sh

root=$(fixture pclean <<'EOF'
pub fn ok(v: Option<u8>) -> u8 { v.unwrap_or(0) }
pub fn ok2(v: Option<u8>) -> u8 { v.unwrap_or_default() }
pub fn ok3(v: Option<u8>) -> u8 { v.expect("invariant: caller checked") }
/// Docs may say `.unwrap()` without being code.
pub fn ok4() {}
EOF
)
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
root=$(fixture p1c <<'EOF'
pub struct S { mode: u8 }
impl S {
    #[cfg(test)]
    pub fn mode(&self) -> u8 { self.mode }
}
pub fn t(v: Option<u8>) -> u8 { v.unwrap() }
EOF
)
expect red "panic-policy: the ckpt.rs shape (inline cfg(test) then unwrap) is caught" env INF_CHECK_ROOT="$root" $PANIC
expect_output "panic-policy: inline items are disclosed" "1 inline cfg(test) items scanned as production" env INF_CHECK_ROOT="$root" $PANIC

root=$(fixture ptest <<'EOF'
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
)
expect green "panic-policy: unwrap inside test-only modules is stripped" env INF_CHECK_ROOT="$root" $PANIC
expect_output "panic-policy: stripped lines are disclosed" "9 test-only lines stripped" env INF_CHECK_ROOT="$root" $PANIC

root=$(fixture pallow <<'EOF'
// panic-policy-allow: fixture reason
pub fn t(v: Option<u8>) -> u8 { v.unwrap() }
EOF
)
expect green "panic-policy: marker with a reason on the line above" env INF_CHECK_ROOT="$root" $PANIC

root=$(fixture pbare <<'EOF'
pub fn t(v: Option<u8>) -> u8 { v.unwrap() } // panic-policy-allow
EOF
)
expect red "panic-policy: a bare marker without a reason fails" env INF_CHECK_ROOT="$root" $PANIC

expect red "panic-policy: no crates at all is a failure, not OK" env INF_CHECK_ROOT="$work/empty" $PANIC

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

# ----------------------------------------------------------------- verdict
if [ "$fail" -ne 0 ]; then
    echo "check-scripts self-test FAILED: $fail of $((pass + fail)) cases"
    exit 1
fi
echo "check-scripts self-test OK ($pass cases: deny-list, panic-policy, run-sweep each red on a planted violation)"
