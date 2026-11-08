//! A namespace change `META` holds that a cell cannot apply stops the node
//! (ADR-0189 D4): a cell left on the old definition would serve it and
//! export it. Each fail-stop runs in a child process (this test binary
//! re-invoked on one ignored test), so its exit code is the process's own,
//! not a panic libtest caught; the parent then boots the data directory and
//! reads `META`'s state on every cell. Also the stop after the origin's
//! apply, with the node running on.

use std::process::{Command, Stdio};

use inf_foundation::fault::FaultSpec;

use super::*;

/// The environment variable that hands a child its data directory.
const CHILD_DIR: &str = "INF_NS_FAIL_STOP_DIR";

/// The durable namespace each scenario creates and drops.
const LEDGER: &[u8] = b"ledger";

/// The reply a cell gives `INF.NS USE ledger` once `ledger` is dropped.
const LEDGER_NOT_FOUND: &str = "-ERR namespace 'ledger' not found";

/// Runs the ignored test `child` of this binary as its own process on
/// `dir`; returns its exit code and its stderr.
fn run_child(child: &str, dir: &std::path::Path) -> (Option<i32>, String) {
    let exe = std::env::current_exe().expect("the test binary");
    let mut process = Command::new(exe)
        .args(["--exact", child, "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD_DIR, dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the child");
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = process.try_wait().expect("the child's status") {
            break status;
        }
        if Instant::now() > deadline {
            process.kill().ok();
            panic!("{child} did not exit within 120 s: no fail-stop happened");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    process.stderr.take().expect("piped").read_to_string(&mut stderr).expect("stderr");
    (status.code(), stderr)
}

/// A child's body: a two-cell durable node with `faults` armed on its
/// cells creates `ledger`, writes a key on each cell and drops it. The drop
/// stops the process before it answers, so a reply is a failure.
fn drop_child(faults: Vec<(&'static str, FaultSpec)>) {
    let dir = std::path::PathBuf::from(std::env::var(CHILD_DIR).expect("run by its parent test"));
    let node = Node::start_durable_with_faults(2, &dir, faults);
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", LEDGER, b"MODE", b"durable", b"FSYNC", b"always"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", LEDGER])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    for key in [&b"k:0"[..], b"k:1", b"k:2", b"k:3"] {
        c.write_all(&cmd(&[b"SET", key, b"v"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    let mut d = conn_on_cell(&node, 0);
    d.write_all(&cmd(&[b"INF.NS", b"DROP", LEDGER])).expect("write");
    let reply = read_line(&mut d);
    panic!("the DROP was answered {:?}: no fail-stop", String::from_utf8_lossy(&reply));
}

/// The parent: `child` exits with the durable fail-stop's code and a
/// stderr line naming `arm`, `META` lacks `ledger` and carries its
/// tombstone, and the boot serves that state on every cell.
fn drop_fail_stop(child: &str, arm: &str, point: &str, verdict: &str) {
    let dir = temp_data_dir(&format!("ns-fail-stop-{point}"));
    let (code, stderr) = run_child(child, &dir);
    assert_eq!(code, Some(inf_server::EXIT_DURABLE_FAILSTOP), "the child's stderr: {stderr}");
    assert!(
        stderr.contains(&format!("namespace DDL {arm} failed (fail-stop)")),
        "the stderr line names the arm {arm:?}: {stderr}"
    );
    let meta = inf_server::load_catalog(&dir).expect("META decodes").expect("META exists");
    assert!(!meta.entries.iter().any(|e| e.name == LEDGER), "META still names ledger: {meta:?}");
    assert_eq!(meta.dropped.len(), 1, "META carries ledger's tombstone: {meta:?}");
    let node = Node::start_durable(2, &dir);
    for cell in 0..node.cells {
        assert_eq!(drop_wait_use_on(&node, cell, LEDGER), LEDGER_NOT_FOUND, "cell {cell}");
    }
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    receipt::verified(point, verdict);
}

#[test]
#[ignore = "child body of a_drop_leg_a_peer_cannot_apply_stops_the_node"]
fn a_drop_leg_refused_child() {
    drop_child(vec![(inf_server::fault::NS_DROP_LEG_REFUSED, FaultSpec::Always)]);
}

/// Crash-matrix row `ns_drop_leg_refused`: the peer cannot apply its `DROP`
/// leg after the swap dropped the namespace, and stops the node.
#[test]
fn a_drop_leg_a_peer_cannot_apply_stops_the_node() {
    let child = "ns_fail_stop::a_drop_leg_refused_child";
    drop_fail_stop(child, "DROP leg apply", "ns_drop_leg_refused", "drop-leg-fail-stop");
}

#[test]
#[ignore = "child body of a_drop_leg_read_as_refused_stops_the_node"]
fn a_drop_leg_read_refused_child() {
    drop_child(vec![(inf_server::fault::NS_LEG_READ_REFUSED, FaultSpec::Always)]);
}

/// Crash-matrix row `ns_leg_read_refused`: the origin reads its applied
/// `DROP` leg as refused, and stops the node.
#[test]
fn a_drop_leg_read_as_refused_stops_the_node() {
    let child = "ns_fail_stop::a_drop_leg_read_refused_child";
    drop_fail_stop(child, "DROP leg read", "ns_leg_read_refused", "leg-read-fail-stop");
}

/// Crash-matrix row `ns_drop_after_apply` (ADR-0189 D5): the `DROP` stops
/// after the origin's apply and before its first leg, and the node runs
/// on. `META` lacks the namespace and carries its tombstone, the origin no
/// longer serves it while its peer still does, the drop's notice was
/// posted (the next DDL answers), and the tombstone stays unstamped while
/// the node runs. A restart re-stamps it and serves the namespace on no
/// cell.
#[test]
fn a_drop_stopped_after_its_origin_applied_is_gone_after_a_restart() {
    let dir = temp_data_dir("ns-drop-after-apply");
    let fault = (inf_server::fault::NS_DROP_AFTER_APPLY, FaultSpec::Nth(1));
    let node = Node::start_durable_with_faults(2, &dir, vec![fault]);
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", LEDGER, b"MODE", b"durable"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"DROP", LEDGER])).expect("write");
    read_exactly(&mut c, b"-ERR fault: ns_drop_after_apply\r\n");
    assert_eq!(drop_wait_use_on(&node, 0, LEDGER), LEDGER_NOT_FOUND, "the origin applied");
    assert_eq!(drop_wait_use_on(&node, 1, LEDGER), "+OK", "the peer got no leg");
    let meta = inf_server::load_catalog(&dir).expect("META decodes").expect("META exists");
    assert!(!meta.entries.iter().any(|e| e.name == LEDGER), "META still names ledger");
    assert_eq!(meta.dropped.len(), 1, "META carries the tombstone");
    // Every cell publishes a checkpoint and a DDL is accepted: an unstamped
    // tombstone does not retire, and the notice was posted (no DDL pends).
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"next", b"MODE", b"memory"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let text = wait_persistence_field(&mut c, "ns_drop_tombstones", "1");
    assert!(text.contains("ns_drop_tombstones:1\r\n"), "the unstamped tombstone held: {text}");
    drop(c);
    node.stop();
    let node = Node::start_durable(2, &dir);
    for cell in 0..node.cells {
        assert_eq!(drop_wait_use_on(&node, cell, LEDGER), LEDGER_NOT_FOUND, "cell {cell}");
    }
    // The boot re-stamped it: once every cell publishes past the stamp,
    // the next accepted request retires it.
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"after", b"MODE", b"memory"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let text = wait_persistence_field(&mut c, "ns_drop_tombstones", "0");
    assert!(text.contains("ns_drop_tombstones:0\r\n"), "the re-stamped tombstone retired: {text}");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    receipt::verified("ns_drop_after_apply", "namespace-gone-after-restart");
}
