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

root=$(manifest ship-clean <<'EOF'
[package]
name = "fake"

[dependencies]
inf-foundation = { workspace = true }

[dev-dependencies]
inf-foundation = { workspace = true, features = ["fault-points", "collision-oracle"] }
EOF
)
expect green "shipping: dev-dependency edge may request the features" env INF_CHECK_ROOT="$root" $SHIP
expect_output "shipping: scope line discloses the scan" "1 manifests scanned" env INF_CHECK_ROOT="$root" $SHIP

mkdir -p "$work/ship-empty/crates"
expect red "shipping: no manifests at all is a failure, not OK" env INF_CHECK_ROOT="$work/ship-empty" $SHIP

root=$(manifest ship-normal <<'EOF'
[package]
name = "fake"

[dependencies]
inf-foundation = { workspace = true, features = ["collision-oracle", "fault-points"] }
EOF
)
expect red "shipping: the F-L16-01 shape — a normal edge requests the features" env INF_CHECK_ROOT="$root" $SHIP

root=$(manifest ship-table <<'EOF'
[package]
name = "fake"

[dependencies.inf-foundation]
workspace = true
features = ["fault-points"]
EOF
)
expect red "shipping: a [dependencies.NAME] table requesting the feature" env INF_CHECK_ROOT="$root" $SHIP

root=$(manifest ship-target <<'EOF'
[package]
name = "fake"

[target.'cfg(unix)'.dependencies]
inf-foundation = { workspace = true, features = ["fault-points"] }
EOF
)
expect red "shipping: a target-cfg dependency edge is a normal edge" env INF_CHECK_ROOT="$root" $SHIP

root=$(manifest ship-default <<'EOF'
[package]
name = "fake"

[features]
default = ["sim"]
sim = ["dst"]
dst = ["inf-foundation/fault-points"]

[dependencies]
inf-foundation = { workspace = true }
EOF
)
expect red "shipping: default reaching a forwarder (transitively)" env INF_CHECK_ROOT="$root" $SHIP

root=$(manifest ship-forwarder <<'EOF'
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
)
expect green "shipping: a non-default forwarder feature (inf-sim's dst shape)" env INF_CHECK_ROOT="$root" $SHIP
expect_output "shipping: forwarders are counted" "1 forwarder feature(s)" env INF_CHECK_ROOT="$root" $SHIP

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

root=$(fixture ra-clean <<'EOF'
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
)
inventory "$root" <<'EOF'
# fixture
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
I	1	crates/fake/src/lib.rs	expect	just built	built two lines up
EOF
expect green "release-asserts: matching inventory" env INF_CHECK_ROOT="$root" $RELEASE
expect_output "release-asserts: scope line discloses sites and classes" "2 release-panic sites in 2 identities" env INF_CHECK_ROOT="$root" $RELEASE

root=$(fixture ra-missing <<'EOF'
pub fn ok(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
EOF
)
expect red "release-asserts: no inventory file is a scope failure" env INF_CHECK_ROOT="$root" $RELEASE

root=$(fixture ra-new <<'EOF'
pub fn ok(n: u64) -> u64 {
    assert!(n > 0, "n is positive");
    assert!(n < 10, "n is small");
    n
}
EOF
)
inventory "$root" <<'EOF'
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
EOF
expect red "release-asserts: a new site is unclassified" env INF_CHECK_ROOT="$root" $RELEASE

root=$(fixture ra-stale <<'EOF'
pub fn ok(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
EOF
)
inventory "$root" <<'EOF'
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
I	1	crates/fake/src/lib.rs	expect	gone	vanished
EOF
expect red "release-asserts: a stale row is red" env INF_CHECK_ROOT="$root" $RELEASE

root=$(fixture ra-count <<'EOF'
pub fn a(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
pub fn b(n: u64) -> u64 { assert!(n > 0, "n is positive"); n }
EOF
)
inventory "$root" <<'EOF'
I	1	crates/fake/src/lib.rs	assert	n is positive	own argument check
EOF
expect red "release-asserts: a second site behind one identity is a count mismatch" env INF_CHECK_ROOT="$root" $RELEASE

# ADR-0107 D2, first amendment (batch 12): a C row's proof pointer must
# RESOLVE — a definition in the named file's production code, by
# rust-symbol-defined.awk over the stripped file. The fixture defines a
# free fn, a `Type::method` inside a multi-line generic `impl Trait for`,
# a const, and a test-only fn that must not count.
root=$(fixture ra-caller <<'EOF'
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
)
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	trust me
EOF
expect red "release-asserts: a C row without a proof pointer is red" env INF_CHECK_ROOT="$root" $RELEASE
inventory "$root" <<'EOF'
C	1	crates/fake/src/lib.rs	assert	caller validated the length	`crates/fake/src/lib.rs:check_bounds` at every write entry
EOF
expect green "release-asserts: a C row citing a resolving free fn is accepted" env INF_CHECK_ROOT="$root" $RELEASE
expect_output "release-asserts: resolved pointers are counted" "1 proof pointers resolved" env INF_CHECK_ROOT="$root" $RELEASE
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

root=$(fixture ra-debug <<'EOF'
pub fn ok(n: u64) -> u64 { debug_assert!(n > 0, "debug only"); n }
EOF
)
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
root=$(clock_fixture clock-clean <<'EOF'
pub fn ok() -> u64 { 1 }
EOF
)
expect green "clock-ban: clean crate under the real config" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
expect_output "clock-ban: scope line discloses config, scan and the skipped probe" "config 9/9 entries, 0 shadow configs, 1 cell crates / 1 files scanned, 0 allowed sites in cell code; probe: skipped (fixture mode)" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
[ -n "$root" ] && rm -f "$root/clippy.toml"
expect red "clock-ban: no clippy.toml is red" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
cp clippy.toml "$root/clippy.toml"
sed -i.bak '/std::time::SystemTime::elapsed/d' "$root/clippy.toml"
expect red "clock-ban: a deleted config entry is red" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
cp clippy.toml "$root/clippy.toml"
printf 'disallowed-methods = []\n' >"$root/crates/fake/clippy.toml"
expect red "clock-ban: a shadow clippy.toml in a crate directory is red" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
[ -n "$root" ] && rm -f "$root/crates/fake/clippy.toml"

for snippet in \
    '#![allow(clippy::disallowed_methods)] pub fn t() {}' \
    '#![allow(clippy::style)] pub fn t() {}' \
    '#![allow(clippy::all)] pub fn t() {}' \
    '#![allow(warnings)] pub fn t() {}' \
    '#![expect(clippy::disallowed_methods)] pub fn t() {}' \
    '#[allow(clippy::all)] pub fn t() {}' \
    '#[allow(clippy::style)] pub fn t() {}' \
    '#[allow(clippy::disallowed_methods)] pub fn t() {}' \
    '#[expect(clippy::disallowed_methods)] pub fn t() {}'
do
    root=$(clock_fixture clock-planted <<<"$snippet")
    expect red "clock-ban: planted '$snippet'" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
done
# The multi-line shape rustfmt produces, without a reason.
root=$(clock_fixture clock-multiline-bare <<'EOF'
#[allow(
    clippy::disallowed_methods
)]
pub fn t() {}
EOF
)
expect red "clock-ban: a multi-line allow without a reason is red" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
# Sanctioned shapes: a per-site allow with a reason, one-line and rustfmt's.
root=$(clock_fixture clock-sanctioned <<'EOF'
#[allow(clippy::disallowed_methods, reason = "control thread: boot narration")]
pub fn t() {}
#[allow(
    clippy::disallowed_methods,
    reason = "the injected clock's origin"
)]
pub fn u() {}
EOF
)
expect green "clock-ban: per-site allows with reasons are green" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
expect_output "clock-ban: the one-line site is listed with its reason" "allowed crates/fake/src/lib.rs:1: control thread: boot narration" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
expect_output "clock-ban: the multi-line site is listed with its reason" "allowed crates/fake/src/lib.rs:3: the injected clock's origin" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
expect_output "clock-ban: the scope line counts both" "2 allowed sites in cell code" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
# An allow inside a test-only module is not cell code.
root=$(clock_fixture clock-testmod <<'EOF'
pub fn ok() {}

#[cfg(test)]
mod tests {
    #[allow(clippy::disallowed_methods)]
    fn scratch() -> u128 { 0 }
}
EOF
)
expect green "clock-ban: a bare allow inside a test-only module is stripped" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK
expect_output "clock-ban: the stripped module counts no site" "0 allowed sites in cell code" env INF_CHECK_ROOT="$root" INF_CLOCK_BAN_PROBE=off $CLOCK

# ----------------------------------------------------------------- verdict
if [ "$fail" -ne 0 ]; then
    echo "check-scripts self-test FAILED: $fail of $((pass + fail)) cases"
    exit 1
fi
echo "check-scripts self-test OK ($pass cases: deny-list, panic-policy, run-sweep, shipping-features, release-asserts, clock-ban each red on a planted violation)"
