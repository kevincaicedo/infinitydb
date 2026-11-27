//! The harness's marks on the simulated disk: `Node::step` marks the disk
//! before each cell's iteration, so every call the disk logs names the cell
//! that made it and the injected time of its step. An oracle that spaces
//! checkpoints or matches attempts reads these entries, never the cell's
//! own counters.

use std::path::Path;

use inf_foundation::CellId;
use inf_foundation::time::Clock;
use inf_log::fs::sim::{CallResult, DiskCall, DiskPhase, LoggedCall};

use super::MiniClient;
use super::park::{Quiet, STEPS_MAX};

/// The cell whose shard directory holds `path` (`…/shard-N/…`).
fn shard_of(path: &Path) -> Option<u16> {
    path.components()
        .find_map(|part| part.as_os_str().to_str()?.strip_prefix("shard-")?.parse().ok())
}

/// Sends `argv` on `client` and steps the node until its reply.
fn call(quiet: &mut Quiet, client: &mut MiniClient, argv: &[&[u8]]) -> Vec<u8> {
    client.send(&mut quiet.node, argv);
    for _ in 0..STEPS_MAX {
        quiet.step();
        if let Some(reply) = client.recv(&mut quiet.node) {
            return reply;
        }
    }
    panic!("no reply to {argv:?} in {STEPS_MAX} steps");
}

/// The position in `calls` of the first call of `cell` that `wanted` accepts.
fn first_of(calls: &[LoggedCall], cell: u16, wanted: impl Fn(&LoggedCall) -> bool) -> usize {
    calls
        .iter()
        .position(|call| shard_of(&call.path) == Some(cell) && wanted(call))
        .unwrap_or_else(|| panic!("cell {cell}: no such call in {calls:#?}"))
}

/// `INF.CKPT WAIT` on a two-cell node: each cell's staging create and its
/// `MANIFEST` rename are logged, the create first, and every call the
/// checkpoint made carries the mark of the cell whose shard it names, at an
/// injected time inside the steps the test ran.
#[test]
fn each_checkpoint_call_carries_the_mark_of_the_cell_that_made_it() {
    let mut quiet = Quiet::boot(2, 0, 0);
    let first = quiet.node.disk.logged_calls(0).len();
    let begun = quiet.clock.now();
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    let reply = call(&mut quiet, &mut client, &[b"INF.CKPT", b"WAIT"]);
    assert_eq!(reply, b"+OK\r\n", "INF.CKPT WAIT");
    let ended = quiet.clock.now();
    let calls = quiet.node.disk.logged_calls(first);
    for cell in 0..2 {
        let created = first_of(&calls, cell, |call| {
            call.call == DiskCall::CheckpointCreate { left_file: true }
                && call.result == CallResult::Ok
        });
        let renamed = first_of(&calls, cell, |call| {
            call.call == DiskCall::ManifestRename && call.result == CallResult::Ok
        });
        assert!(created < renamed, "cell {cell}: the create precedes the rename: {calls:#?}");
    }
    for call in &calls {
        let mark = call.mark.unwrap_or_else(|| panic!("an unmarked call: {call:?}"));
        let cell = shard_of(&call.path).unwrap_or_else(|| panic!("no shard: {call:?}"));
        assert_eq!(mark.phase, DiskPhase::Cell(CellId(cell)), "made by its own cell: {call:?}");
        assert!(begun <= mark.at && mark.at <= ended, "at a step the test ran: {call:?}");
    }
    assert_eq!(quiet.node.disk.calls_unlogged(), 0);
}

/// `ckpt_create_fail` once, then `INF.CKPT WAIT` on a one-cell node: the
/// refused staging create is logged under the cell's mark with no file left,
/// the walk aborts and hands the manual request back, and the retry that
/// serves it is the next logged create, at a later step; it makes the file,
/// the `MANIFEST` rename follows, and the `WAIT` answers. Each attempt is a
/// logged create. The retry here is the manual request's; the automatic
/// trigger's is read by the crash matrix's `ckpt_create_fail` row.
#[test]
fn a_refused_create_aborts_the_walk_and_its_retry_is_the_next_logged_create() {
    use inf_foundation::fault::{self, FaultSpec};
    let point = inf_log::fault::CKPT_CREATE_FAIL;
    let mut quiet = Quiet::boot(1, 0, 0);
    let first = quiet.node.disk.logged_calls(0).len();
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    fault::arm(inf_log::fault::CKPT_CREATE_FAIL, FaultSpec::Nth(1));
    let reply = call(&mut quiet, &mut client, &[b"INF.CKPT", b"WAIT"]);
    let fired = fault::fired(point);
    fault::disarm_all();
    assert_eq!(reply, b"+OK\r\n", "the retry serves the manual request");
    assert_eq!(fired, 1, "engagement: one staging create refused");
    let (_, aborted) = quiet.node.plane(0).ckpt_stats_for_sim();
    assert_eq!(aborted, 1, "the refused create aborted its walk");
    let calls = quiet.node.disk.logged_calls(first);
    let refused =
        first_of(&calls, 0, |call| matches!(call.call, DiskCall::CheckpointCreate { .. }));
    let retried = refused + 1;
    let renamed = first_of(&calls, 0, |call| call.call == DiskCall::ManifestRename);
    let (refusal, retry) = (&calls[refused], &calls[retried]);
    assert_eq!(refusal.call, DiskCall::CheckpointCreate { left_file: false }, "{calls:#?}");
    assert_eq!(refusal.result, CallResult::Injected(point));
    assert_eq!(retry.call, DiskCall::CheckpointCreate { left_file: true }, "{calls:#?}");
    assert_eq!(retry.result, CallResult::Ok);
    assert!(retried < renamed, "the rename follows the retry: {calls:#?}");
    let (refused_at, retried_at) = (refusal.mark.map(|m| m.at), retry.mark.map(|m| m.at));
    assert!(refused_at < retried_at, "the retry comes at a later step: {calls:#?}");
    for call in &calls {
        let mark = call.mark.unwrap_or_else(|| panic!("an unmarked call: {call:?}"));
        assert_eq!(mark.phase, DiskPhase::Cell(CellId(0)), "{call:?}");
    }
}
