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
# probe is fixture-skipped and disclosed; it always runs on the real tree.
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
for insn in 'lock\t\txaddq\t%rax, (%rcx)' 'xchgq\t%rax, (%rcx)' 'mfence' 'ldaxr\tw8, [x0]' 'stlxr\tw9, w8, [x0]' 'casal\tw8, w9, [x0]' 'ldaddal\tw8, w9, [x0]' 'swpal\tw8, w9, [x0]' 'dmb\tish'
do
    asm=$( { waker_body waker_wake 'movq\t%rdi, %rax' "$insn" 'retq'
             for f in waker_clone waker_wake_by_ref waker_drop; do waker_body "$f" 'retq'; done
             waker_vtable; } | waker_asm waker-insn )
    expect red "waker: planted '$insn'" env INF_WAKER_ASM="$asm" INF_WAKER_PROBE=off $WAKER
done
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
rm -rf "$root/bins"
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
rm -rf "$root/tests"
expect red "line-width: a missing tests/ is a scope error" env INF_CHECK_ROOT="$root" $LINEW
# ------------------------------------------------------------ lint-ratchet
# ADR-0144 D3 (absorbs ADR-0125's fn-length ratchet). Fixtures are captured
# clippy JSON; the approved copies are real commits in a fixture repository.
RATCHET="$SCRIPT_DIR/check-lint-ratchet.sh"
ls_commit() { git -C "$1" add -A && git -C "$1" -c user.name=t -c user.email=t@t commit -q -m "$2"; }
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
    git -C "$root" branch -q base-tip
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
git -C "$root" reset -q --hard HEAD~1
# a fix does not buy a violation: -1 in dec.rs, +1 in an existing file
printf 't/x\t%s\tcast,arith\tratchet\nt/x\t%s\tcast,arith\tratchet\n' $D $F >"$root/docs/lint-scopes.tsv"
{ rt_msg arithmetic_side_effects $F 2 1; rt_msg cast_possible_truncation $D 5 9; rt_done; } >"$root/clippy.json"
printf 'arith\t1\t%s\ncast\t1\t%s\n' $F $D >"$root/docs/lint-baseline.tsv"
expect red "lint-ratchet: sites fixed in one file do not pay for new ones in an existing file" rt_run "$root"
# a split carries its counts to an added file
git -C "$root" reset -q --hard HEAD
printf 'pub fn h() {}\n' >"$root/crates/fake/src/split.rs"
printf 't/x\t%s\tcast,arith\tratchet\nt/x\tcrates/fake/src/split.rs\tcast,arith\tratchet\n' $D >"$root/docs/lint-scopes.tsv"
{ rt_msg arithmetic_side_effects crates/fake/src/split.rs 1 1; rt_msg cast_possible_truncation $D 5 9; rt_done; } >"$root/clippy.json"
printf 'arith\t1\tcrates/fake/src/split.rs\ncast\t1\t%s\n' $D >"$root/docs/lint-baseline.tsv"
git -C "$root" add -A
expect green "lint-ratchet: a split carries its count to an added file" rt_run "$root"
expect_output "lint-ratchet: the moved row is printed" "moved: arith crates/fake/src/split.rs" rt_run "$root"
git -C "$root" reset -q --hard HEAD
expect red "lint-ratchet: an unresolvable base ref is a scope error" env INF_CHECK_ROOT="$root" INF_LINT_BASE_REF=no-such-ref INF_LINT_RATCHET_INPUT="$root/clippy.json" "$RATCHET"
# two changes that each pass: the base tip moved down, this branch did not
git -C "$root" checkout -q -b feature
git -C "$root" checkout -q -B base-tip
printf 'cast\t1\t%s\n' $D >"$root/docs/lint-baseline.tsv"
ls_commit "$root" "PR1: arith fixed on the base"
git -C "$root" checkout -q feature
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
git -C "$doc_root" add tests/fixtures/artifacts.txt bins/inf-sim/seeds/regression.txt
expect green "docs: regression inputs and seeds belong in source" env INF_CHECK_ROOT="$doc_root" $DOCS
for output in .artifacts/gate.log artifacts/claim.json tests/fuzz/artifacts/crash; do
    mkdir -p "$doc_root/$(dirname "$output")"
    printf 'generated output\n' >"$doc_root/$output"
    expect green "docs: local output may exist ($output)" env INF_CHECK_ROOT="$doc_root" $DOCS
    git -C "$doc_root" add -f "$output"
    expect red "docs: tracked output is forbidden ($output)" env INF_CHECK_ROOT="$doc_root" $DOCS
    git -C "$doc_root" rm -q --cached -f "$output"
done

# ------------------------------------------------------------- lint-scopes
# ADR-0144 D1/D2: the crate-root wildcard deny, the structural suppression
# audit (one planted case per spelling that compiles clean under the deny)
# and the frozen ADR-0143 exemption table against its approved copies —
# fixture repositories with real commits, because the approved copies are
# history (HEAD, the base tip, the table's introducing commit).
LINTSCOPES="$SCRIPT_DIR/check-lint-scopes.sh"
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
ls_scopes() {
    mkdir -p "$1/crates/fake/fuzz/fuzz_targets" "$1/docs"
    printf '// fuzz\n' >"$1/crates/fake/fuzz/fuzz_targets/t.rs"
    printf 'fake/t\tnone: the fixture target enters no decoder\t-\t-\n' >"$1/docs/lint-scopes.tsv"
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
    ls_scopes "$root"
    git -C "$root" init -q
    git -C "$root" add -A
    git -C "$root" -c user.name=t -c user.email=t@t commit -q -m base
    git -C "$root" branch -q base-tip
    echo "$root"
}
ls_run() { env INF_CHECK_ROOT="$1" INF_LINT_BASE_REF=base-tip "$LINTSCOPES"; }
ls_commit() { git -C "$1" add -A && git -C "$1" -c user.name=t -c user.email=t@t commit -q -m "$2"; }
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
while IFS=$'\t' read -r lint scope cls; do
    cls=${cls%:}
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
    ls_case green "$lint — a reasoned function-level allow (multi-line)" "#[allow(
    clippy::$lint,
    reason = \"$cls: stated over two lines, through another attribute\"
)]
pub fn f() {}"
done < <(env INF_LINT_RULES=1 "$LINTSCOPES")
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
expect red "lint-scopes: a fuzz target no row names" ls_run "$root"
rm "$root/crates/fake/fuzz/fuzz_targets/new_decoder.rs"
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
rm -rf "$root/.git" "$root/scripts" "$root/docs"
git -C "$root" init -q
ls_commit "$root" "before the gate"
git -C "$root" branch -q base-tip
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
git -C "$root" reset -q --hard HEAD~1
git -C "$root" mv crates/fake/src/a.rs crates/fake/src/moved.rs
printf 'crates/fake/src/moved.rs\tf\tk\n' >"$root/docs/lint-exemptions.tsv"
git -C "$root" add -A
expect green "lint-scopes: a renamed file keeps its row" ls_run "$root"
git -C "$root" reset -q --hard HEAD
printf '%s\n' '#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column k")]' 'pub fn f2() {}' >"$root/crates/fake/src/extra.rs"
sed -i.bak 's/pub fn f2/pub fn f/' "$root/crates/fake/src/extra.rs" && rm "$root/crates/fake/src/extra.rs.bak"
expect red "lint-scopes: a second ADR-0143 allow with no row of its own" ls_run "$root"
git -C "$root" rm -q -f docs/lint-exemptions.tsv
expect red "lint-scopes: a missing table is a scope error" ls_run "$root"
git -C "$root" reset -q --hard HEAD
rm -f "$root/crates/fake/src/extra.rs"
expect red "lint-scopes: an unresolvable base ref is a scope error" env INF_CHECK_ROOT="$root" INF_LINT_BASE_REF=no-such-ref "$LINTSCOPES"
git -C "$root" checkout -q -b feature
git -C "$root" branch -q -f base-tip HEAD
git -C "$root" rm -q -f docs/lint-exemptions.tsv
expect red "lint-scopes: a table deleted from under the gate is not a bootstrap" ls_run "$root"

# the probe's judge: a plant that compiles clean, and a plant that fails
# for an unrelated reason, are both red (ADR-0144 D5's probe rule).
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
expect red "lint-scopes: an unreachable config entry is red" ls_probe "$work/probe-unreachable-api"

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

# ----------------------------------------------------------------- verdict
if [ "$fail" -ne 0 ]; then
    echo "check-scripts self-test FAILED: $fail of $((pass + fail)) cases"
    exit 1
fi
echo "check-scripts self-test OK ($pass cases: deny-list, panic-policy, run-sweep, shipping-features, sim-canaries, release-asserts, clock-ban, waker-atomics, fault-points, fsync-fail-stop, unsafe-roots, safety-inventory, file-length, line-width, lint-ratchet, doc-artifacts, parent-doc-gates, arith-spellings, lint-scopes each red on a planted violation)"
