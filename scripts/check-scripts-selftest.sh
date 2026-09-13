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
root=$(fsync_fixture fs-clean <<'EOF'
pub fn ok() -> u64 { 1 }
EOF
)
expect green "fsync: the declarations alone, each marked, are green" env INF_CHECK_ROOT="$root" $FSYNC
expect_output "fsync: the scope line discloses the derived pattern set" "derived fsync-error patterns" env INF_CHECK_ROOT="$root" $FSYNC

root=$(fsync_fixture fs-catch <<'EOF'
pub fn swallow(r: Result<(), LogError>) {
    if let Err(LogError::Fsync(_)) = r {}
}
EOF
)
expect red "fsync: a catch-and-continue anywhere is red (there is no file allow-list)" env INF_CHECK_ROOT="$root" $FSYNC

root=$(fsync_fixture fs-discard <<'EOF'
pub fn seal(file: &mut std::fs::File) {
    let _ = file.sync_data();
}
EOF
)
expect red "fsync: a discarded raw sync_data is red with no named type involved" env INF_CHECK_ROOT="$root" $FSYNC
root=$(fsync_fixture fs-discard-ok <<'EOF'
pub fn seal(file: &mut std::fs::File) -> std::io::Result<()> {
    file.sync_data()?;
    Ok(())
}
EOF
)
expect green "fsync: a propagated sync_data is green" env INF_CHECK_ROOT="$root" $FSYNC

root=$(fsync_fixture fs-newtype <<'EOF'
pub enum CkptWriteFailure {
    Write(std::io::Error),
    Fsync(std::io::Error),
}
pub fn barrier(r: Result<(), CkptWriteFailure>) {
    if let Err(CkptWriteFailure::Fsync(_)) = r {}
}
EOF
)
expect red "fsync: a brand-new fsync error type is derived and gated" env INF_CHECK_ROOT="$root" $FSYNC

root=$(fsync_fixture fs-bare <<'EOF'
// fsync-fail-stop-allow:
pub fn swallow(r: Result<(), LogError>) {
    if let Err(LogError::Fsync(_)) = r {}
}
EOF
)
expect red "fsync: a bare marker (no reason) does not audit a site" env INF_CHECK_ROOT="$root" $FSYNC

root=$(fsync_fixture fs-stale <<'EOF'
// fsync-fail-stop-allow: guards nothing
pub fn ok() -> u64 { 1 }
EOF
)
expect red "fsync: a marker guarding no site is stale scope" env INF_CHECK_ROOT="$root" $FSYNC

root=$(fsync_fixture fs-prose <<'EOF'
//! LogError::Fsync is non-recoverable by contract (§8.4).
/// Returns TierFlushError::Fsync on a failed barrier.
pub fn ok() -> u64 { 1 }
EOF
)
expect green "fsync: prose naming the contract is not a site" env INF_CHECK_ROOT="$root" $FSYNC

root=$(fsync_fixture fs-testmod <<'EOF'
pub fn ok() -> u64 { 1 }

#[cfg(test)]
mod tests {
    fn t(r: Result<(), LogError>) { if let Err(LogError::Fsync(_)) = r {} }
}
EOF
)
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

# ------------------------------------------- doc-read profile (D13)
# ADR-0106 fourth amendment (F-L20-08): the parser-symbol profile gate's
# verdict runs on a planted flat report through INF_PROFILE_REPORT — an
# empty report, too few rows or samples, a missing positive control, and a
# banned symbol at 0.01% (below perf's old 0.05% floor) are each red; the
# sanctioned shape is green and names its positive control.
PROFILE=./scripts/check-doc-read-profile.sh
profile_report() {  # <path> <samples> <rows> <expected: yes|no> <banned: yes|no>
    local path=$1 samples=$2 rows=$3 expected=$4 banned=$5
    {
        printf '# To display the perf.data header info, please use --header/--header-only options.\n#\n'
        printf '# Samples: %s of event '"'"'cpu_core/cycles/P'"'"'\n# Event count (approx.): 109049269736\n#\n' "$samples"
        printf '# Overhead  Command  Shared Object  Symbol\n# ........  .......  .............  ......\n#\n'
        if [ "$expected" = yes ]; then
            printf '     4.65%%  cell-0  infinityd  [.] <inf_doc::tape::ObjIter as core::iter::traits::iterator::Iterator>::next\n'
            printf '     1.74%%  cell-0  infinityd  [.] inf_doc::tape::read_value\n'
            printf '     0.29%%  cell-0  infinityd  [.] inf_store::doc::<impl inf_store::store::CellStore>::json_get\n'
        fi
        if [ "$banned" = yes ]; then
            printf '     0.01%%  cell-2  infinityd  [.] inf_doc::json::JsonParser::parse_into\n'
        fi
        local i=0
        while [ "$i" -lt "$rows" ]; do
            printf '     0.02%%  cell-1  infinityd  [.] inf_store::store::CellStore::lookup_%d\n' "$i"
            i=$((i + 1))
        done
    } >"$path"
}
: >"$work/profile-empty.txt"
profile_report "$work/profile-ok.txt" 68K 250 yes no
profile_report "$work/profile-few-rows.txt" 68K 5 yes no
profile_report "$work/profile-few-samples.txt" 900 250 yes no
profile_report "$work/profile-no-control.txt" 68K 250 no no
profile_report "$work/profile-banned.txt" 68K 250 yes yes
expect red "doc-read-profile: an empty report is not a measurement" env INF_PROFILE_REPORT="$work/profile-empty.txt" $PROFILE "$work/profile-out"
expect red "doc-read-profile: too few symbol rows" env INF_PROFILE_REPORT="$work/profile-few-rows.txt" $PROFILE "$work/profile-out"
expect red "doc-read-profile: too few samples" env INF_PROFILE_REPORT="$work/profile-few-samples.txt" $PROFILE "$work/profile-out"
expect red "doc-read-profile: positive control missing" env INF_PROFILE_REPORT="$work/profile-no-control.txt" $PROFILE "$work/profile-out"
expect red "doc-read-profile: a parser symbol at 0.01% is seen" env INF_PROFILE_REPORT="$work/profile-banned.txt" $PROFILE "$work/profile-out"
expect green "doc-read-profile: the sanctioned shape" env INF_PROFILE_REPORT="$work/profile-ok.txt" $PROFILE "$work/profile-out"
expect_output "doc-read-profile: the pass names its positive control" "positive control present" env INF_PROFILE_REPORT="$work/profile-ok.txt" $PROFILE "$work/profile-out"
expect_output "doc-read-profile: the pass names the flat floor" "percent-limit 0" env INF_PROFILE_REPORT="$work/profile-ok.txt" $PROFILE "$work/profile-out"

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

# ----------------------------------------------------------------- verdict
if [ "$fail" -ne 0 ]; then
    echo "check-scripts self-test FAILED: $fail of $((pass + fail)) cases"
    exit 1
fi
echo "check-scripts self-test OK ($pass cases: deny-list, panic-policy, run-sweep, shipping-features, release-asserts, clock-ban, waker-atomics, fault-points, fsync-fail-stop, doc-read-profile, unsafe-roots, safety-inventory each red on a planted violation)"
